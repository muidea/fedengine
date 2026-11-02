//! Catalog control requests; remote indexes are discovered through fixed DuckDB queries.
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use fedengine_ddl::{IndexOwner, ObjectRef};
use modorch::{prelude::*, *};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};
pub const CATALOG_TOPIC: &str = "catalog/control/v1";
#[derive(Clone)]
pub enum CatalogRequest {
    Snapshot,
    Indexes(DataSourceId),
}
pub enum CatalogReply {
    Snapshot(BTreeMap<DataSourceId, u64>),
    Indexes(Vec<IndexOwner>),
}
pub struct CatalogFactory;
struct CatalogModule {
    registry: Option<ServiceHandle<DataSourceRegistry>>,
    runtime: Option<ServiceHandle<ExecutionService>>,
}
fn descriptor() -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::module("catalog-manager"));
    d.requires.extend([
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::initiator("duckdb-runtime"),
    ]);
    d
}
impl ModuleFactory for CatalogFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(CatalogModule {
            registry: None,
            runtime: None,
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
        Ok(())
    }
}
impl Module for CatalogModule {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Serial {
            handler: Box::new(CatalogHandler {
                registry: self.registry.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                10,
                descriptor().component(),
                CATALOG_TOPIC,
            )],
        }))
    }
}
struct CatalogHandler {
    registry: ServiceHandle<DataSourceRegistry>,
    runtime: ServiceHandle<ExecutionService>,
}
#[async_trait]
impl SerialHandler for CatalogHandler {
    async fn handle(
        &mut self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<CatalogRequest>()?;
        let result: Result<CatalogReply> = match request.as_ref() {
            CatalogRequest::Snapshot => {
                let r = self.registry.acquire()?;
                Ok(CatalogReply::Snapshot(
                    r.sources()
                        .map(|s| (s.definition.id.clone(), s.generation.load(Ordering::SeqCst)))
                        .collect(),
                ))
            }
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
