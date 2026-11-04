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
        // Never hold a source lock or a local write transaction while waiting here.
        self.phase(
            id,
            "WaitingForInstanceAndDrain",
            OperationStatus::WaitingForDrain,
        )
        .await?;
        let barrier = loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                lock = tokio::time::timeout(Duration::from_millis(self.config.management.drain_timeout_ms), registry.instance.clone().write_owned()) => {
                    if let Ok(lock) = lock { break lock; }
                    self.phase(id, "WaitingForDrain: active execution or suspended portal", OperationStatus::WaitingForDrain).await?;
                }
            }
        };
        let barrier_started = std::time::Instant::now();
        let source_lock = if let Ok(source) = registry.get(&source.id) {
            Some(source.fence.clone().write_owned().await)
        } else {
            None
        };
        if shutdown.is_cancelled() {
            return Ok(());
        }
        let phase = match operation.action {
            ManagementAction::Apply { .. } => "ProbingAndAttaching",
            ManagementAction::Disable { .. } => "Detaching",
            ManagementAction::Refresh { .. } => "Refreshing",
            _ => "Probing",
        };
        self.phase(id, phase, OperationStatus::Running).await?;
        let action = match operation.action {
            ManagementAction::Refresh { .. } if strict_refresh => Some(ControlAction::Switch {
                previous: previous
                    .clone()
                    .filter(|_| registry.get(&source.id).is_ok()),
                next: desired.clone(),
            }),
            ManagementAction::Apply { .. } => Some(ControlAction::Switch {
                previous: previous
                    .clone()
                    .filter(|_| registry.get(&source.id).is_ok()),
                next: desired.clone(),
            }),
            ManagementAction::Disable { .. } => previous
                .clone()
                .filter(|_| registry.get(&source.id).is_ok())
                .map(ControlAction::Disable),
            ManagementAction::Refresh { .. } => previous.clone().map(ControlAction::Refresh),
            ManagementAction::Probe { .. } | ManagementAction::Discover { .. } => {
                Some(ControlAction::Inspect {
                    revision: desired.clone(),
                    reuse_attached: matches!(operation.action, ManagementAction::Discover { .. })
                        && previous
                            .as_ref()
                            .is_some_and(|r| r.revision == desired.revision)
                        && registry.get(&source.id).is_ok(),
                })
            }
            _ => unreachable!(),
        };
        let result = if let Some(action) = action {
            control
                .port
                .control(
                    action,
                    Box::new((
                        self.control.acquire().map_err(engine)?,
                        self.storage.acquire().map_err(engine)?,
                        self.registry.acquire().map_err(engine)?,
                    )),
                )
                .await
        } else {
            Ok(ControlOutcome {
                schemas: vec![],
                catalog: None,
                restored_previous: false,
                error: None,
            })
        };
        let (mut outcome, error) = match result {
            Ok(mut outcome) => {
                let error = outcome.error.take();
                (outcome, error)
            }
            Err(error) => (
                ControlOutcome {
                    schemas: vec![],
                    catalog: None,
                    restored_previous: false,
                    error: None,
                },
                Some(error),
            ),
        };
        let succeeded = error.is_none();
        let is_apply = matches!(operation.action, ManagementAction::Apply { .. }) || strict_refresh;
        let is_refresh =
            matches!(operation.action, ManagementAction::Refresh { .. }) && !strict_refresh;
        let is_disable = matches!(operation.action, ManagementAction::Disable { .. });
        let generation_before = registry
            .get(&source.id)
            .map(|s| s.version().generation)
            .unwrap_or(source.generation)
            .max(source.generation);
        let changed = outcome.catalog.as_ref().is_some_and(|catalog| {
            source
                .catalog_snapshot
                .as_ref()
                .and_then(|id| state.catalogs.get(id))
                .is_none_or(|old| old.structure_digest != catalog.structure_digest)
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
        let catalog = outcome.catalog.clone();
        let reason = error.as_ref().map(ToString::to_string);
        let previous_stale = source.status == SourceStatus::MetadataStale
            || registry
                .get(&source.id)
                .is_ok_and(|s| s.stale.load(std::sync::atomic::Ordering::SeqCst));
        let restoring = outcome.restored_previous && !previous_stale;
        let id_for_store = id.to_owned();
        let incarnation = source.incarnation.clone();
        let schemas = outcome.schemas.clone();
        let new_status = if succeeded {
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
        // Persist the complete effective result before publishing. On storage failure keep
        // admission closed and the operation unfinished; retry without repeating remote I/O.
        loop {
            let id = id_for_store.clone();
            let incarnation = incarnation.clone();
            let catalog = catalog.clone();
            let reason = reason.clone();
            let schemas = schemas.clone();
            let saved = storage
                .port
                .update(Box::new(move |state| {
                    let source = state.sources.get_mut(&incarnation).unwrap();
                    source.status = new_status;
                    source.last_error = reason.clone();
                    source.generation = generation;
                    if succeeded && is_apply {
                        source.applied_revision = Some(desired.revision);
                    }
                    if let Some(catalog) = catalog {
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
                    let operation = state.operations.get_mut(&id).unwrap();
                    operation.phase = "Publishing".into();
                    operation.error = reason;
                    operation.result_schemas = Some(schemas);
                    operation.updated_at = chrono::Utc::now().to_rfc3339();
                    Ok(())
                }))
                .await;
            match saved {
                Ok(_) => break,
                Err(_) => {
                    tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
                }
            }
        }
        if changing {
            if succeeded && (is_apply || is_refresh) {
                let revision = if is_apply {
                    &desired
                } else {
                    previous.as_ref().unwrap()
                };
                registry.publish(
                    revision.effective_definition(),
                    source.incarnation.clone(),
                    revision.revision,
                    generation,
                );
            } else if restoring {
                let revision = previous.as_ref().unwrap();
                registry.publish(
                    revision.effective_definition(),
                    source.incarnation.clone(),
                    revision.revision,
                    generation,
                );
            } else if succeeded && is_disable {
                registry.remove(&source.id);
            } else if is_refresh || previous_stale {
                let _ = registry.invalidate(&source.id, true);
            }
        }
        loop {
            let id = id.to_owned();
            let result = storage
                .port
                .update(Box::new(move |state| {
                    let operation = state.operations.get_mut(&id).unwrap();
                    operation.status = if succeeded {
                        OperationStatus::Succeeded
                    } else {
                        OperationStatus::Failed
                    };
                    operation.phase = "Complete".into();
                    operation.updated_at = chrono::Utc::now().to_rfc3339();
                    state
                        .sources
                        .get_mut(&operation.incarnation)
                        .unwrap()
                        .current_operation = None;
                    state.prune_catalogs(3);
                    Ok(())
                }))
                .await;
            match result {
                Ok(_) => break,
                Err(_) => {
                    tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
                }
            }
        }
        drop((source_lock, barrier));
        tracing::debug!(operation=%id, elapsed_ms=barrier_started.elapsed().as_secs_f64()*1000., "management instance publication barrier released");
        Ok(())
    }
}
