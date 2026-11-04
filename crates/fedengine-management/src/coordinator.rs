use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use modorch::prelude::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Coordinator {
    pub config: Arc<Config>,
    pub tasks: TaskScheduler,
    pub storage: ServiceHandle<ManagementStorage>,
    pub registry: ServiceHandle<DataSourceRegistry>,
    pub control: ServiceHandle<RuntimeControl>,
    pub slots: Arc<Semaphore>,
    pub memory: Arc<Semaphore>,
}
fn engine(error: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("management resource unavailable: {error}"))
}
impl Coordinator {
    pub fn start(&self, id: String) -> FrameworkResult<()> {
        let coordinator = self.clone();
        // Acquire before scheduling, retaining dependencies even if shutdown starts next.
        let leases = (
            self.storage.acquire()?,
            self.registry.acquire()?,
            self.control.acquire()?,
        );
        self.tasks.spawn(move |shutdown| async move {
            let _leases = leases;
            if let Err(error) = coordinator.run(&id, shutdown).await {
                tracing::error!(operation_id=%id, sqlstate=error.sqlstate,"management task left durable state for recovery");
            }
            Ok(())
        })?;
        Ok(())
    }
    async fn phase(&self, id: &str, phase: &str, status: OperationStatus) -> Result<()> {
        let id = id.to_owned();
        let phase = phase.to_owned();
        self.storage
            .acquire()
            .map_err(engine)?
            .port
            .update(Box::new(move |state| {
                let operation = state
                    .operations
                    .get_mut(&id)
                    .ok_or_else(|| EngineError::new("42704", "operation disappeared"))?;
                operation.phase = phase;
                operation.status = status;
                operation.updated_at = chrono::Utc::now().to_rfc3339();
                Ok(())
            }))
            .await?;
        Ok(())
    }
    async fn abort_publication(
        &self,
        id: &str,
        source: &ManagedSource,
        is_apply: bool,
        error: EngineError,
        registry: &DataSourceRegistry,
    ) -> Result<()> {
        if is_apply {
            registry.close(&source.id);
        } else {
            let _ = registry.invalidate(&source.id, true);
        }
        let id = id.to_owned();
        let incarnation = source.incarnation.clone();
        self.storage
            .acquire()
            .map_err(engine)?
            .port
            .update(Box::new(move |state| {
                let source = state
                    .sources
                    .get_mut(&incarnation)
                    .ok_or_else(|| EngineError::new("42704", "source disappeared"))?;
                source.status = if is_apply {
                    SourceStatus::Degraded
                } else {
                    SourceStatus::MetadataStale
                };
                source.current_operation = None;
                source.last_error = Some(error.to_string());
                if let Some(c) = source
                    .catalog_snapshot
                    .as_ref()
                    .and_then(|id| state.catalogs.get_mut(id))
                {
                    c.stale = true;
                }
                let op = state.operations.get_mut(&id).unwrap();
                op.status = OperationStatus::Failed;
                op.phase = "PublicationAborted".into();
                op.error = Some(error.to_string());
                op.updated_at = chrono::Utc::now().to_rfc3339();
                Ok(())
            }))
            .await?;
        Ok(())
    }
    async fn run(&self, id: &str, shutdown: CancellationToken) -> Result<()> {
        let _slot = tokio::select! {
            permit = self.slots.clone().acquire_owned() => permit.map_err(|_| EngineError::new("57P01", "management stopped"))?,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let storage = self.storage.acquire().map_err(engine)?;
        let registry = self.registry.acquire().map_err(engine)?;
        let control = self.control.acquire().map_err(engine)?;
        let state = storage.port.read().await?;
        let operation = state
            .operations
            .get(id)
            .ok_or_else(|| EngineError::new("42704", "operation does not exist"))?
            .clone();
        let source = state.sources.get(&operation.incarnation).unwrap().clone();
        let desired = state
            .revision(&source.incarnation, operation.target_revision)?
            .clone();
        let previous = source
            .applied_revision
            .map(|r| state.revision(&source.incarnation, r).cloned())
            .transpose()?;
        let strict_refresh = matches!(operation.action, ManagementAction::Refresh { .. })
            && self
                .config
                .management
                .mysql_identity_policy
                .resets(source.kind)
            && source.applied_revision != Some(desired.revision);
        self.phase(id, "Running", OperationStatus::Running).await?;
        if matches!(
            operation.action,
            ManagementAction::Register { .. }
                | ManagementAction::Patch { .. }
                | ManagementAction::Schemas { .. }
                | ManagementAction::Delete { .. }
        ) {
            let id = id.to_owned();
            storage
                .port
                .update(Box::new(move |state| {
                    let operation = state.operations.get_mut(&id).unwrap();
                    if matches!(operation.action, ManagementAction::Delete { .. }) {
                        let source = state.sources.get_mut(&operation.incarnation).unwrap();
                        source.deleted = true;
                    }
                    operation.status = OperationStatus::Succeeded;
                    operation.phase = "Complete".into();
                    operation.updated_at = chrono::Utc::now().to_rfc3339();
                    state
                        .sources
                        .get_mut(&operation.incarnation)
                        .unwrap()
                        .current_operation = None;
                    Ok(())
                }))
                .await?;
            return Ok(());
        }
        let changing = matches!(
            operation.action,
            ManagementAction::Apply { .. }
                | ManagementAction::Disable { .. }
                | ManagementAction::Refresh { .. }
        );
        if changing
            && (!matches!(operation.action, ManagementAction::Refresh { .. }) || strict_refresh)
        {
            registry.close(&source.id);
            let incarnation = source.incarnation.clone();
            let disabling = matches!(operation.action, ManagementAction::Disable { .. });
            storage
                .port
                .update(Box::new(move |state| {
                    let source = state.sources.get_mut(&incarnation).unwrap();
                    source.status = if disabling {
                        SourceStatus::Draining
                    } else {
                        SourceStatus::Activating
                    };
                    Ok(())
                }))
                .await?;
        }
        let memory = tokio::select! {
            permit=self.memory.clone().acquire_many_owned(self.config.management.metadata_operation_memory_mb)=>Arc::new(permit.map_err(|_|EngineError::new("57P01","metadata budget closed"))?),
            _=shutdown.cancelled()=>return Ok(()),
        };
        let expected_version = registry.get(&source.id).ok().map(|s| s.version());
        self.phase(id, "Collecting", OperationStatus::Running)
            .await?;
        let is_apply = matches!(operation.action, ManagementAction::Apply { .. }) || strict_refresh;
        let is_refresh =
            matches!(operation.action, ManagementAction::Refresh { .. }) && !strict_refresh;
        let is_disable = matches!(operation.action, ManagementAction::Disable { .. });
        let mut outcome = ControlOutcome {
            schemas: vec![],
            catalog: None,
            restored_previous: false,
            error: None,
        };
        if is_apply || is_refresh {
            let revision = if is_apply {
                desired.clone()
            } else {
                previous.clone().ok_or_else(|| {
                    EngineError::new("55000", "refresh requires an applied revision")
                })?
            };
            match control
                .port
                .capture(
                    revision,
                    shutdown.clone(),
                    Box::new((
                        self.control.acquire().map_err(engine)?,
                        self.storage.acquire().map_err(engine)?,
                        self.registry.acquire().map_err(engine)?,
                        memory.clone(),
                    )),
                )
                .await
            {
                Ok(catalog) => outcome.catalog = Some(catalog),
                Err(error) => {
                    outcome.error = Some(error);
                    outcome.restored_previous = is_apply
                        && previous.is_some()
                        && registry.get(&source.id).is_ok()
                        && !self
                            .config
                            .management
                            .mysql_identity_policy
                            .resets(source.kind);
                }
            }
        } else if !is_disable {
            // Probe/discover also run in a separate native instance.
            let inspected = control
                .port
                .inspect(
                    desired.clone(),
                    shutdown.clone(),
                    Box::new((self.control.acquire().map_err(engine)?, memory.clone())),
                )
                .await;
            match inspected {
                Ok(value) => outcome = value,
                Err(error) => outcome.error = Some(error),
            }
        }
        if !changing {
            let id = id.to_owned();
            storage
                .port
                .update(Box::new(move |state| {
                    let operation = state.operations.get_mut(&id).unwrap();
                    operation.status = if outcome.error.is_none() {
                        OperationStatus::Succeeded
                    } else {
                        OperationStatus::Failed
                    };
                    operation.phase = "Complete".into();
                    operation.error = outcome.error.map(|e| e.to_string());
                    operation.result_schemas = Some(outcome.schemas);
                    operation.updated_at = chrono::Utc::now().to_rfc3339();
                    state
                        .sources
                        .get_mut(&operation.incarnation)
                        .unwrap()
                        .current_operation = None;
                    Ok(())
                }))
                .await?;
            return Ok(());
        }
        if shutdown.is_cancelled() {
            return Ok(());
        }
        // Source drain is independent of remote collection. The instance barrier is only
        // held for the live ATTACH/DETACH/cache mutation, never for the catalog scan.
        if changing && (outcome.error.is_none() || (is_apply && !outcome.restored_previous)) {
            self.phase(
                id,
                "WaitingForLiveMutation",
                OperationStatus::WaitingForDrain,
            )
            .await?;
            let barrier = acquire_barrier(&registry, &shutdown).await?;
            let source_lock = if let Ok(current) = registry.get(&source.id) {
                Some(current.fence.clone().write_owned().await)
            } else {
                None
            };
            let live = if is_refresh {
                previous.clone().map(ControlAction::ClearCache)
            } else {
                Some(ControlAction::Mount {
                    previous: previous
                        .clone()
                        .filter(|_| registry.get(&source.id).is_ok()),
                    next: if is_apply && outcome.error.is_none() {
                        Some(desired.clone())
                    } else {
                        None
                    },
                })
            };
            if let Some(action) = live {
                let live = control
                    .port
                    .control(
                        action,
                        Box::new((
                            barrier,
                            source_lock,
                            self.control.acquire().map_err(engine)?,
                            self.storage.acquire().map_err(engine)?,
                            self.registry.acquire().map_err(engine)?,
                        )),
                    )
                    .await;
                match live {
                    Ok(live) if live.error.is_none() => {}
                    Ok(live) => {
                        outcome.catalog = None;
                        outcome.restored_previous = live.restored_previous;
                        outcome.error = live.error;
                    }
                    Err(error) => {
                        outcome.catalog = None;
                        outcome.restored_previous = false;
                        outcome.error = Some(error);
                    }
                }
            }
        }
        let succeeded = outcome.error.is_none();
        let generation_before = expected_version
            .as_ref()
            .map(|v| v.generation)
            .unwrap_or(source.generation)
            .max(source.generation);
        let changed = outcome.catalog.as_ref().is_some_and(|c| {
            source
                .catalog_snapshot
                .as_ref()
                .and_then(|id| state.catalogs.get(id))
                .is_none_or(|old| old.structure_digest != c.structure_digest)
        });
        let generation = generation_before
            + u64::from(
                is_apply
                    || is_disable
                    || (changed && self.config.management.enabled)
                    || (!succeeded && outcome.restored_previous),
            );
        if let Some(catalog) = &mut outcome.catalog {
            catalog.generation = generation;
        }
        let previous_stale = source.status == SourceStatus::MetadataStale
            || registry
                .get(&source.id)
                .is_ok_and(|s| s.stale.load(std::sync::atomic::Ordering::SeqCst));
        let restoring = outcome.restored_previous && !previous_stale;
        let status = if succeeded {
            if is_disable {
                SourceStatus::Disabled
            } else if is_apply || is_refresh {
                SourceStatus::Active
            } else {
                source.status
            }
        } else if restoring {
            SourceStatus::Active
        } else if is_refresh || (outcome.restored_previous && previous_stale) {
            SourceStatus::MetadataStale
        } else if changing {
            SourceStatus::Degraded
        } else {
            source.status
        };
        self.phase(id, "PreparingPublication", OperationStatus::Running)
            .await?;
        let operation_id = id.to_owned();
        let incarnation = source.incarnation.clone();
        let target_revision = desired.revision;
        let reason = outcome.error.map(|e| e.to_string());
        // Keep retryable facts compressed rather than retaining another full object graph.
        // The sole decoded copy is owned by the storage preparation worker.
        let catalog = if let Some(catalog) = outcome.catalog {
            let owned = (
                self.storage.acquire().map_err(engine)?,
                self.registry.acquire().map_err(engine)?,
                memory.clone(),
            );
            let bytes = self
                .tasks
                .spawn_cpu(move |cancel| {
                    let _owned = owned;
                    Ok(stage_catalog(&catalog, cancel))
                })
                .map_err(engine)?
                .wait_finished()
                .await
                .map_err(engine)??;
            Some(Arc::new(bytes))
        } else {
            None
        };
        let schemas = outcome.schemas;
        let expected_snapshot = source.catalog_snapshot.clone();
        let mut attempts = 0;
        let (prepared, barrier) = loop {
            attempts += 1;
            let operation_id = operation_id.clone();
            let incarnation = incarnation.clone();
            let reason = reason.clone();
            let catalog = catalog.clone();
            let schemas = schemas.clone();
            let expected_snapshot = expected_snapshot.clone();
            let prepared = storage
                .port
                .prepare_publication(
                    incarnation.clone(),
                    Box::new(move |state| {
                        let source = state.sources.get_mut(&incarnation).ok_or_else(|| {
                            EngineError::new("40001", "source disappeared during preparation")
                        })?;
                        if source.current_operation.as_deref() != Some(&operation_id)
                            || source.desired_revision != target_revision
                            || source.catalog_snapshot != expected_snapshot
                        {
                            return Err(EngineError::new(
                                "40001",
                                "management target changed during preparation",
                            ));
                        }
                        source.status = status;
                        source.last_error = reason.clone();
                        source.generation = generation;
                        if succeeded && is_apply {
                            source.applied_revision = Some(target_revision);
                        }
                        if let Some(catalog) = catalog {
                            let reader = std::io::BufReader::new(flate2::read::ZlibDecoder::new(
                                catalog.as_slice(),
                            ));
                            let catalog: CatalogSnapshot = serde_json::from_reader(reader)
                                .map_err(|_| {
                                    EngineError::new("XX000", "invalid staged metadata facts")
                                })?;
                            source.catalog_snapshot = Some(catalog.id.clone());
                            state.catalogs.insert(catalog.id.clone(), catalog);
                        }
                        if !succeeded
                            && (is_refresh || (changing && !restoring))
                            && let Some(snapshot) = source
                                .catalog_snapshot
                                .as_ref()
                                .and_then(|id| state.catalogs.get_mut(id))
                        {
                            snapshot.stale = true;
                        }
                        source.current_operation = None;
                        let operation = state.operations.get_mut(&operation_id).unwrap();
                        operation.status = if succeeded {
                            OperationStatus::Succeeded
                        } else {
                            OperationStatus::Failed
                        };
                        operation.phase = "Complete".into();
                        operation.error = reason;
                        operation.result_schemas = Some(schemas);
                        operation.updated_at = chrono::Utc::now().to_rfc3339();
                        Ok(())
                    }),
                    Box::new((
                        self.storage.acquire().map_err(engine)?,
                        self.registry.acquire().map_err(engine)?,
                        memory.clone(),
                    )),
                )
                .await;
            let prepared = match prepared {
                Ok(p) => p,
                Err(error) => {
                    self.abort_publication(id, &source, is_apply || is_disable, error, &registry)
                        .await?;
                    return Ok(());
                }
            };
            // Non-queued, bounded attempts avoid repeatedly rebuilding a large
            // transaction just because one short query overlaps preparation exit.
            // Instance owners must try the writer without waiting (virtuals do so).
            if let Ok(Ok(barrier)) = tokio::time::timeout(
                Duration::from_millis(100),
                acquire_barrier(&registry, &shutdown),
            )
            .await
            {
                break (prepared, barrier);
            }
            // Never retain the writer across an unbounded drain or suspended portal.
            // Abort before publishing a wait phase or accepting another local attempt.
            drop(prepared);
            if shutdown.is_cancelled() {
                return Ok(());
            }
            if attempts >= 3 {
                self.abort_publication(
                    id,
                    &source,
                    is_apply || is_disable,
                    EngineError::new(
                        "55P03",
                        "publication conflicted with active execution; retry refresh",
                    ),
                    &registry,
                )
                .await?;
                return Ok(());
            }
            self.phase(
                id,
                "WaitingForPublicationDrain",
                OperationStatus::WaitingForDrain,
            )
            .await?;
            drop(acquire_barrier(&registry, &shutdown).await?);
        };
        let barrier_started = std::time::Instant::now();
        let current_version = registry.get(&source.id).ok().map(|s| s.version());
        if current_version != expected_version {
            // The unpublished transaction must be dropped before acquiring the writer again.
            drop(prepared);
            let _ = registry.invalidate(&source.id, true);
            drop(barrier);
            let id = id.to_owned();
            let incarnation = source.incarnation.clone();
            storage
                .port
                .update(Box::new(move |state| {
                    let source = state.sources.get_mut(&incarnation).unwrap();
                    source.status = if is_apply || is_disable {
                        SourceStatus::Degraded
                    } else {
                        SourceStatus::MetadataStale
                    };
                    source.current_operation = None;
                    if let Some(c) = source
                        .catalog_snapshot
                        .as_ref()
                        .and_then(|id| state.catalogs.get_mut(id))
                    {
                        c.stale = true;
                    }
                    let op = state.operations.get_mut(&id).unwrap();
                    op.status = OperationStatus::Failed;
                    op.phase = "VersionConflict".into();
                    op.error = Some("40001: source generation changed during collection".into());
                    Ok(())
                }))
                .await?;
            return Ok(());
        }
        let registry_for_commit = self.registry.acquire().map_err(engine)?;
        let source_id = source.id.clone();
        let incarnation = source.incarnation.clone();
        let effective = if succeeded && (is_apply || is_refresh) {
            Some(if is_apply {
                desired.clone()
            } else {
                previous.clone().unwrap()
            })
        } else if restoring {
            previous.clone()
        } else {
            None
        };
        let committed = prepared
            .commit(
                Box::new((
                    barrier,
                    self.storage.acquire().map_err(engine)?,
                    self.registry.acquire().map_err(engine)?,
                    memory,
                )),
                Box::new(move || {
                    if changing {
                        if let Some(revision) = effective {
                            registry_for_commit.publish(
                                revision.effective_definition(),
                                incarnation,
                                revision.revision,
                                generation,
                            );
                        } else if succeeded && is_disable {
                            registry_for_commit.remove(&source_id);
                        } else if is_refresh || previous_stale {
                            let _ = registry_for_commit.invalidate(&source_id, true);
                        }
                    }
                }),
            )
            .await;
        if let Err(error) = committed {
            self.abort_publication(id, &source, is_apply || is_disable, error, &registry)
                .await?;
            return Ok(());
        }
        tracing::debug!(operation=%id,elapsed_ms=barrier_started.elapsed().as_secs_f64()*1000.,"management publication finalization finished");
        Ok(())
    }
}
/// Keep compression buffered and cooperate with owner cancellation between chunks.
fn stage_catalog(catalog: &CatalogSnapshot, cancel: CancellationToken) -> Result<Vec<u8>> {
    use std::io::Write;
    struct CancelWriter<W> {
        inner: W,
        cancel: CancellationToken,
    }
    impl<W: Write> Write for CancelWriter<W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.cancel.is_cancelled() {
                return Err(std::io::Error::other("metadata staging canceled"));
            }
            self.inner.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    let failure = || {
        if cancel.is_cancelled() {
            EngineError::new("57014", "metadata staging canceled")
        } else {
            EngineError::new("XX000", "cannot stage metadata facts")
        }
    };
    let mut writer = CancelWriter {
        inner: flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast()),
        cancel: cancel.clone(),
    };
    {
        let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, &mut writer);
        serde_json::to_writer(&mut buffered, catalog).map_err(|_| failure())?;
        buffered.flush().map_err(|_| failure())?;
    }
    writer.inner.finish().map_err(|_| failure())
}

