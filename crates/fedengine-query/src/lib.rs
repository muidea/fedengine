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
        let registry = self.registry.acquire().map_err(message_error)?;
        // Instance lease precedes source fences and is transferred to the actual worker.
        let instance = tokio::time::timeout(remaining(r)?, registry.instance.clone().read_owned())
            .await
            .map_err(|_| EngineError::new("55P03", "instance execution barrier timed out"))?;
        let definitions = registry.definitions();
        let headers = ctx
            .event_hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                CatalogRequest::Headers,
                Default::default(),
            )
            .map_err(message_error)?
            .wait(remaining(r)?)
            .await
            .map_err(message_error)?;
        let headers = match headers.as_ref() {
            Ok(CatalogReply::Headers(headers)) => headers.clone(),
            Err(error) => return Err(error.clone()),
            _ => return Err(EngineError::new("XX000", "invalid catalog headers reply")),
        };
        let scope = fedengine_policy::session_metadata_scope(
            &self.config,
            &r.principal,
            &r.session,
            headers,
            &definitions,
        )?;
        let database_version = scope.virtual_database.as_ref().map(|d| DatabaseVersion {
            oid: d.oid,
            revision: d.revision,
        });
        if let QueryMode::Execute {
            expected_database: Some(expected),
            ..
        } = &r.mode
            && Some(*expected) != database_version
        {
            return Err(EngineError::new(
                "0A000",
                "virtual database definition changed; Parse/Bind the statement again",
            ));
        }
        let declared = match &r.mode {
            QueryMode::Prepare { parameter_types } => parameter_types
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(i, t)| t.map(|t| (i + 1, t)))
                .collect(),
            QueryMode::Execute {
                parameter_types, ..
            } => parameter_types
                .iter()
                .copied()
                .enumerate()
                .map(|(i, t)| (i + 1, t))
                .collect(),
        };
        let business_sql = match fedengine_pg_compat::plan_with_parameters(
            &r.sql,
            &r.session,
            &scope,
            r.session_id,
            &declared,
        )? {
            fedengine_pg_compat::CompatibilityPlan::Business(sql) => sql,
            fedengine_pg_compat::CompatibilityPlan::Metadata(plan) => {
                let generations = registry.generations(scope.catalogs.iter().map(|c| c.id.clone()));
                let (prepare_only, parameters, wire_types) =
                    query_parameters(r, &r.sql, &generations, &plan.parameter_types)?;
                // Compatibility lowering may erase arguments, but Parse/Bind still
                // describe and validate the complete original client contract.
                let wire_types = wire_types
                    .into_iter()
                    .map(|t| {
                        t.ok_or_else(|| {
                            EngineError::new("42P18", "parameter type cannot be determined")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let (sql, slots) = fedengine_ddl::compact_parameters(&plan.sql)?;
                let parameter_types = slots
                    .iter()
                    .map(|i| Some(wire_types[*i]))
                    .collect::<Vec<_>>();
                let parameters = if prepare_only {
                    vec![]
                } else {
                    slots.iter().map(|i| parameters[*i].clone()).collect()
                };
                let sql = typed_sql(&sql, &parameter_types)?;
                let notices=scope.catalogs.iter().filter(|c|c.stale).take(8).map(|c|(c.snapshot_id.clone(),format!("catalog {} uses a stale metadata snapshot; inspect the management API catalog status",c.catalog))).collect::<Vec<_>>();
                let runtime = self.runtime.acquire().map_err(message_error)?;
                let mut reply = runtime
                    .port
                    .submit(
                        ExecuteRequest {
                            metadata: Some(MetadataExecution {
                                search_path: fedengine_pg_compat::catalog_path(&r.session, &scope),
                                scope,
                                columns: plan.columns,
                            }),
                            principal: r.principal.clone(),
                            session_id: r.session_id,
                            sql,
                            kind: StatementKind::Query,
                            parameters,
                            parameter_types,
                            prepare_only,
                            generations,
                            deadline: r.deadline,
                            cancel: r.cancel.clone(),
                            ddl_source: None,
                            command: plan.command,
                        },
                        Box::new((
                            self.runtime.acquire().map_err(message_error)?,
                            registry,
                            instance,
                        )),
                    )
                    .await?;
                if let RuntimeReply::Accepted(a) = &mut reply {
                    a.notices = notices;
                }
                if let RuntimeReply::Prepared(d) = &mut reply {
                    d.parameters = wire_types;
                }
                return Ok(query_reply(reply, r, database_version));
            }
        };
        let mut analyzer = Analyzer {
            namespace: None,
            sources: definitions.clone(),
            indexes: vec![],
        };
        let mut relations = std::collections::BTreeSet::new();
        let mut bindings = std::collections::BTreeMap::new();
        for catalog in scope
            .catalogs
            .iter()
            .filter(|c| c.catalog == r.session.database)
        {
            let names = ctx
                .event_hub
                .request::<_, Result<CatalogReply>>(
                    CATALOG_TOPIC,
                    Destination::Component(ComponentRef::module("catalog-manager")),
                    CatalogRequest::Names(catalog.snapshot_id.clone()),
                    Default::default(),
                )
                .map_err(message_error)?
                .wait(remaining(r)?)
                .await
                .map_err(message_error)?;
            match names.as_ref() {
                Ok(CatalogReply::Names(names)) => relations.extend(names.clone()),
                Err(e) => return Err(e.clone()),
                _ => return Err(EngineError::new("XX000", "invalid catalog names reply")),
            }
            let schemas = if catalog.mapping.is_none() {
                definitions
                    .iter()
                    .find(|d| d.id == catalog.id)
                    .map(|d| d.allowed_schemas.as_slice())
                    .unwrap_or(&[])
            } else {
                &catalog.schemas
            };
            for schema in schemas {
                let remote = catalog
                    .mapping
                    .as_ref()
                    .map(|m| m.remote_schema.clone())
                    .unwrap_or_else(|| schema.clone());
                bindings.insert(schema.clone(), (catalog.id.clone(), remote));
            }
        }
        analyzer.namespace = Some(fedengine_ddl::BindingNamespace {
            catalog: r.session.database.clone(),
            allow_cross_database: self.config.pgwire.allow_cross_database,
            search_path: fedengine_pg_compat::effective_path(&r.session, &scope),
            relations,
            bindings,
        });
        // Inspect the actual AST to discover DROP INDEX ownership; never trust textual prefixes.
        let statements = Analyzer::parse(&business_sql)?;
        if statements.len() != 1 {
            return Err(EngineError::unsupported("one SQL statement is required"));
        }
        if let Some(name) = drop_index_name(&business_sql)? {
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
        let analysis = analyzer.analyze(&business_sql)?;
        if scope.virtual_database.is_some() && analysis.kind != Kind::Query {
            return Err(EngineError::unsupported(
                "virtual database bindings support read-only queries",
            ));
        }
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
        let analyzed_versions = registry.generations(analysis.sources.iter().cloned());
        let ddl = analysis.kind == Kind::Ddl;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        // BTreeSet provides a fixed source lock order for joins and DDL.
        for id in &analysis.sources {
            let source = registry.get(id)?;
            if source.stale.load(Ordering::SeqCst) {
                return Err(EngineError::new(
                    "55000",
                    "source metadata is stale; refresh it online",
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
        registry.check(&analyzed_versions)?;
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
        let (prepare, parameters, wire_types) =
            query_parameters(r, &r.sql, &generations, &Default::default())?;
        let wire_types = wire_types
            .into_iter()
            .map(|t| {
                t.ok_or_else(|| EngineError::new("42P18", "parameter type cannot be determined"))
            })
            .collect::<Result<Vec<_>>>()?;
        let sql = if let Some(index) = &analysis.index {
            let source = definitions
                .iter()
                .find(|s| s.id == index.table.source)
                .unwrap();
            index.duckdb_adapter_sql(source.kind)?
        } else {
            analysis.sql
        };
        let (sql, slots) = fedengine_ddl::compact_parameters(&sql)?;
        let parameter_types = slots
            .iter()
            .map(|i| Some(wire_types[*i]))
            .collect::<Vec<_>>();
        let parameters = if prepare {
            vec![]
        } else {
            slots.iter().map(|i| parameters[*i].clone()).collect()
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
            instance,
            reads,
            writes,
        ));
        let reply = runtime
            .port
            .submit(
                ExecuteRequest {
                    metadata: None,
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
                descriptor.parameters = wire_types;
                descriptor.sql = r.sql.clone();
                descriptor.namespace_version = r.session.namespace_version;
                descriptor.database_version = database_version;
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

fn query_reply(
    reply: RuntimeReply,
    request: &QueryRequest,
    database_version: Option<DatabaseVersion>,
) -> QueryReply {
    match reply {
        RuntimeReply::Prepared(mut descriptor) => {
            descriptor.sql = request.sql.clone();
            descriptor.namespace_version = request.session.namespace_version;
            descriptor.database_version = database_version;
            QueryReply::Prepared(descriptor)
        }
        RuntimeReply::Accepted(a) => QueryReply::Accepted(a),
    }
}
fn query_parameters(
    r: &QueryRequest,
    sql: &str,
    generations: &std::collections::BTreeMap<DataSourceId, SourceVersion>,
    signatures: &std::collections::BTreeMap<usize, PgType>,
) -> Result<(bool, Vec<Cell>, Vec<Option<PgType>>)> {
    match &r.mode {
        QueryMode::Prepare { parameter_types } => {
            let mut hints = fedengine_ddl::parameter_hints(sql)?;
            if parameter_types.len() > 1024 {
                return Err(EngineError::new("54000", "parameter limit exceeded"));
            }
            hints.resize(hints.len().max(parameter_types.len()), None);
            Ok((
                true,
                vec![],
                hints
                    .into_iter()
                    .enumerate()
                    .map(|(i, h)| {
                        parameter_types
                            .get(i)
                            .copied()
                            .flatten()
                            .or_else(|| signatures.get(&(i + 1)).copied())
                            .or_else(|| h.and_then(|t| hint_type(&t)))
                    })
                    .collect(),
            ))
        }
        QueryMode::Execute {
            parameters,
            parameter_types,
            expected,
            ..
        } => {
            if parameters.len() != parameter_types.len()
                || parameters.len() < fedengine_ddl::parameter_hints(sql)?.len()
            {
                return Err(EngineError::new("08P01", "Bind parameter count mismatch"));
            }
            for (value, typ) in parameters.iter().zip(parameter_types) {
                if let Cell::Array { lower_bound, .. } = value
                    && *lower_bound != 1
                    && !matches!(typ, PgType::Catalog(22 | 30))
                {
                    return Err(EngineError::unsupported(
                        "execution of arrays with non-unit lower bounds is not implemented",
                    ));
                }
            }
            if !expected.is_empty() && expected != generations {
                return Err(EngineError::new(
                    "0A000",
                    "catalog generation changed; Parse/Bind the statement again",
                ));
            }
            Ok((
                false,
                parameters.clone(),
                parameter_types.iter().copied().map(Some).collect(),
            ))
        }
    }
}
fn typed_sql(sql: &str, types: &[Option<PgType>]) -> Result<String> {
    let hints = fedengine_ddl::parameter_hints(sql)?;
    let types = types
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if hints.get(i).is_some_and(Option::is_some) {
                return Ok("SKIP".into());
            }
            p.map(|t| t.duckdb_name().into()).ok_or_else(|| {
                EngineError::new(
                    "42P18",
                    "parameter type requires an OID or explicit SQL cast",
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if types.is_empty() {
        Ok(sql.into())
    } else {
        fedengine_ddl::cast_parameters(sql, &types)
    }
}
