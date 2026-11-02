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
}
impl FedEngine {
    pub async fn start(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let config = Arc::new(config);
        let metrics = Arc::new(EngineMetrics::default());
        let status = Arc::new(ListenerStatus::default());
        let mut selection = ComponentSelection::new();
        for name in ["datasource-registry", "duckdb-runtime", "pgwire-listener"] {
            selection.enable_initiator(name);
        }
        for name in ["catalog-manager", "policy-manager", "query-coordinator"] {
            selection.enable_module(name);
        }
        let resources = ExecutionResources {
            blocking_io_threads: config.duckdb.max_workers + 2,
            blocking_io_capacity: config.duckdb.max_workers * 2 + 8,
            max_tasks_per_owner: config.pgwire.max_connections + config.duckdb.max_workers * 4 + 32,
            ..Default::default()
        };
        let mut builder = ApplicationBuilder::new(&config.application.name)
            .selection(selection)
            .execution_resources(resources)
            .queue_capacities(64, 64);
        builder
            .register_initiator(fedengine_datasource::DataSourceFactory {
                definitions: config.datasources.clone(),
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
            .register_module(fedengine_catalog::CatalogFactory)?
            .register_module(fedengine_policy::PolicyFactory {
                config: config.clone(),
            })?
            .register_module(fedengine_query::QueryFactory {
                config: config.clone(),
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
        })
    }
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }
    pub fn address(&self) -> Option<SocketAddr> {
        self.status.address()
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
