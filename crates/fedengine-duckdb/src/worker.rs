use crate::{
    codec::{cell, to_duckdb},
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
    pub(super) completion: Option<oneshot::Sender<Completion>>,
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
    if r.cancel.is_cancelled() {
        return Err(EngineError::new(
            "57014",
            "query canceled before preparing execution",
        ));
    }
    if let Some(metadata) = &r.metadata {
        for catalog in &metadata.scope.catalogs {
            let exists: i64 = conn
                .query_row(
                    "SELECT count(*) FROM _fedengine.metadata_projection_state WHERE snapshot_id=?",
                    [&catalog.snapshot_id],
                    |row| row.get(0),
                )
                .map_err(engine_error)?;
            if exists != 1 {
                return Err(EngineError::new(
                    "40001",
                    "metadata snapshot retired before execution; retry the query",
                ));
            }
        }
    }
    let tx = output.sender;
    let metadata = output.metadata;
    let overrides = r.metadata.as_ref().map(|m| m.columns.as_slice());
    let sql = fedengine_pg_compat::execution_sql(&r.sql)?;
    let mut stmt = conn.prepare(&sql).map_err(engine_error)?;
    if r.cancel.is_cancelled() {
        return Err(EngineError::new(
            "57014",
            "query canceled during preparation",
        ));
    }
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
            describe(conn, &sql, count, overrides)?
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
            if c.management.enabled {
                // Publish invalidation immediately. Refresh is coordinated after this worker
                // releases its source fence and instance read lease; DDL is never replayed.
                registry.invalidate(source, true)?;
            } else {
                registry.invalidate(source, false)?;
                let source = registry.get(source)?;
                if conn
                    .execute_batch(&format!(
                        "CALL {}_clear_cache()",
                        source.definition.kind.extension()
                    ))
                    .is_err()
                {
                    source.stale.store(true, Ordering::SeqCst);
                    metrics.metadata_stale.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        tracing::info!(operation=?r.kind,affected_rows=n,"commit confirmed");
        let completion = Completion {
            notices: vec![],
            command: r.command.clone(),
            affected_rows: if r.kind == StatementKind::Ddl {
                None
            } else {
                Some(n)
            },
            outcome: ExecutionOutcome::CommitConfirmed,
        };
        if let Some(deferred) = output.completion {
            // Release connection, source fences and instance lease before refresh.
            // The runtime publishes this completion after the publication barrier.
            let _ = deferred.send(completion);
            return Ok(());
        }
        send_chunk(tx, ResultChunk::Complete(completion), r).ok();
    } else {
        let values = r.parameters.iter().map(to_duckdb).collect::<Vec<_>>();
        let described_columns = describe(conn, &sql, count, overrides)?;
        if r.cancel.is_cancelled() {
            return Err(EngineError::new("57014", "query canceled before streaming"));
        }
        let columns = described_columns;
        let row_types: Vec<_> = columns.iter().map(|c| c.data_type).collect();
        let regclass_names = if row_types.contains(&PgType::Catalog(2205)) {
            r.metadata
                .as_ref()
                .map(|m| regclass_names(conn, m))
                .transpose()?
                .unwrap_or_default()
        } else {
            Default::default()
        };
        let mut stream = stmt
            .stream_arrow(params_from_iter(values.iter()))
            .map_err(engine_error)?;
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
                            .zip(&row_types)
                            .map(|(a, t)| {
                                let value = cell(a.as_ref(), row)
                                    .and_then(|c| crate::codec::coerce(c, *t))?;
                                Ok(if *t == PgType::Catalog(2205) {
                                    if let Cell::Oid(oid) = value {
                                        match regclass_names.get(&oid) {
                                            Some(name) => Cell::NamedOid {
                                                oid,
                                                name: name.clone(),
                                            },
                                            None => Cell::Oid(oid),
                                        }
                                    } else {
                                        value
                                    }
                                } else {
                                    value
                                })
                            })
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
                notices: vec![],
                command: r.command.clone(),
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
fn regclass_names(
    conn: &Connection,
    metadata: &MetadataExecution,
) -> Result<std::collections::BTreeMap<u32, String>> {
    use fedengine_pg_compat::{display_identifier, scoped_catalog};
    let sql = format!(
        "SELECT c.oid,c.relname,n.nspname FROM ({}) c JOIN ({}) n ON n.oid=c.relnamespace",
        scoped_catalog("pg_class", &metadata.scope)?,
        scoped_catalog("pg_namespace", &metadata.scope)?
    );
    let mut statement = conn.prepare(&sql).map_err(engine_error)?;
    let entries = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(engine_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(engine_error)?;
    let mut visible = std::collections::BTreeMap::<&str, (usize, &str)>::new();
    for (_, name, schema) in &entries {
        if let Some(rank) = metadata.search_path.iter().position(|s| s == schema) {
            let first = visible.entry(name).or_insert((rank, schema));
            if rank < first.0 {
                *first = (rank, schema);
            }
        }
    }
    let mut names = entries
        .iter()
        .map(|(oid, name, schema)| {
            let display = if visible
                .get(name.as_str())
                .is_some_and(|(_, s)| *s == schema)
            {
                display_identifier(name)
            } else {
                format!(
                    "{}.{}",
                    display_identifier(schema),
                    display_identifier(name)
                )
            };
            (*oid, display)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    names.insert(0, "-".into());
    Ok(names)
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
