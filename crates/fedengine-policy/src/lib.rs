//! Default-deny object authorization and versioned DDL capability decisions.
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;
use fedengine_ddl::{Analysis, Kind};
use modorch::{prelude::*, *};
use std::sync::Arc;
pub const POLICY_TOPIC: &str = "policy/authorize/v1";

/// Metadata is visible only through existing SELECT grants and the effective registry.
pub fn metadata_scope(
    config: &Config,
    principal: &str,
    database: &str,
    headers: impl Into<fedengine_contract::CatalogHeads>,
    effective: &[DataSourceDefinition],
) -> Result<fedengine_contract::MetadataScope> {
    session_metadata_scope(
        config,
        principal,
        &fedengine_contract::SessionContext {
            database: database.into(),
            ..Default::default()
        },
        headers,
        effective,
    )
}
/// Existing sessions retain CONNECT admission but must keep their original database identity.
pub fn session_metadata_scope(
    config: &Config,
    principal: &str,
    session: &fedengine_contract::SessionContext,
    headers: impl Into<fedengine_contract::CatalogHeads>,
    effective: &[DataSourceDefinition],
) -> Result<fedengine_contract::MetadataScope> {
    let database = session.database.as_str();
    use fedengine_contract::{MetadataScope, VisibleCatalog};
    if !config.users.iter().any(|u| u.name == principal) {
        return Err(EngineError::denied("unknown principal"));
    }
    let headers = headers.into();
    let can_connect = |db: &VirtualDatabase| {
        db.config.owner == principal || db.config.connect.iter().any(|u| u == principal)
    };
    let virtual_database = headers
        .databases
        .iter()
        .find(|d| d.config.name == database)
        .cloned();
    if let Some(oid) = session.database_oid {
        let current = virtual_database.as_ref().map(|d| d.oid).or_else(|| {
            headers
                .sources
                .iter()
                .find(|h| !h.source.deleted && h.source.catalog_alias == database)
                .and_then(|h| h.snapshot.as_ref())
                .map(|s| s.database_oid)
        });
        if current != Some(oid) {
            return Err(EngineError::new(
                "3D000",
                "connected database was removed or replaced; reconnect",
            ));
        }
    }
    if let Some(db) = &virtual_database {
        if session.database_oid.is_none() && !can_connect(db) {
            return Err(EngineError::denied("permission denied for database"));
        }
        if session.database_oid.is_none() && !db.config.allow_connections {
            return Err(EngineError::new(
                "55000",
                "database is not currently accepting connections",
            ));
        }
    }
    let mut catalogs = Vec::new();
    for header in &headers.sources {
        let source = &header.source;
        if source.deleted
            || !matches!(
                source.status,
                SourceStatus::Active | SourceStatus::MetadataStale
            )
        {
            continue;
        }
        let Some(definition) = effective.iter().find(|d| d.id == source.id && d.enabled) else {
            continue;
        };
        let Some(snapshot) = &header.snapshot else {
            continue;
        };
        let grants: Vec<_> = config
            .grants
            .iter()
            .filter(|g| {
                g.principal == principal
                    && g.source == source.id
                    && g.actions.contains("SELECT")
                    && (g.schema == "*" || definition.allowed_schemas.contains(&g.schema))
            })
            .cloned()
            .collect();
        if grants.is_empty() {
            continue;
        }
        catalogs.push(VisibleCatalog {
            id: source.id.clone(),
            catalog: source.catalog_alias.clone(),
            snapshot_id: snapshot.id.clone(),
            database_oid: snapshot.database_oid,
            schemas: snapshot.schemas.clone(),
            default_schema: definition.default_schema.clone(),
            grants,
            stale: snapshot.stale || source.status == SourceStatus::MetadataStale,
            mapping: None,
        });
    }
    if let Some(db) = &virtual_database {
        let direct = catalogs.clone();
        for binding in &db.schemas {
            let Some(source) = direct.iter().find(|c| c.id == binding.config.source) else {
                continue;
            };
            let Some(snapshot_id) = &binding.snapshot_id else {
                continue;
            };
            if !source.schemas.contains(&binding.config.remote_schema) {
                continue;
            }
            let grants = source
                .grants
                .iter()
                .filter(|g| g.schema == "*" || g.schema == binding.config.remote_schema)
                .map(|g| {
                    let mut g = g.clone();
                    g.schema = binding.config.name.clone();
                    g
                })
                .collect::<Vec<_>>();
            if grants.is_empty() {
                continue;
            }
            catalogs.push(VisibleCatalog {
                id: source.id.clone(),
                catalog: db.config.name.clone(),
                snapshot_id: snapshot_id.clone(),
                database_oid: db.oid,
                schemas: vec![binding.config.name.clone()],
                default_schema: String::new(),
                grants,
                stale: source.stale,
                mapping: Some(binding.config.clone()),
            });
        }
    } else if !catalogs.iter().any(|c| c.catalog == database) {
        if let Some(h) = headers
            .sources
            .iter()
            .find(|h| !h.source.deleted && h.source.catalog_alias == database)
        {
            if !matches!(
                h.source.status,
                SourceStatus::Active | SourceStatus::MetadataStale
            ) {
                return Err(EngineError::new(
                    "55000",
                    "database is not currently accepting connections",
                ));
            }
            return Err(EngineError::denied("permission denied for database"));
        }
        return Err(EngineError::new(
            "3D000",
            format!("database {} does not exist", quote_identifier(database)),
        ));
    }
    Ok(MetadataScope {
        database: database.into(),
        principal: principal.into(),
        catalogs,
        virtual_database,
        databases: headers
            .databases
            .into_iter()
            .filter(|d| can_connect(d) || session.database_oid == Some(d.oid))
            .collect(),
    })
}
#[derive(Clone)]
pub struct Authorize {
    pub principal: String,
    pub analysis: Analysis,
}
pub fn authorize(config: &Config, request: &Authorize) -> Result<()> {
    let a = &request.analysis;
    if !config.users.iter().any(|u| u.name == request.principal) {
        return Err(EngineError::denied("unknown principal"));
    }
    for object in &a.objects {
        let source = config
            .datasources
            .iter()
            .find(|s| s.id == object.source && s.enabled)
            .ok_or_else(|| EngineError::denied("disabled source"))?;
        if a.kind != Kind::Query && source.read_only {
            return Err(EngineError::denied("data source is read-only"));
        }
        let action = if a.kind == Kind::Query
            || (a.kind == Kind::Ddl && a.primary.as_ref().is_some_and(|p| p != object))
        {
            "SELECT"
        } else {
            &a.action
        };
        let allowed = config.grants.iter().any(|g| {
            g.principal == request.principal
                && g.source == object.source
                && (g.schema == "*" || g.schema == object.schema)
                && (g.object == "*" || g.object == object.name)
                && g.actions.contains(action)
        });
        if !allowed {
            return Err(EngineError::denied("object action is not granted"));
        }
    }
    if a.kind == Kind::Ddl {
        if !config.ddl.enabled || a.sources.len() != 1 {
            return Err(EngineError::denied("DDL disabled or not single-source"));
        }
        let source = &a.sources.iter().next().unwrap();
        let s = config
            .datasources
            .iter()
            .find(|s| &s.id == *source)
            .unwrap();
        if !s.ddl_enabled {
            return Err(EngineError::denied("source does not allow DDL"));
        }
        if a.action == "DROP_SCHEMA" && !config.ddl.allow_drop_schema {
            return Err(EngineError::denied("DROP SCHEMA is disabled"));
        }
        let capability = config.capabilities.iter().find(|c| &c.source == *source);
        if !capability.is_some_and(|c| {
            c.operations.contains(&a.action) && c.operations.contains(&a.capability)
        }) {
            return Err(EngineError::unsupported(
                "DDL operation has no verified version capability record",
            ));
        }
        if a.index.is_some() && !capability.unwrap().index_adapter {
            return Err(EngineError::unsupported(
                "remote index adapter has not been verified",
            ));
        }
    }
    Ok(())
}
pub struct PolicyFactory {
    pub config: Arc<Config>,
}
struct PolicyModule {
    config: Arc<Config>,
    registry: Option<ServiceHandle<DataSourceRegistry>>,
}
impl ModuleFactory for PolicyFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut d = ComponentDescriptor::new(ComponentRef::module("policy-manager"));
        d.requires
            .push(ComponentRef::initiator("datasource-registry"));
        d
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(PolicyModule {
            config: self.config.clone(),
            registry: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for PolicyModule {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.registry = Some(
            ctx.services
                .get(&ComponentRef::initiator("datasource-registry"), "registry")?,
        );
        Ok(())
    }
}
impl Module for PolicyModule {
    fn descriptor(&self) -> ComponentDescriptor {
        PolicyFactory {
            config: self.config.clone(),
        }
        .descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Serial {
            handler: Box::new(PolicyHandler {
                config: self.config.clone(),
                registry: self.registry.clone().unwrap(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                20,
                self.descriptor().component(),
                POLICY_TOPIC,
            )],
        }))
    }
}
struct PolicyHandler {
    config: Arc<Config>,
    registry: ServiceHandle<DataSourceRegistry>,
}
#[async_trait]
impl SerialHandler for PolicyHandler {
    async fn handle(
        &mut self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let r = message.payload::<Authorize>()?;
        let mut effective = (*self.config).clone();
        effective.datasources = self.registry.acquire()?.definitions();
        Ok(Arc::new(authorize(&effective, &r)))
    }
}
