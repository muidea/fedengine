use crate::{
    codec::{cell, pg_type, to_duckdb},
    error::engine_error,
    metadata::describe,
};
use duckdb::{Connection, params_from_iter};
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
pub(super) struct WorkerOutput<'a> {
    pub(super) sender: &'a mpsc::Sender<ResultChunk>,
    pub(super) metadata: &'a mut Option<oneshot::Sender<Result<Metadata>>>,
}
pub(super) struct Metadata {
    pub(super) columns: Vec<ColumnMeta>,
    pub(super) parameters: Vec<PgType>,
}
pub(super) fn run(
    conn: &Connection,
    r: &ExecuteRequest,
    c: &Config,
    registry: &DataSourceRegistry,
    output: WorkerOutput<'_>,
    metrics: &EngineMetrics,
) -> Result<()> {
    let tx = output.sender;
    let metadata = output.metadata;
    let mut stmt = conn.prepare(&r.sql).map_err(engine_error)?;
    let count = stmt.parameter_count();
    if count != r.parameter_types.len() && r.prepare_only {
        return Err(EngineError::new(
            "42P18",
            "parameter types require explicit OIDs or SQL casts",
        ));
    }
    let types: Vec<PgType> = r
        .parameter_types
        .iter()
        .map(|p| {
            p.ok_or_else(|| {
                EngineError::new(
                    "42P18",
                    "parameter type cannot be determined; add an explicit SQL cast",
                )
            })
        })
        .collect::<Result<_>>()?;
    if r.prepare_only {
        let columns = if r.kind == StatementKind::Query {
            describe(conn, &r.sql, count)?
        } else {
            vec![]
        };
        let _ = metadata.take().unwrap().send(Ok(Metadata {
            columns,
            parameters: types,
        }));
        return Ok(());
    }
    if count != r.parameters.len() {
        return Err(EngineError::new("08P01", "parameter count mismatch"));
    }
    if r.kind != StatementKind::Query {
        let _ = metadata.take().unwrap().send(Ok(Metadata {
            columns: vec![],
            parameters: types,
        }));
        if r.cancel.is_cancelled() {
            return Err(EngineError::new("57014", "write canceled before execute"));
        }
        let values = r.parameters.iter().map(to_duckdb).collect::<Vec<_>>();
        // MySQL's native UPDATE/DELETE expose the correct Count result while
        // duckdb_rows_changed reports zero. Consume that single result, once.
        let n = if r.kind == StatementKind::Ddl {
            stmt.execute(params_from_iter(values.iter()))
                .map_err(|error| engine_error(error).unknown())? as u64
        } else {
            let count: i64 = stmt
                .query_row(params_from_iter(values.iter()), |row| row.get(0))
                .map_err(|error| engine_error(error).unknown())?;
            u64::try_from(count)
                .map_err(|_| EngineError::new("XX000", "invalid affected row count").unknown())?
        };
        if let Some(source) = &r.ddl_source {
            metrics.ddl_confirmed.fetch_add(1, Ordering::Relaxed);
            registry.invalidate(source, false)?;
            let s = registry.get(source)?;
            let refresh = match s.definition.kind {
                DataSourceKind::Mysql => conn.execute_batch("CALL mysql_clear_cache()"),
                DataSourceKind::Postgres => conn.execute_batch("CALL postgres_clear_cache()"),
            };
            if refresh.is_err() {
                s.stale.store(true, Ordering::SeqCst);
                metrics.metadata_stale.fetch_add(1, Ordering::Relaxed);
                tracing::error!(source=?source,"DDL committed; metadata refresh failed; source marked stale");
            }
        }
        tracing::info!(operation=?r.kind,affected_rows=n,"commit confirmed");
        send_chunk(
            tx,
            ResultChunk::Complete(Completion {
                command: r.command.clone(),
                affected_rows: if r.kind == StatementKind::Ddl {
                    None
                } else {
                    Some(n as u64)
                },
                outcome: ExecutionOutcome::CommitConfirmed,
            }),
            r,
        )
        .ok();
    } else {
        let values = r.parameters.iter().map(to_duckdb).collect::<Vec<_>>();
        let described_columns = describe(conn, &r.sql, count)?;
        let mut stream = stmt
            .stream_arrow(params_from_iter(values.iter()))
            .map_err(engine_error)?;
        let schema = stream.get_schema();
        let columns = schema
            .fields()
            .iter()
            .map(|f| {
                Ok(ColumnMeta {
                    name: f.name().clone(),
                    data_type: pg_type(f.data_type())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let columns = if described_columns.len() == columns.len() {
            described_columns
        } else {
            columns
        };
        let _ = metadata.take().unwrap().send(Ok(Metadata {
            columns,
            parameters: types,
        }));
        let mut total = 0;
        for batch in &mut stream {
            let batch = batch.map_err(engine_error)?;
            for start in (0..batch.num_rows()).step_by(c.duckdb.batch_rows) {
                if r.cancel.is_cancelled() {
                    return Err(EngineError::new("57014", "query canceled"));
                }
                let end = (start + c.duckdb.batch_rows).min(batch.num_rows());
                let mut rows = Vec::with_capacity(end - start);
                for row in start..end {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|a| cell(a.as_ref(), row))
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                total += rows.len() as u64;
                metrics
                    .rows_streamed
                    .fetch_add(rows.len() as u64, Ordering::Relaxed);
                send_chunk(tx, ResultChunk::Rows(rows), r)
                    .map_err(|_| EngineError::new("57014", "result receiver closed"))?;
            }
        }
        send_chunk(
            tx,
            ResultChunk::Complete(Completion {
                command: "SELECT".into(),
                affected_rows: Some(total),
                outcome: ExecutionOutcome::Completed,
            }),
            r,
        )
        .map_err(|_| EngineError::new("57014", "result receiver closed"))?;
    }
    // Source fences, connection and permits survive until the consumer drains/closes the stream.
    while !tx.is_closed() && !r.cancel.is_cancelled() && Instant::now() < r.deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
fn send_chunk(
    tx: &mpsc::Sender<ResultChunk>,
    mut chunk: ResultChunk,
    r: &ExecuteRequest,
) -> Result<()> {
    loop {
        if r.cancel.is_cancelled() || Instant::now() >= r.deadline {
            return Err(EngineError::new(
                "57014",
                "query canceled while waiting for result capacity",
            ));
        }
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(EngineError::new("57014", "result receiver closed"));
            }
            Err(mpsc::error::TrySendError::Full(value)) => {
                chunk = value;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
