use crate::{
    error::{engine_error, framework_error, fw},
    metadata::discover_indexes,
    worker::{Metadata, WorkerOutput, run},
};
use duckdb::{Connection, InterruptHandle};
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use modorch::prelude::*;
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
pub(super) struct DuckdbRuntime {
    pub(super) metrics: Arc<EngineMetrics>,
    pub(super) root: Arc<Mutex<Connection>>,
    pub(super) slots: Arc<Semaphore>,
    pub(super) tasks: TaskScheduler,
    pub(super) config: Arc<Config>,
    pub(super) registry: ServiceHandle<DataSourceRegistry>,
}
#[async_trait]
impl ExecutionPort for DuckdbRuntime {
    async fn submit(
        &self,
        request: ExecuteRequest,
        ownership: Box<dyn Send>,
    ) -> Result<RuntimeReply> {
        let wait = Duration::from_millis(self.config.duckdb.acquire_timeout_ms)
            .min(request.deadline.saturating_duration_since(Instant::now()));
        let waiting = GaugeGuard::enter(self.metrics.worker_waiters.clone());
        let permit = tokio::select! {
            _=request.cancel.cancelled()=>return Err(EngineError::new("57014","query canceled before worker admission")),
            p=tokio::time::timeout(wait,self.slots.clone().acquire_owned())=>p.map_err(|_|EngineError::new("53300","worker pool acquisition timed out"))?.map_err(|_|EngineError::new("57P01","engine stopped"))?
        };
        drop(waiting);
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        let metrics = self.metrics.clone();
        let root = self.root.clone();
        let config = self.config.clone();
        let registry = self.registry.acquire().map_err(framework_error)?;
        let (meta_tx, meta_rx) = oneshot::channel();
        let (tx, rx) = mpsc::channel(config.duckdb.result_channel_batches);
        let interrupt: Arc<Mutex<Option<Arc<InterruptHandle>>>> = Arc::new(Mutex::new(None));
        let monitor_handle = interrupt.clone();
        let cancel = request.cancel.clone();
        let token = cancel.clone();
        let finished = CancellationToken::new();
        let done = finished.clone();
        let deadline = request.deadline;
        let prepare = request.prepare_only;
        let sql = request.sql.clone();
        let generations = request.generations.clone();
        let kind = request.kind;
        let operation_id = uuid::Uuid::new_v4().to_string();
        let fingerprint = Sha256::digest(request.sql.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        self.tasks.spawn(move |shutdown|async move{
            tokio::select!{biased;_=done.cancelled()=>return Ok(()),_=token.cancelled()=>{},_=shutdown.cancelled()=>{},_=tokio::time::sleep_until(deadline.into())=>{}}
            token.cancel();
            if let Some(h)=monitor_handle.lock().unwrap().as_ref(){h.interrupt();}
            Ok(())
        }).map_err(framework_error)?;
        let submitted=self.tasks.spawn_blocking_io(move |_|{
            let _span = tracing::info_span!("execution", %operation_id, %fingerprint, operation=?kind, principal=%request.principal, session_id=request.session_id, sources=?request.generations.keys().collect::<Vec<_>>()).entered();
            let _worker=GaugeGuard::enter(metrics.active_workers.clone());
            let started = Instant::now();
            tracing::info!(prepare_only=prepare,"execution running");
            let _owned=(ownership,permit);
            let mut metadata=Some(meta_tx);
            let result=(||->Result<()>{
                if request.cancel.is_cancelled(){return Err(EngineError::new("57014","query canceled before start"));}
                let conn=root.lock().unwrap().try_clone().map_err(engine_error)?;
                *interrupt.lock().unwrap()=Some(conn.interrupt_handle());
                if request.cancel.is_cancelled(){return Err(EngineError::new("57014","query canceled before execution"));}
                let result=run(&conn,&request,&config,&registry,WorkerOutput{sender:&tx,metadata:&mut metadata},&metrics);
                *interrupt.lock().unwrap()=None;
                result
            })();
            if result.is_ok() { metrics.completed.fetch_add(1,Ordering::Relaxed); }
            if let Err(mut error)=result{
                metrics.failed.fetch_add(1,Ordering::Relaxed);
                if error.sqlstate=="57014" {metrics.canceled.fetch_add(1,Ordering::Relaxed);}
                if error.outcome_unknown {metrics.unknown_writes.fetch_add(1,Ordering::Relaxed);}
                if error.outcome_unknown {error.message=format!("{}; operation_id={operation_id}",error.message);}
                if let Some(source)=&request.ddl_source&& error.outcome_unknown {let _=registry.invalidate(source,true);metrics.metadata_stale.fetch_add(1,Ordering::Relaxed);}
                tracing::warn!(operation=?kind,sqlstate=error.sqlstate,outcome_unknown=error.outcome_unknown,"execution ended with error");
                if let Some(m)=metadata.take(){let _=m.send(Err(error));}else{let _=tx.try_send(ResultChunk::Error(error));}
            }
            metrics.execution_micros.fetch_add(started.elapsed().as_micros().min(u64::MAX as u128) as u64,Ordering::Relaxed);
            tracing::info!(elapsed_ms=started.elapsed().as_millis(),"worker exited");
            finished.cancel();Ok(())
        });
        if let Err(e) = submitted {
            cancel.cancel();
            return Err(framework_error(e));
        }
        // Scheduler owns the worker independently of the waiting caller.
        match meta_rx
            .await
            .map_err(|_| EngineError::new("XX000", "worker exited without metadata"))??
        {
            Metadata {
                columns,
                parameters,
            } if prepare => Ok(RuntimeReply::Prepared(PreparedDescriptor {
                sql,
                parameters,
                columns,
                generations,
                kind,
            })),
            Metadata { columns, .. } => Ok(RuntimeReply::Accepted(AcceptedExecution::new(
                columns,
                kind,
                ExecutionStream {
                    receiver: rx,
                    cancel,
                },
            ))),
        }
    }
    async fn indexes(
        &self,
        source: DataSourceId,
        ownership: Box<dyn Send>,
    ) -> Result<Vec<RemoteIndex>> {
        let state = self
            .registry
            .acquire()
            .map_err(framework_error)?
            .get(&source)?;
        let def = state.definition.clone();
        let root = self.root.clone();
        let slots = self.slots.clone();
        let waiting = GaugeGuard::enter(self.metrics.worker_waiters.clone());
        let metrics = self.metrics.clone();
        let permit = tokio::time::timeout(
            Duration::from_millis(self.config.duckdb.acquire_timeout_ms),
            slots.acquire_owned(),
        )
        .await
        .map_err(|_| EngineError::new("53300", "metadata worker acquisition timed out"))?
        .map_err(|_| EngineError::new("57P01", "engine stopped"))?;
        drop(waiting);
        self.tasks
            .spawn_blocking_io(move |_| {
                let _worker = GaugeGuard::enter(metrics.active_workers.clone());
                let _keep = (ownership, permit);
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                Ok(discover_indexes(&conn, &def))
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
}
