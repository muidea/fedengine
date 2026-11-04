//! Catalog control requests; remote indexes are discovered through fixed DuckDB queries.
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use fedengine_ddl::{IndexOwner, ObjectRef};
use modorch::{prelude::*, *};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
};
pub const CATALOG_TOPIC: &str = "catalog/control/v1";
#[derive(Clone)]
pub enum CatalogRequest {
    Snapshot,
    Indexes(DataSourceId),
    Headers,
    Names(String),
    Directory(DirectoryRequest),
}
pub enum CatalogReply {
    Snapshot(BTreeMap<DataSourceId, SourceVersion>),
    Indexes(Vec<IndexOwner>),
    Directory(CatalogPage),
    Headers(CatalogHeads),
    Names(Arc<BTreeSet<(String, String)>>),
}
pub struct CatalogFactory {
    pub managed: bool,
}
struct CatalogModule {
    registry: Option<ServiceHandle<DataSourceRegistry>>,
    runtime: Option<ServiceHandle<ExecutionService>>,
    storage: Option<ServiceHandle<ManagementStorage>>,
    managed: bool,
}
fn descriptor(managed: bool) -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::module("catalog-manager"));
    d.execution = ExecutionPolicy::Concurrent {
        max_in_flight: NonZeroUsize::new(16).unwrap(),
    };
    d.requires.extend([
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::initiator("duckdb-runtime"),
    ]);
    if managed {
        d.requires.push(ComponentRef::initiator("duckdb-instance"));
    }
    d
}
impl ModuleFactory for CatalogFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(CatalogModule {
            registry: None,
            runtime: None,
            storage: None,
            managed: self.managed,
        }))
    }
}
#[async_trait]
impl Lifecycle for CatalogModule {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.registry = Some(
            ctx.services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
        );
        self.runtime = Some(
            ctx.services
                .get(&ComponentRef::initiator("duckdb-runtime"), "execution")?,
        );
        if self.managed {
            self.storage = Some(
                ctx.services
                    .get(&ComponentRef::initiator("duckdb-instance"), "storage")?,
            );
        }
        Ok(())
    }
}
impl Module for CatalogModule {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Concurrent {
            max_in_flight: NonZeroUsize::new(16).unwrap(),
            handler: Arc::new(CatalogHandler {
                registry: self.registry.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
                storage: self.storage.clone(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                10,
                descriptor(self.managed).component(),
                CATALOG_TOPIC,
            )],
        }))
    }
}
struct CatalogHandler {
    registry: ServiceHandle<DataSourceRegistry>,
    runtime: ServiceHandle<ExecutionService>,
    storage: Option<ServiceHandle<ManagementStorage>>,
}
#[async_trait]
impl ConcurrentHandler for CatalogHandler {
    async fn handle(
        &self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<CatalogRequest>()?;
        let result: Result<CatalogReply> = match request.as_ref() {
            CatalogRequest::Snapshot => {
                let r = self.registry.acquire()?;
                Ok(CatalogReply::Snapshot(
                    r.sources()
                        .map(|s| (s.definition.id.clone(), s.version()))
                        .collect(),
                ))
            }
            CatalogRequest::Names(snapshot) => match &self.storage {
                Some(handle) => handle
                    .acquire()?
                    .port
                    .relation_names(snapshot.clone())
                    .await
                    .map(CatalogReply::Names),
                None => Ok(CatalogReply::Names(Default::default())),
            },
            CatalogRequest::Headers => match &self.storage {
                Some(handle) => handle
                    .acquire()?
                    .port
                    .catalog_headers()
                    .await
                    .map(CatalogReply::Headers),
                None => Ok(CatalogReply::Headers(CatalogHeads::default())),
            },
            CatalogRequest::Directory(r) => self.directory(r).await.map(CatalogReply::Directory),
            CatalogRequest::Indexes(id) => {
                let runtime = self.runtime.acquire()?;
                runtime
                    .port
                    .indexes(id.clone(), Box::new(self.runtime.acquire()?))
                    .await
                    .map(|indexes| {
                        CatalogReply::Indexes(
                            indexes
                                .into_iter()
                                .map(|i| {
                                    let index = ObjectRef {
                                        source: i.source,
                                        catalog: i.catalog,
                                        schema: i.schema,
                                        name: i.index,
                                    };
                                    let table = ObjectRef {
                                        name: i.table,
                                        ..index.clone()
                                    };
                                    IndexOwner {
                                        index,
                                        table,
                                        protected: i.protected,
                                    }
                                })
                                .collect(),
                        )
                    })
            }
        };
        Ok(Arc::new(result))
    }
}

impl CatalogHandler {
    async fn directory(&self, request: &DirectoryRequest) -> Result<CatalogPage> {
        let handle = self
            .storage
            .as_ref()
            .ok_or_else(|| EngineError::unsupported("management is disabled"))?;
        let storage = handle
            .acquire()
            .map_err(|_| EngineError::new("53300", "catalog storage unavailable"))?;
        let mut page = storage.port.directory(request.clone()).await?;
        let registry = self
            .registry
            .acquire()
            .map_err(|_| EngineError::new("53300", "catalog registry unavailable"))?;
        page.stale |= registry
            .get(&request.id)
            .is_ok_and(|s| s.stale.load(std::sync::atomic::Ordering::SeqCst));
        Ok(page)
    }
}
