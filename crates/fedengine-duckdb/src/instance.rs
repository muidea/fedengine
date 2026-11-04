use crate::{
    error::{engine_error, fw},
    storage::DuckdbStore,
};
use duckdb::Connection;
use fedengine_contract::ManagementStorage;
use fedengine_core::*;
use modorch::{prelude::*, *};
use std::sync::{Arc, Mutex};

pub struct DuckdbInstanceFactory {
    pub config: Arc<Config>,
}
pub(super) struct DuckdbInstance {
    pub root: Arc<Mutex<Connection>>,
}
struct InstanceInitiator {
    config: Arc<Config>,
    instance: Option<Arc<DuckdbInstance>>,
    storage: Option<Arc<ManagementStorage>>,
}
fn descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::initiator("duckdb-instance"))
}
impl InitiatorFactory for DuckdbInstanceFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(InstanceInitiator {
            config: self.config.clone(),
            instance: None,
            storage: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for InstanceInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let config = self.config.clone();
        let connection = ctx
            .tasks
            .spawn_blocking_io(move |_| {
                let conn = initialize_local(&config)?;
                crate::storage::initialize(&conn, &config).map_err(fw)?;
                Ok(conn)
            })?
            .wait_finished()
            .await?;
        let root = Arc::new(Mutex::new(connection));
        self.instance = Some(Arc::new(DuckdbInstance { root: root.clone() }));
        {
            self.storage = Some(Arc::new(ManagementStorage {
                port: Arc::new(DuckdbStore {
                    root,
                    tasks: ctx.tasks.clone(),
                    writer: Arc::new(tokio::sync::Mutex::new(())),
                }),
            }));
        }
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.storage.take();
        self.instance.take();
        Ok(())
    }
}
impl Initiator for InstanceInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn export_services(&self, publisher: &mut ServicePublisher) -> FrameworkResult<()> {
        publisher.publish("instance", self.instance.as_ref().unwrap().clone())?;
        if let Some(storage) = &self.storage {
            publisher.publish("storage", storage.clone())?;
        }
        Ok(())
    }
}
pub(super) fn initialize_local(config: &Config) -> FrameworkResult<Connection> {
    let d = &config.duckdb;
    if d.database_path != ":memory:"
        && let Some(parent) = std::path::Path::new(&d.database_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|_| fw(EngineError::config("cannot create engine directory")))?;
    }
    std::fs::create_dir_all(&d.temp_directory)
        .map_err(|_| fw(EngineError::config("cannot create temp directory")))?;
    let mut options = duckdb::Config::default();
    options = options
        .with("allow_unsigned_extensions", "false")
        .map_err(engine_error)
        .map_err(fw)?;
    options = options
        .with("autoload_known_extensions", "false")
        .map_err(engine_error)
        .map_err(fw)?;
    options = options
        .with("autoinstall_known_extensions", "false")
        .map_err(engine_error)
        .map_err(fw)?;
    if let Some(dir) = &d.extension_directory {
        options = options
            .with("extension_directory", dir)
            .map_err(engine_error)
            .map_err(fw)?;
    }
    let conn = Connection::open_with_flags(&d.database_path, options)
        .map_err(engine_error)
        .map_err(fw)?;
    let version: String = conn
        .query_row("SELECT version()", [], |r| r.get(0))
        .map_err(engine_error)
        .map_err(fw)?;
    if version != "v2.0.0" {
        return Err(fw(EngineError::config("DuckDB core must be v2.0.0")));
    }
    conn.execute_batch(&format!(
        "SET threads={}; SET memory_limit={}; SET temp_directory={}; SET TimeZone='UTC'",
        d.threads,
        quote_literal(&d.memory_limit),
        quote_literal(&d.temp_directory)
    ))
    .map_err(engine_error)
    .map_err(fw)?;
    crate::functions::register(&conn)
        .map_err(engine_error)
        .map_err(fw)?;
    Ok(conn)
}
