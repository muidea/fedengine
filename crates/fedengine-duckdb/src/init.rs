use crate::{error::fw, instance::DuckdbInstance, runtime::DuckdbRuntime};
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use modorch::{prelude::*, *};
use std::sync::Arc;
use tokio::sync::Semaphore;
pub struct DuckdbFactory {
    pub metrics: Arc<EngineMetrics>,
    pub config: Arc<Config>,
}
struct DuckdbInitiator {
    metrics: Arc<EngineMetrics>,
    config: Arc<Config>,
    service: Option<Arc<ExecutionService>>,
    control: Option<Arc<RuntimeControl>>,
}
fn descriptor() -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::initiator("duckdb-runtime"));
    descriptor.requires.extend([
        ComponentRef::initiator("duckdb-instance"),
        ComponentRef::initiator("datasource-registry"),
    ]);
    descriptor
}
impl InitiatorFactory for DuckdbFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(DuckdbInitiator {
            metrics: self.metrics.clone(),
            config: self.config.clone(),
            service: None,
            control: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for DuckdbInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let registry_handle: ServiceHandle<DataSourceRegistry> = ctx
            .services
            .get(&ComponentRef::initiator("datasource-registry"), "registry")?;
        let instance_handle: ServiceHandle<DuckdbInstance> = ctx
            .services
            .get(&ComponentRef::initiator("duckdb-instance"), "instance")?;
        let instance = instance_handle.acquire()?;
        let root = instance.root.clone();
        let config = self.config.clone();
        let registry = registry_handle.acquire()?;
        let setup_root = root.clone();
        ctx.tasks
            .spawn_blocking_io(move |_| {
                let _instance = instance;
                let conn = setup_root.lock().unwrap();
                // Explicitly provisioned extensions can be verified and initialized
                // before admitting PG sessions, avoiding first-use LOAD in a live barrier.
                if let Some(directory) = &config.duckdb.extension_directory {
                    for kind in [DataSourceKind::Postgres, DataSourceKind::Mysql] {
                        if crate::extensions::provisioned(directory, kind.extension())
                            .map_err(fw)?
                        {
                            crate::control::load_extension(&conn, &config, kind).map_err(fw)?;
                        }
                    }
                }
                if config.management.enabled {
                    crate::control::recover(&conn, &config, &registry).map_err(fw)?;
                } else {
                    crate::control::initialize_static(&conn, &config, &registry).map_err(fw)?;
                }
                Ok(())
            })?
            .wait_finished()
            .await?;
        let runtime = Arc::new(DuckdbRuntime {
            metrics: self.metrics.clone(),
            root,
            slots: Arc::new(Semaphore::new(self.config.duckdb.max_workers)),
            tasks: ctx.tasks.clone(),
            config: self.config.clone(),
            registry: registry_handle,
            hub: ctx.event_hub.clone(),
            storage: Some(
                ctx.services
                    .get(&ComponentRef::initiator("duckdb-instance"), "storage")?,
            ),
        });
        self.control = Some(Arc::new(RuntimeControl {
            port: runtime.clone(),
        }));
        self.service = Some(Arc::new(ExecutionService { port: runtime }));
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.control.take();
        self.service.take();
        Ok(())
    }
}
impl Initiator for DuckdbInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn export_services(&self, publisher: &mut ServicePublisher) -> FrameworkResult<()> {
        publisher.publish("execution", self.service.as_ref().unwrap().clone())?;
        publisher.publish("control", self.control.as_ref().unwrap().clone())
    }
}
