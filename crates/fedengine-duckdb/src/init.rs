use crate::{
    error::{engine_error, fw},
    extensions::verify_extension,
    runtime::DuckdbRuntime,
};
use duckdb::Connection;
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use modorch::{prelude::*, *};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
pub struct DuckdbFactory {
    pub metrics: Arc<EngineMetrics>,
    pub config: Arc<Config>,
}
struct DuckdbInitiator {
    metrics: Arc<EngineMetrics>,
    config: Arc<Config>,
    service: Option<Arc<ExecutionService>>,
}
fn descriptor() -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::initiator("duckdb-runtime"));
    d.requires
        .push(ComponentRef::initiator("datasource-registry"));
    d
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
        }))
    }
}
#[async_trait]
impl Lifecycle for DuckdbInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let handle: ServiceHandle<DataSourceRegistry> = ctx
            .services
            .get(&ComponentRef::initiator("datasource-registry"), "registry")?;
        let registry = handle.acquire()?;
        let config = self.config.clone();
        let connection = ctx
            .tasks
            .spawn_blocking_io(move |_| Ok(initialize(&config)))?
            .wait_finished()
            .await??;
        self.service = Some(Arc::new(ExecutionService {
            port: Arc::new(DuckdbRuntime {
                metrics: self.metrics.clone(),
                root: Arc::new(Mutex::new(connection)),
                slots: Arc::new(Semaphore::new(self.config.duckdb.max_workers)),
                tasks: ctx.tasks.clone(),
                config: self.config.clone(),
                registry: handle,
            }),
        }));
        drop(registry);
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.service.take();
        Ok(())
    }
}
impl Initiator for DuckdbInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn export_services(&self, p: &mut ServicePublisher) -> FrameworkResult<()> {
        p.publish("execution", self.service.as_ref().unwrap().clone())
    }
}
fn initialize(config: &Config) -> FrameworkResult<Connection> {
    let d = &config.duckdb;
    if d.database_path != ":memory:"
        && let Some(parent) = std::path::Path::new(&d.database_path).parent()
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
    for kind in [DataSourceKind::Postgres, DataSourceKind::Mysql] {
        if !config
            .datasources
            .iter()
            .any(|s| s.enabled && s.kind == kind)
        {
            continue;
        }
        let ext = kind.extension();
        if d.install_extensions {
            conn.execute_batch("SET custom_extension_repository='https://extensions.duckdb.org'")
                .map_err(engine_error)
                .map_err(fw)?;
            conn.execute_batch(&format!("INSTALL {ext}"))
                .map_err(engine_error)
                .map_err(fw)?;
        }
        if let Some(directory) = &d.extension_directory {
            verify_extension(directory, ext).map_err(fw)?;
        }
        conn.execute_batch(&format!("LOAD {ext}"))
            .map_err(engine_error)
            .map_err(fw)?;
        let settings = if kind == DataSourceKind::Postgres {
            format!(
                "SET pg_pool_acquire_mode='wait'; SET pg_pool_max_connections={}; SET pg_pool_wait_timeout_millis={}",
                d.pool_max_connections, d.pool_wait_timeout_ms
            )
        } else {
            format!(
                "SET mysql_pool_acquire_mode='wait'; SET mysql_pool_size={}; SET mysql_pool_wait_timeout_millis={}",
                d.pool_max_connections, d.pool_wait_timeout_ms
            )
        };
        conn.execute_batch(&settings)
            .map_err(engine_error)
            .map_err(fw)?;
        for source in config
            .datasources
            .iter()
            .filter(|s| s.enabled && s.kind == kind)
        {
            let secret = std::env::var(&source.secret_ref)
                .map_err(|_| fw(EngineError::config("data source SecretRef is missing")))?;
            if secret.is_empty() {
                return Err(fw(EngineError::config("data source SecretRef is empty")));
            }
            // Only this privileged initialization path builds ATTACH. Errors are scrubbed.
            let sql = format!(
                "ATTACH {} AS {} (TYPE {}{})",
                quote_literal(&secret),
                quote_identifier(&source.catalog_alias),
                ext,
                if source.read_only { ", READ_ONLY" } else { "" }
            );
            conn.execute_batch(&sql).map_err(engine_error).map_err(fw)?;
            if let Some(c) = config.capabilities.iter().find(|c| c.source == source.id) {
                let extension_version: String = conn
                    .query_row(
                        "SELECT extension_version FROM duckdb_extensions() WHERE extension_name=?",
                        [format!("{ext}_scanner")],
                        |r| r.get(0),
                    )
                    .map_err(engine_error)
                    .map_err(fw)?;
                let remote_sql = if kind == DataSourceKind::Postgres {
                    "SELECT current_setting('server_version') AS version"
                } else {
                    "SELECT VERSION() AS version"
                };
                let remote: String = conn
                    .query_row(
                        &format!(
                            "SELECT version FROM {ext}_query({}, {})",
                            quote_literal(&source.catalog_alias),
                            quote_literal(remote_sql)
                        ),
                        [],
                        |r| r.get(0),
                    )
                    .map_err(engine_error)
                    .map_err(fw)?;
                if extension_version != c.extension_version || remote != c.remote_version {
                    return Err(fw(EngineError::config(
                        "capability extension/remote version does not match",
                    )));
                }
            }
        }
    }
    Ok(conn)
}
