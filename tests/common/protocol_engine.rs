//! Wire/codec tests supply a catalog fixture through the module boundary. They
//! exercise the real listener, policy, query coordinator and DuckDB workers
//! without creating a production entry database or requiring a remote server.
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_pgwire::{ListenerStatus, PgwireFactory};
use modorch::{prelude::*, *};
use std::{net::SocketAddr, sync::Arc, time::Duration};

pub struct ProtocolEngine {
    pub handle: ApplicationHandle,
    metrics: Arc<EngineMetrics>,
    status: Arc<ListenerStatus>,
    management_status: Arc<fedengine_management::ManagementListenerStatus>,
}
impl ProtocolEngine {
    pub async fn start(
        mut config: Config,
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let fixture = DataSourceDefinition {
            id: DataSourceId("protocol_fixture".into()),
            kind: DataSourceKind::Postgres,
            catalog_alias: "protocol_db".into(),
            secret_ref: "UNUSED_PROTOCOL_FIXTURE".into(),
            default_schema: "public".into(),
            allowed_schemas: vec!["public".into()],
            read_only: true,
            ddl_enabled: false,
            enabled: true,
        };
        // The persisted configuration is disabled, so the runtime never attaches
        // a remote database. Only the test registry and catalog module publish it.
        let mut disabled = fixture.clone();
        disabled.enabled = false;
        config.datasources = vec![disabled];
        config.grants = vec![Grant {
            principal: config.users[0].name.clone(),
            source: fixture.id.clone(),
            schema: "public".into(),
            object: "*".into(),
            actions: ["SELECT".into()].into_iter().collect(),
        }];
        config.validate()?;
        let config = Arc::new(config);
        let metrics = Arc::new(EngineMetrics::default());
        let status = Arc::new(ListenerStatus::default());
        let management_status = Arc::new(fedengine_management::ManagementListenerStatus::default());
        let mut selection = ComponentSelection::new();
        selection.enable_module("source-manager");
        if config.management.enabled {
            selection.enable_initiator("management-api");
        }
        for name in [
            "duckdb-instance",
            "datasource-registry",
            "duckdb-runtime",
            "pgwire-listener",
        ] {
            selection.enable_initiator(name);
        }
        for name in [
            "catalog-manager",
            "policy-manager",
            "query-coordinator",
            "protocol-catalog-fixture",
        ] {
            selection.enable_module(name);
        }
        let mut builder = ApplicationBuilder::new("protocol-tests")
            .selection(selection)
            .execution_resources(ExecutionResources {
                blocking_io_threads: 4,
                blocking_io_capacity: 32,
                max_tasks_per_owner: 512,
                ..Default::default()
            });
        builder
            .register_initiator(fedengine_duckdb::DuckdbInstanceFactory {
                config: config.clone(),
            })?
            .register_initiator(fedengine_datasource::DataSourceFactory {
                definitions: vec![fixture.clone()],
                managed: false,
            })?
            .register_initiator(fedengine_duckdb::DuckdbFactory {
                config: config.clone(),
                metrics: metrics.clone(),
            })?
            .register_initiator(PgwireFactory {
                config: config.clone(),
                metrics: metrics.clone(),
                status: status.clone(),
            })?
            .register_module(fedengine_catalog::CatalogFactory { managed: true })?
            .register_module(CatalogFixture)?
            .register_module(fedengine_policy::PolicyFactory {
                config: config.clone(),
            })?
            .register_module(fedengine_query::QueryFactory {
                config: config.clone(),
            })?
            .register_module(fedengine_management::SourceManagerFactory {
                config: config.clone(),
            })?
            .register_initiator(fedengine_management::ManagementApiFactory {
                config,
                status: management_status.clone(),
            })?;
        let handle = builder
            .build()?
            .start()
            .await
            .map_err(|error| error.source)?;
        Ok(Self {
            handle,
            metrics,
            status,
            management_status,
        })
    }
    #[allow(dead_code)] // Used by the management integration binary.
    pub fn management_address(&self) -> Option<SocketAddr> {
        self.management_status.address()
    }
    pub fn address(&self) -> Option<SocketAddr> {
        self.status.address()
    }
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }
    pub async fn shutdown_until_drained(&self) -> FrameworkResult<ShutdownReceipt> {
        loop {
            match self.shutdown().await {
                Err(FrameworkError::StopIncomplete { .. }) => tokio::task::yield_now().await,
                result => return result,
            }
        }
    }
    pub async fn shutdown(&self) -> FrameworkResult<ShutdownReceipt> {
        self.handle
            .shutdown_checked(ShutdownOptions {
                timeout: Duration::from_secs(30),
            })
            .await
    }
}
/// Publish an empty, test-owned catalog through the normal storage port after
/// runtime startup, so real snapshot leases and catalog queries are exercised.
struct CatalogFixture;
fn fixture_descriptor() -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::module("protocol-catalog-fixture"));
    descriptor.requires.extend([
        ComponentRef::initiator("duckdb-instance"),
        ComponentRef::initiator("duckdb-runtime"),
    ]);
    descriptor
}
impl ModuleFactory for CatalogFixture {
    fn descriptor(&self) -> ComponentDescriptor {
        fixture_descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(Self))
    }
}
#[async_trait]
impl Lifecycle for CatalogFixture {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let storage: ServiceHandle<ManagementStorage> = ctx
            .services
            .get(&ComponentRef::initiator("duckdb-instance"), "storage")?;
        let incarnation = storage
            .acquire()?
            .port
            .read()
            .await
            .map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?
            .source(&DataSourceId("protocol_fixture".into()))
            .map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?
            .incarnation
            .clone();
        let prepared = storage
            .acquire()?
            .port
            .prepare_publication(
                incarnation,
                Box::new(|state| {
                    let source = state
                        .sources
                        .values_mut()
                        .find(|s| s.id.0 == "protocol_fixture")
                        .unwrap();
                    source.status = SourceStatus::Active;
                    source.applied_revision = Some(source.desired_revision);
                    source.catalog_snapshot = Some("protocol-snapshot".into());
                    // Retain the disabled intent: this fixture never reconnects to a
                    // remote database during persistent management-test recovery.
                    let snapshot = CatalogSnapshot {
                        id: "protocol-snapshot".into(),
                        incarnation: source.incarnation.clone(),
                        revision: source.desired_revision,
                        generation: 1,
                        database_oid: 0,
                        schema_oids: Default::default(),
                        refreshed_at: "fixture".into(),
                        complete: true,
                        stale: false,
                        structure_digest: "fixture".into(),
                        schemas: vec!["public".into()],
                        objects: vec![],
                    };
                    state.catalogs.insert(snapshot.id.clone(), snapshot);
                    Ok(())
                }),
                Box::new(storage.acquire()?),
            )
            .await
            .map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?;
        prepared
            .commit(Box::new(()), Box::new(|| {}))
            .await
            .map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?;
        Ok(())
    }
}
impl Module for CatalogFixture {
    fn descriptor(&self) -> ComponentDescriptor {
        fixture_descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(None)
    }
}
