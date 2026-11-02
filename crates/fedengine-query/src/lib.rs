//! SQL coordinator: analysis → binding → policy → source fences → DuckDB execution.
use fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use fedengine_ddl::{Analyzer, Kind, drop_index_name};
use fedengine_policy::{Authorize, POLICY_TOPIC};
use modorch::{prelude::*, *};
use std::{
    num::NonZeroUsize,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

pub struct QueryFactory {
    pub config: Arc<Config>,
}
struct QueryModule {
    config: Arc<Config>,
    registry: Option<ServiceHandle<DataSourceRegistry>>,
    runtime: Option<ServiceHandle<ExecutionService>>,
}
fn descriptor(max_workers: usize) -> ComponentDescriptor {
    let mut d = ComponentDescriptor::new(ComponentRef::module("query-coordinator"));
    d.execution = ExecutionPolicy::Concurrent {
        max_in_flight: NonZeroUsize::new(max_workers * 2).unwrap(),
    };
    d.requires.extend([
        ComponentRef::initiator("duckdb-runtime"),
        ComponentRef::initiator("datasource-registry"),
        ComponentRef::module("catalog-manager"),
        ComponentRef::module("policy-manager"),
    ]);
    d
}
impl ModuleFactory for QueryFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.config.duckdb.max_workers)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(QueryModule {
            config: self.config.clone(),
            registry: None,
            runtime: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for QueryModule {
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
impl Module for QueryModule {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.config.duckdb.max_workers)
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Concurrent {
            max_in_flight: NonZeroUsize::new(self.config.duckdb.max_workers * 2).unwrap(),
            handler: Arc::new(QueryHandler {
                config: self.config.clone(),
                registry: self.registry.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                30,
                descriptor(self.config.duckdb.max_workers).component(),
                QUERY_TOPIC,
            )],
        }))
    }
}
struct QueryHandler {
    config: Arc<Config>,
    registry: ServiceHandle<DataSourceRegistry>,
    runtime: ServiceHandle<ExecutionService>,
}
fn message_error(e: FrameworkError) -> EngineError {
    EngineError::new("53300", format!("control request failed: {e}"))
}
#[async_trait]
impl ConcurrentHandler for QueryHandler {
    async fn handle(
        &self,
        ctx: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let request = message.payload::<QueryRequest>()?;
        let result = self.execute(ctx, &request).await;
        Ok(Arc::new(result))
    }
}
impl QueryHandler {
    async fn execute(&self, ctx: &HandlerContext, r: &QueryRequest) -> Result<QueryReply> {
        if r.cancel.is_cancelled() {
            return Err(EngineError::new("57014", "query canceled before admission"));
        }
        let mut analyzer = Analyzer {
            sources: self.config.datasources.clone(),
            indexes: vec![],
        };
        // Inspect the actual AST to discover DROP INDEX ownership; never trust textual prefixes.
        let statements = Analyzer::parse(&r.sql)?;
        if statements.len() != 1 {
            return Err(EngineError::unsupported("one SQL statement is required"));
        }
        if let Some(name) = drop_index_name(&r.sql)? {
            let source = analyzer.bind(&name, false)?.source;
            if !self.config.grants.iter().any(|g| {
                g.principal == r.principal && g.source == source && g.actions.contains("DROP_INDEX")
            }) {
                return Err(EngineError::denied("DROP_INDEX is not granted"));
            }
            let response = ctx
                .event_hub
                .request::<_, Result<CatalogReply>>(
                    CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    CatalogRequest::Indexes(source),
                    Default::default(),
                )
                .map_err(message_error)?
                .wait(remaining(r)?)
                .await
                .map_err(message_error)?;
            match response.as_ref() {
                Ok(CatalogReply::Indexes(indexes)) => analyzer.indexes = indexes.clone(),
                Err(e) => return Err(e.clone()),
                _ => return Err(EngineError::new("XX000", "invalid catalog reply")),
            }
        }
        let analysis = analyzer.analyze(&r.sql)?;
        let permission = ctx
            .event_hub
            .request::<_, Result<()>>(
                POLICY_TOPIC,
                Destination::Component(ComponentRef::module("policy-manager")),
                Authorize {
                    principal: r.principal.clone(),
                    analysis: analysis.clone(),
                },
                Default::default(),
            )
            .map_err(message_error)?
            .wait(remaining(r)?)
            .await
            .map_err(message_error)?;
        permission.as_ref().clone()?;
        let registry = self.registry.acquire().map_err(message_error)?;
        let ddl = analysis.kind == Kind::Ddl;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        // BTreeSet provides a fixed source lock order for joins and DDL.
        for id in &analysis.sources {
            let source = registry.get(id)?;
            if source.stale.load(Ordering::SeqCst) {
                return Err(EngineError::new(
                    "55000",
                    "source metadata is stale; an authorized reconciliation/restart is required",
                ));
            }
            let duration =
                Duration::from_millis(self.config.ddl.source_lock_timeout_ms).min(remaining(r)?);
            if ddl {
                writes.push(
                    tokio::time::timeout(duration, source.fence.clone().write_owned())
                        .await
                        .map_err(|_| EngineError::new("55P03", "source DDL fence timed out"))?,
                );
            } else {
                reads.push(
                    tokio::time::timeout(duration, source.fence.clone().read_owned())
                        .await
                        .map_err(|_| EngineError::new("55P03", "source fence timed out"))?,
                );
            }
        }
        // State may change while awaiting the fence. Recheck under the acquired locks.
        for id in &analysis.sources {
            if registry.get(id)?.stale.load(Ordering::SeqCst) {
                return Err(EngineError::new(
                    "55000",
                    "source metadata became stale while awaiting its fence",
                ));
            }
        }
        if let Some(index) = &analysis.index
            && !index.create
        {
            let runtime = self.runtime.acquire().map_err(message_error)?;
            let current = runtime
                .port
                .indexes(
                    index.table.source.clone(),
                    Box::new(self.runtime.acquire().map_err(message_error)?),
                )
                .await?;
            let owners: Vec<_> = current
                .iter()
                .filter(|i| i.schema == index.table.schema && i.index == index.name)
                .collect();
            if owners.len() != 1 || owners[0].table != index.table.name || owners[0].protected {
                return Err(EngineError::denied(
                    "index ownership changed while awaiting the DDL fence",
                ));
            }
        }
        let generations = registry.generations(analysis.sources.iter().cloned());
        let (prepare, parameters, parameter_types): (bool, Vec<Cell>, Vec<Option<PgType>>) =
            match &r.mode {
                QueryMode::Prepare { parameter_types } => {
                    let hints = fedengine_ddl::parameter_hints(&analysis.sql)?;
                    if parameter_types.len() > hints.len() {
                        return Err(EngineError::new("08P01", "too many parameter type OIDs"));
                    }
                    let resolved: Vec<Option<PgType>> = hints
                        .into_iter()
                        .enumerate()
                        .map(|(i, h)| {
                            parameter_types
                                .get(i)
                                .copied()
                                .flatten()
                                .or_else(|| h.and_then(|t| hint_type(&t)))
                        })
                        .collect();
                    (true, vec![], resolved)
                }
                QueryMode::Execute {
                    parameters,
                    parameter_types,
                    expected,
                } => {
                    if !expected.is_empty() && *expected != generations {
                        return Err(EngineError::new(
                            "0A000",
                            "catalog generation changed; Parse/Bind the statement again",
                        ));
                    }
                    (
                        false,
                        parameters.clone(),
                        parameter_types.iter().map(|t| Some(*t)).collect(),
                    )
                }
            };
        let sql = if let Some(index) = &analysis.index {
            let source = self
                .config
                .datasources
                .iter()
                .find(|s| s.id == index.table.source)
                .unwrap();
            index.duckdb_adapter_sql(source.kind)?
        } else {
            analysis.sql
        };
        let explicit_hints = fedengine_ddl::parameter_hints(&sql)?;
        let resolved_types: Vec<String> = parameter_types
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if explicit_hints.get(i).is_some_and(|h| h.is_some()) {
                    return Ok("SKIP".into());
                }
                if *p == Some(PgType::Numeric) {
                    return Err(EngineError::new(
                        "42P18",
                        "NUMERIC parameters need an explicit DECIMAL precision/scale cast",
                    ));
                }
                p.map(|t| t.duckdb_name().to_string()).ok_or_else(|| {
                    EngineError::new(
                        "42P18",
                        "parameter type requires an OID or explicit SQL cast",
                    )
                })
            })
            .collect::<Result<_>>()?;
        let sql = if resolved_types.is_empty() {
            sql
        } else {
            fedengine_ddl::cast_parameters(&sql, &resolved_types)?
        };
        let kind = match analysis.kind {
            Kind::Query => StatementKind::Query,
            Kind::Insert => StatementKind::Insert,
            Kind::Update => StatementKind::Update,
            Kind::Delete => StatementKind::Delete,
            Kind::Ddl => StatementKind::Ddl,
        };
        let runtime = self.runtime.acquire().map_err(message_error)?;
        let ownership = Box::new((
            self.runtime.acquire().map_err(message_error)?,
            registry,
            reads,
            writes,
        ));
        let reply = runtime
            .port
            .submit(
                ExecuteRequest {
                    principal: r.principal.clone(),
                    session_id: r.session_id,
                    sql,
                    kind,
                    parameters,
                    parameter_types,
                    prepare_only: prepare,
                    generations,
                    deadline: r.deadline,
                    cancel: r.cancel.clone(),
                    ddl_source: if ddl {
                        analysis.sources.iter().next().cloned()
                    } else {
                        None
                    },
                    command: analysis.command,
                },
                ownership,
            )
            .await?;
        Ok(match reply {
            RuntimeReply::Prepared(mut descriptor) => {
                descriptor.sql = r.sql.clone();
                QueryReply::Prepared(descriptor)
            }
            RuntimeReply::Accepted(a) => QueryReply::Accepted(a),
        })
    }
}
fn remaining(r: &QueryRequest) -> Result<Duration> {
    let d = r
        .deadline
        .saturating_duration_since(std::time::Instant::now());
    if d.is_zero() {
        Err(EngineError::new("57014", "query deadline exceeded"))
    } else {
        Ok(d)
    }
}
fn hint_type(t: &str) -> Option<PgType> {
    Some(match t.to_uppercase().as_str() {
        "INT" | "INTEGER" | "INT4" => PgType::Int4,
        "BIGINT" | "INT8" => PgType::Int8,
        "SMALLINT" | "INT2" => PgType::Int2,
        "BOOLEAN" | "BOOL" => PgType::Bool,
        "REAL" | "FLOAT4" => PgType::Float4,
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => PgType::Float8,
        "TEXT" | "VARCHAR" => PgType::Text,
        "BLOB" | "BYTEA" => PgType::Bytea,
        "DATE" => PgType::Date,
        "TIME" => PgType::Time,
        "TIMESTAMP" => PgType::Timestamp,
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => PgType::Timestamptz,
        "UUID" => PgType::Uuid,
        "JSON" => PgType::Json,
        v if v.starts_with("DECIMAL") || v.starts_with("NUMERIC") => PgType::Numeric,
        _ => return None,
    })
}
