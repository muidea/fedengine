use crate::{coordinator::Coordinator, reducer::accept};
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use modorch::{prelude::*, *};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct SourceManagerFactory {
    pub config: Arc<Config>,
}
struct SourceManager {
    config: Arc<Config>,
    coordinator: Option<Coordinator>,
}
fn descriptor() -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::module("source-manager"));
    descriptor.requires.extend([
        ComponentRef::initiator("duckdb-instance"),
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::initiator("duckdb-runtime"),
        ComponentRef::module("catalog-manager"),
    ]);
    descriptor
}
impl ModuleFactory for SourceManagerFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(SourceManager {
            config: self.config.clone(),
            coordinator: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for SourceManager {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.coordinator = Some(Coordinator {
            config: self.config.clone(),
            tasks: ctx.tasks.clone(),
            storage: ctx
                .services
                .get(&ComponentRef::initiator("duckdb-instance"), "storage")?,
            registry: ctx
                .services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
            control: ctx
                .services
                .get(&ComponentRef::initiator("duckdb-runtime"), "control")?,
            memory: Arc::new(tokio::sync::Semaphore::new(
                self.config.management.metadata_memory_budget_mb as usize,
            )),
            slots: Arc::new(tokio::sync::Semaphore::new(
                self.config.management.max_concurrent_operations,
            )),
        });
        Ok(())
    }
}
impl Module for SourceManager {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Serial {
            handler: Box::new(ManagerHandler {
                coordinator: self.coordinator.clone().unwrap(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                40,
                descriptor().component(),
                MANAGEMENT_TOPIC,
            )],
        }))
    }
}
struct ManagerHandler {
    coordinator: Coordinator,
}
fn framework_error(error: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("management admission failed: {error}"))
}
#[async_trait]
impl SerialHandler for ManagerHandler {
    async fn handle(
        &mut self,
        ctx: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<ManagementRequest>()?;
        Ok(Arc::new(self.handle_request(ctx, request.as_ref()).await))
    }
}
impl ManagerHandler {
    async fn submit(&self, action: ManagementAction, key: String) -> Result<ManagementReply> {
        let digest = Sha256::digest(
            serde_json::to_vec(&action)
                .map_err(|_| EngineError::config("invalid management action"))?,
        )
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let capacity = self.coordinator.config.management.queue_capacity;
        let identity_policy = self.coordinator.config.management.mysql_identity_policy;
        let update_id = operation_id.clone();
        let update_key = key.clone();
        let storage = self
            .coordinator
            .storage
            .acquire()
            .map_err(framework_error)?;
        let state = storage
            .port
            .update(Box::new(move |state| {
                accept(
                    state,
                    action,
                    update_key,
                    digest,
                    update_id,
                    capacity,
                    identity_policy,
                )
            }))
            .await?;
        let operation = state
            .operations
            .values()
            .find(|o| o.idempotency_key == key)
            .unwrap()
            .clone();
        if operation.id == operation_id {
            // The intent is already durable; a scheduler failure must remain visible and recoverable.
            if self.coordinator.start(operation_id.clone()).is_err() {
                let id = operation_id;
                storage
                    .port
                    .update(Box::new(move |state| {
                        let operation = state.operations.get_mut(&id).unwrap();
                        operation.error =
                            Some("operation could not start: owner scheduler unavailable".into());
                        operation.status = OperationStatus::Failed;
                        operation.phase = "SchedulingFailed".into();
                        state
                            .sources
                            .get_mut(&operation.incarnation)
                            .unwrap()
                            .current_operation = None;
                        Ok(())
                    }))
                    .await?;
            }
        }
        Ok(ManagementReply::Operation(operation))
    }
    async fn handle_request(
        &self,
        ctx: &HandlerContext,
        request: &ManagementRequest,
    ) -> Result<ManagementReply> {
        match request {
            ManagementRequest::VirtualDatabases | ManagementRequest::VirtualDatabase(_) => {
                let storage = self
                    .coordinator
                    .storage
                    .acquire()
                    .map_err(framework_error)?;
                let databases = storage.port.catalog_headers().await?.databases;
                return match request {
                    ManagementRequest::VirtualDatabase(name) => databases
                        .into_iter()
                        .find(|d| d.config.name == *name)
                        .map(ManagementReply::VirtualDatabase)
                        .ok_or_else(|| {
                            EngineError::new("3D000", "virtual database does not exist")
                        }),
                    _ => Ok(ManagementReply::VirtualDatabases(databases)),
                };
            }
            ManagementRequest::MutateVirtualDatabase {
                mutation,
                idempotency_key,
            } => {
                let registry = self
                    .coordinator
                    .registry
                    .acquire()
                    .map_err(framework_error)?;
                // Fail before mutation while queries or suspended portals own the instance.
                // Never wait for their client inside the Serial manager handler.
                let barrier=registry.instance.clone().try_write_owned().map_err(|_|EngineError::new("55P03","active execution prevents virtual database publication; close or drain portals and retry with the same Idempotency-Key"))?;
                let storage = self
                    .coordinator
                    .storage
                    .acquire()
                    .map_err(framework_error)?;
                let receipt = storage
                    .port
                    .virtual_database_mutation(
                        mutation.clone(),
                        idempotency_key.clone(),
                        self.coordinator
                            .config
                            .users
                            .iter()
                            .map(|u| u.name.clone())
                            .collect(),
                        Box::new((
                            barrier,
                            registry,
                            self.coordinator
                                .storage
                                .acquire()
                                .map_err(framework_error)?,
                        )),
                    )
                    .await?;
                return Ok(ManagementReply::VirtualDatabaseReceipt(receipt));
            }
            _ => {}
        }
        if let ManagementRequest::Submit {
            action,
            idempotency_key,
        } = request
        {
            return self.submit(action.clone(), idempotency_key.clone()).await;
        }
        if let ManagementRequest::Catalog {
            object,
            snapshot_id,
            id,
            schema,
            object_type,
            offset,
            limit,
        } = request
        {
            let reply = ctx
                .event_hub
                .request::<_, Result<fedengine_catalog::CatalogReply>>(
                    fedengine_catalog::CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    fedengine_catalog::CatalogRequest::Directory(DirectoryRequest {
                        object: object.clone(),
                        snapshot_id: snapshot_id.clone(),
                        id: id.clone(),
                        schema: schema.clone(),
                        object_type: object_type.clone(),
                        offset: *offset,
                        limit: *limit,
                    }),
                    Default::default(),
                )
                .map_err(framework_error)?
                .wait(std::time::Duration::from_millis(
                    self.coordinator.config.management.request_timeout_ms,
                ))
                .await
                .map_err(framework_error)?;
            return match reply.as_ref() {
                Ok(fedengine_catalog::CatalogReply::Directory(page)) => {
                    Ok(ManagementReply::Catalog(page.clone()))
                }
                Err(error) => Err(error.clone()),
                _ => Err(EngineError::new("XX000", "unexpected catalog response")),
            };
        };
        let registry = self
            .coordinator
            .registry
            .acquire()
            .map_err(framework_error)?;
        let storage = self
            .coordinator
            .storage
            .acquire()
            .map_err(framework_error)?;
        let state = storage.port.read().await?;
        match request {
            ManagementRequest::List => Ok(ManagementReply::Sources(
                state
                    .sources
                    .values()
                    .filter(|s| !s.deleted)
                    .map(|s| view(&state, s, &registry))
                    .collect::<Result<_>>()?,
            )),
            ManagementRequest::Get(id) => Ok(ManagementReply::Source(Box::new(view(
                &state,
                state.source(id)?,
                &registry,
            )?))),
            ManagementRequest::Operation(id) => state
                .operations
                .get(id)
                .cloned()
                .map(ManagementReply::Operation)
                .ok_or_else(|| EngineError::new("42704", "operation does not exist")),
            ManagementRequest::Catalog { .. } => unreachable!(),
            ManagementRequest::DdlInvalidated(id) => {
                let source = state.source(id)?;
                let incarnation = source.incarnation.clone();
                storage
                    .port
                    .update(Box::new(move |state| {
                        let source = state.sources.get_mut(&incarnation).unwrap();
                        if source.status == SourceStatus::Active {
                            source.status = SourceStatus::MetadataStale;
                        }
                        if let Some(snapshot) = source
                            .catalog_snapshot
                            .as_ref()
                            .and_then(|id| state.catalogs.get_mut(id))
                        {
                            snapshot.stale = true;
                        }
                        Ok(())
                    }))
                    .await?;
                if let Some(current) = &source.current_operation
                    && let Some(operation) = state
                        .operations
                        .get(current)
                        .filter(|o| !o.status.finished())
                {
                    if !matches!(
                        operation.action,
                        ManagementAction::Apply { .. }
                            | ManagementAction::Disable { .. }
                            | ManagementAction::Refresh { .. }
                    ) {
                        let storage = self
                            .coordinator
                            .storage
                            .acquire()
                            .map_err(framework_error)?;
                        let id = id.clone();
                        let hub = ctx.event_hub.clone();
                        self.coordinator.tasks.spawn(move |shutdown| async move {
                                loop {
                                    let state = storage.port.read().await.map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?;
                                    let source = state.source(&id).map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?;
                                    if source.current_operation.is_none() {
                                        if let Ok(ticket) = hub.request::<_, Result<ManagementReply>>(MANAGEMENT_TOPIC, Destination::Component(ComponentRef::module("source-manager")), ManagementRequest::DdlInvalidated(id), Default::default()) {
                                            let _ = ticket.wait(std::time::Duration::from_secs(10)).await;
                                        }
                                        break;
                                    }
                                    tokio::select! { _ = shutdown.cancelled() => break, _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {} }
                                }
                                Ok(())
                            }).map_err(framework_error)?;
                    }
                    return Ok(ManagementReply::Operation(operation.clone()));
                }
                self.submit(
                    ManagementAction::Refresh {
                        id: id.clone(),
                        target: SourceTarget {
                            incarnation: source.incarnation.clone(),
                            expected_revision: source.desired_revision,
                        },
                    },
                    format!("ddl:{}", uuid::Uuid::new_v4()),
                )
                .await
            }
            ManagementRequest::Submit { .. }
            | ManagementRequest::VirtualDatabases
            | ManagementRequest::VirtualDatabase(_)
            | ManagementRequest::MutateVirtualDatabase { .. } => unreachable!(),
        }
    }
}
fn view(
    state: &ManagementState,
    source: &ManagedSource,
    registry: &DataSourceRegistry,
) -> Result<SourceView> {
    let mut source = source.clone();
    match registry.get(&source.id) {
        Ok(effective) => {
            source.generation = effective
                .generation
                .load(std::sync::atomic::Ordering::SeqCst);
            if source.status == SourceStatus::Active {
                if effective.stale.load(std::sync::atomic::Ordering::SeqCst) {
                    source.status = SourceStatus::MetadataStale;
                } else if !effective.admitted.load(std::sync::atomic::Ordering::SeqCst) {
                    source.status = SourceStatus::Activating;
                }
            }
        }
        Err(_) if source.status == SourceStatus::Active => source.status = SourceStatus::Activating,
        Err(_) => {}
    }
    let desired = state
        .revision(&source.incarnation, source.desired_revision)?
        .clone();
    let applied = source
        .applied_revision
        .map(|r| state.revision(&source.incarnation, r).cloned())
        .transpose()?;
    let catalog_updated_at = source
        .catalog_snapshot
        .as_ref()
        .and_then(|id| state.catalogs.get(id))
        .map(|c| c.refreshed_at.clone());
    Ok(SourceView {
        source,
        desired,
        applied,
        catalog_updated_at,
    })
}
