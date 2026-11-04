//! Explicit Modorch composition and checked application shutdown.
use fedengine_contract::{EngineMetrics, MetricsSnapshot};
use fedengine_core::Config;
use fedengine_pgwire::{ListenerStatus, PgwireFactory};
use modorch::{prelude::*, *};
use std::{net::SocketAddr, sync::Arc, time::Duration};
pub struct FedEngine {
    metrics: Arc<EngineMetrics>,
    pub handle: ApplicationHandle,
    status: Arc<ListenerStatus>,
    grace: Duration,
    management_status: Arc<fedengine_management::ManagementListenerStatus>,
}
impl FedEngine {
    pub async fn start(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let config = Arc::new(config);
        let metrics = Arc::new(EngineMetrics::default());
        let status = Arc::new(ListenerStatus::default());
        let management_status = Arc::new(fedengine_management::ManagementListenerStatus::default());
        let mut selection = ComponentSelection::new();
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
            "source-manager",
        ] {
            selection.enable_module(name);
        }
        if config.management.enabled {
            selection.enable_initiator("management-api");
        }
        let management_workers = config.management.max_concurrent_operations;
        let management_queue = config.management.queue_capacity;
        let resources = ExecutionResources {
            blocking_io_threads: config.duckdb.max_workers + management_workers + 2,
            blocking_io_capacity: config.duckdb.max_workers * 2 + management_queue + 8,
            max_tasks_per_owner: config.pgwire.max_connections
                + config.duckdb.max_workers * 4
                + management_queue
                + 32,
            ..Default::default()
        };
        let mut builder = ApplicationBuilder::new(&config.application.name)
            .selection(selection)
            .execution_resources(resources)
            .queue_capacities(64, 64);
        builder
            .register_initiator(fedengine_duckdb::DuckdbInstanceFactory {
                config: config.clone(),
            })?
            .register_initiator(fedengine_datasource::DataSourceFactory {
                definitions: config.datasources.clone(),
                managed: config.management.enabled,
            })?
            .register_initiator(fedengine_duckdb::DuckdbFactory {
                metrics: metrics.clone(),
                config: config.clone(),
            })?
            .register_initiator(PgwireFactory {
                metrics: metrics.clone(),
                config: config.clone(),
                status: status.clone(),
            })?
            .register_module(fedengine_catalog::CatalogFactory { managed: true })?
            .register_module(fedengine_policy::PolicyFactory {
                config: config.clone(),
            })?
            .register_module(fedengine_query::QueryFactory {
                config: config.clone(),
            })?;
        builder
            .register_module(fedengine_management::SourceManagerFactory {
                config: config.clone(),
            })?
            .register_initiator(fedengine_management::ManagementApiFactory {
                config: config.clone(),
                status: management_status.clone(),
            })?;
        let grace = Duration::from_secs(config.shutdown.grace_period_secs);
        let handle = match builder.build()?.start().await {
            Ok(handle) => handle,
            Err(error) => {
                tracing::error!(cause=%error.source,cleanup=?error.cleanup,"startup failed");
                // Preserve both the original failure and compensation result; retry incomplete cleanup.
                if error.cleanup.is_err() {
                    let retry = drain(&error.handle, grace).await;
                    tracing::error!(cleanup=?retry,"startup cleanup completion");
                }
                return Err(Box::new(error));
            }
        };
        if !handle.health().ready {
            drain(&handle, grace).await?;
            return Err("application did not become ready".into());
        }
        Ok(Self {
            metrics,
            handle,
            status,
            grace,
            management_status,
        })
    }
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }
    pub fn address(&self) -> Option<SocketAddr> {
        self.status.address()
    }
    pub fn management_address(&self) -> Option<SocketAddr> {
        self.management_status.address()
    }
    /// Retain the application and retry while owned work is still draining.
    pub async fn shutdown_until_drained(&self) -> FrameworkResult<ShutdownReceipt> {
        drain(&self.handle, self.grace).await
    }
    pub async fn shutdown(&self) -> FrameworkResult<ShutdownReceipt> {
        self.handle
            .shutdown_checked(ShutdownOptions {
                timeout: self.grace,
            })
            .await
    }
}

async fn drain(handle: &ApplicationHandle, timeout: Duration) -> FrameworkResult<ShutdownReceipt> {
    loop {
        match handle.shutdown_checked(ShutdownOptions { timeout }).await {
            Err(error @ FrameworkError::StopIncomplete { .. }) => {
                tracing::error!(%error, "shutdown incomplete; retaining resources and waiting for actual exit");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            result => return result,
        }
    }
}
