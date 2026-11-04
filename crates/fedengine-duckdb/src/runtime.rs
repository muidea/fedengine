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
    pub(super) storage: Option<ServiceHandle<ManagementStorage>>,
    pub(super) hub: modorch::EventHub,
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
            interrupt_until_finished(monitor_handle, done).await;
            Ok(())
        }).map_err(framework_error)?;
        // Cancellation must keep observing a worker until its captures actually
        // leave scope, including submission failure and panic unwinding.
        let finished_guard = finished.drop_guard();
        let ddl_source = request.ddl_source.clone();
        let deferred = self.storage.is_some() && ddl_source.is_some() && !prepare;
        let (completion_tx, completion_rx) = oneshot::channel();
        let completion_tx = deferred.then_some(completion_tx);
        let completion_sender = tx.clone();
        let submitted=self.tasks.spawn_blocking_io(move |_|{
            let _finished=finished_guard;
            let _span = tracing::info_span!("execution", %operation_id, %fingerprint, operation=?kind, principal=%request.principal, session_id=request.session_id, sources=?request.generations.keys().collect::<Vec<_>>()).entered();
            let _worker=GaugeGuard::enter(metrics.active_workers.clone());
            let started = Instant::now();
            tracing::info!(prepare_only=prepare,"execution running");
            let _owned=(ownership,permit);
            let mut metadata=Some(meta_tx);
            let result=(||->Result<()>{
                if request.cancel.is_cancelled(){return Err(EngineError::new("57014","query canceled before start"));}
                let conn=root.lock().unwrap().try_clone().map_err(engine_error)?;
                conn.execute_batch(fedengine_pg_compat::EXECUTION_SETTINGS).map_err(engine_error)?;
                *interrupt.lock().unwrap()=Some(conn.interrupt_handle());
                if request.cancel.is_cancelled(){return Err(EngineError::new("57014","query canceled before execution"));}
                if request.metadata.is_some(){conn.execute_batch("SET lambda_syntax='ENABLE_SINGLE_ARROW'; BEGIN TRANSACTION").map_err(engine_error)?;}
                let result=run(&conn,&request,&config,&registry,WorkerOutput{sender:&tx,metadata:&mut metadata,completion:completion_tx},&metrics);
                if request.metadata.is_some(){let _=conn.execute_batch("ROLLBACK");}
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
            Ok(())
        });
        let job = match submitted {
            Ok(job) => job,
            Err(error) => {
                cancel.cancel();
                return Err(framework_error(error));
            }
        };
        if let Some(storage) = &self.storage
            && let Some(source) = ddl_source
            && !prepare
        {
            let hub = self.hub.clone();
            let registry = self.registry.acquire().map_err(framework_error)?;
            let storage = storage.acquire().map_err(framework_error)?;
            let static_mode = !self.config.management.enabled;
            let timeout = Duration::from_millis(self.config.management.request_timeout_ms);
            self.tasks.spawn(move |_| async move {
                // wait_finished, rather than wait, guarantees lease/fence release.
                job.wait_finished().await?;
                let confirmed = completion_rx.await.ok();
                let needs_refresh = static_mode || registry.get(&source).is_ok_and(|s| s.stale.load(Ordering::SeqCst));
                let fresh = !needs_refresh || crate::ddl_refresh::refresh(&hub, &storage, &registry, &source, timeout.min(deadline.saturating_duration_since(Instant::now()))).await;
                if let Some(mut completion) = confirmed {
                    if !fresh {
                        completion.notices.push((uuid::Uuid::new_v4().to_string(), "DDL committed; metadata refresh failed or is still pending. Inspect the source management status before issuing further statements; do not retry the DDL.".into()));
                    }
                    let _ = completion_sender.send(ResultChunk::Complete(completion)).await;
                }
                Ok(())
            }).map_err(framework_error)?;
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
                namespace_version: 0,
                database_version: None,
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

#[async_trait]
impl RuntimeControlPort for DuckdbRuntime {
    async fn control(
        &self,
        action: ControlAction,
        ownership: Box<dyn Send>,
    ) -> Result<ControlOutcome> {
        let root = self.root.clone();
        let config = self.config.clone();
        // Control work has independent scheduler quota; it never waits for query slots.
        self.tasks
            .spawn_blocking_io(move |_| {
                let _ownership = ownership;
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                Ok(crate::control::execute(&conn, &config, action))
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
}

/// DuckDB clears the interrupt flag at the start of a native statement. An
/// execution has setup/describe/stream/cleanup statements, so one interrupt can
/// be consumed before the expensive statement begins. Keep cancellation armed
/// until the actual worker exits; never infer that a write rolled back.
async fn interrupt_until_finished(
    handle: Arc<Mutex<Option<Arc<InterruptHandle>>>>,
    done: CancellationToken,
) {
    loop {
        if done.is_cancelled() {
            return;
        }
        let current = handle.lock().unwrap().clone();
        if let Some(current) = current {
            current.interrupt();
        }
        tokio::select! {biased;_=done.cancelled()=>return, _=tokio::time::sleep(Duration::from_millis(20))=>{}}
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_survives_native_statement_reset_and_drains_monitor() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("SET threads=1").unwrap();
        let interrupt = conn.interrupt_handle();
        interrupt.interrupt();
        assert_eq!(
            conn.query_row("SELECT 1", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            1,
            "a new native statement resets an earlier interrupt"
        );
        let handle = Arc::new(Mutex::new(Some(interrupt.clone())));
        let done = CancellationToken::new();
        let monitor = tokio::spawn(interrupt_until_finished(handle, done.clone()));
        // Allow the initial interrupt to precede the next native statement,
        // exactly the setup -> describe/execute race of a canceled worker.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let exit = done.drop_guard();
        let mut worker = tokio::task::spawn_blocking(move || {
            let _exit = exit;
            conn.query_row(
                "SELECT sum(a.i*b.i) FROM range(1000000000) a(i), range(1000000000) b(i)",
                [],
                |r| r.get::<_, i64>(0),
            )
        });
        let result = tokio::time::timeout(Duration::from_secs(3), &mut worker).await;
        if result.is_err() {
            // Clean up even when this assertion regresses, instead of leaving
            // an unkillable blocking test task at runtime shutdown.
            interrupt.interrupt();
            let _ = worker.await;
            monitor.await.unwrap();
            panic!("cancellation was lost when the native statement reset its interrupt flag");
        }
        let error = result.unwrap().unwrap().unwrap_err();
        assert_eq!(engine_error(error).sqlstate, "57014");
        tokio::time::timeout(Duration::from_secs(1), monitor)
            .await
            .unwrap()
            .unwrap();
    }
}