/// Do not queue a write waiter on Tokio's fair RwLock: a suspended portal must
/// not turn one pending publication into a global query admission outage.
async fn acquire_barrier(
    registry: &DataSourceRegistry,
    shutdown: &CancellationToken,
) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>> {
    loop {
        if shutdown.is_cancelled() {
            return Err(EngineError::new(
                "57P01",
                "publication canceled before commit",
            ));
        }
        if let Ok(guard) = registry.instance.clone().try_write_owned() {
            return Ok(guard);
        }
        tokio::select! {_=shutdown.cancelled()=>return Err(EngineError::new("57P01","publication canceled before commit")),_=tokio::time::sleep(Duration::from_millis(1))=>{}}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn waiting_publication_does_not_queue_a_writer_ahead_of_new_queries() {
        let registry = Arc::new(DataSourceRegistry::new(&[]));
        let portal = registry.instance.clone().read_owned().await;
        let cancel = CancellationToken::new();
        let owner = registry.clone();
        let token = cancel.clone();
        let waiter = tokio::spawn(async move { acquire_barrier(&owner, &token).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let query = tokio::time::timeout(
            Duration::from_millis(100),
            registry.instance.clone().read_owned(),
        )
        .await
        .expect("pending publication must not block unrelated readers");
        drop(query);
        cancel.cancel();
        assert_eq!(waiter.await.unwrap().unwrap_err().sqlstate, "57P01");
        drop(portal);
        assert!(registry.instance.clone().try_write_owned().is_ok());
    }
}
