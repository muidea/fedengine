//! Configuration and domain types, independent of orchestration and protocol crates.
use serde::{Deserialize, Serialize};
mod management;
pub use management::*;
mod metadata;
pub use metadata::*;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message} (SQLSTATE {sqlstate})")]
pub struct EngineError {
    pub sqlstate: &'static str,
    pub message: String,
    pub outcome_unknown: bool,
}
impl EngineError {
    pub fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
            outcome_unknown: false,
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("0A000", message)
    }
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("42501", message)
    }
    pub fn config(message: impl Into<String>) -> Self {
        Self::new("22023", message)
    }
    pub fn unknown(mut self) -> Self {
        self.outcome_unknown = true;
        self
    }
}
pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DataSourceId(pub String);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSourceKind {
    Postgres,
    Mysql,
}
impl DataSourceKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataSourceDefinition {
    pub id: DataSourceId,
    pub kind: DataSourceKind,
    pub catalog_alias: String,
    /// Environment variable name or a file: reference under the configured credential directory.
    pub secret_ref: String,
    pub default_schema: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub ddl_enabled: bool,
    pub allowed_schemas: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub application: ApplicationConfig,
    pub pgwire: PgwireConfig,
    pub duckdb: DuckdbConfig,
    pub ddl: DdlConfig,
    pub shutdown: ShutdownConfig,
    #[serde(default)]
    pub management: ManagementConfig,
    #[serde(default)]
    pub datasources: Vec<DataSourceDefinition>,
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub virtual_databases: Vec<VirtualDatabaseConfig>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub capabilities: Vec<CapabilityRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationConfig {
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PgwireConfig {
    pub listen: String,
    pub max_connections: usize,
    pub startup_timeout_ms: u64,
    pub query_timeout_ms: u64,
    pub output_timeout_ms: u64,
    pub tls_required: bool,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub max_statements: usize,
    pub max_portals: usize,
    #[serde(default)]
    pub allow_cross_database: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DuckdbConfig {
    pub database_path: String,
    pub max_workers: usize,
    pub threads: usize,
    pub memory_limit: String,
    pub temp_directory: String,
    pub result_channel_batches: usize,
    pub batch_rows: usize,
    pub acquire_timeout_ms: u64,
    pub install_extensions: bool,
    pub extension_directory: Option<String>,
    pub pool_max_connections: usize,
    pub pool_wait_timeout_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DdlConfig {
    pub enabled: bool,
    pub allow_drop_schema: bool,
    pub source_lock_timeout_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    pub grace_period_secs: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub name: String,
    pub password_secret_ref: String,
}
/// Explicit business database; CONNECT never grants access to a remote object.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualDatabaseConfig {
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub connect: Vec<String>,
    #[serde(default = "yes")]
    pub allow_connections: bool,
    #[serde(default)]
    pub schemas: Vec<VirtualSchemaConfig>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualSchemaConfig {
    pub name: String,
    pub source: DataSourceId,
    pub remote_schema: String,
}
/// Durable logical identities and projection pointers, published with source snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VirtualDatabase {
    pub config: VirtualDatabaseConfig,
    pub oid: u32,
    pub revision: u64,
    pub public_oid: u32,
    pub schemas: Vec<VirtualSchema>,
}
impl VirtualDatabaseConfig {
    /// Shared validation for TOML reconciliation and trusted management requests.
    pub fn validate(&self, users: &BTreeSet<String>) -> Result<()> {
        if !identifier(&self.name)
            || self.name.len() > 63
            || !allowed_namespace(&self.name)
            || ["memory", "main", "temp", "system"]
                .contains(&self.name.to_ascii_lowercase().as_str())
            || !users.contains(&self.owner)
            || self.connect.len() > 1024
            || self.connect.iter().any(|u| !users.contains(u))
            || self.schemas.len() > 256
        {
            return Err(EngineError::config(
                "invalid virtual database name, owner, CONNECT principal or resource bounds",
            ));
        }
        let mut schemas = BTreeSet::from(["public".to_string()]);
        for binding in &self.schemas {
            if !identifier(&binding.name)
                || binding.name.len() > 63
                || !allowed_namespace(&binding.name)
                || !schemas.insert(binding.name.clone())
                || !allowed_namespace(&binding.remote_schema)
                || binding.remote_schema.len() > 128
                || !identifier(&binding.source.0)
                || binding.source.0.len() > 128
            {
                return Err(EngineError::config(
                    "invalid or duplicate virtual schema binding",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VirtualSchema {
    pub config: VirtualSchemaConfig,
    pub oid: u32,
    pub snapshot_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub principal: String,
    pub source: DataSourceId,
    pub schema: String,
    pub object: String,
    pub actions: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRecord {
    pub source: DataSourceId,
    pub duckdb_version: String,
    pub extension_version: String,
    pub remote_version: String,
    pub evidence: String,
    pub operations: BTreeSet<String>,
    #[serde(default)]
    pub index_adapter: bool,
}
impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| EngineError::config(e.to_string()))?;
        let config: Self = toml::from_str(&text).map_err(|e| EngineError::config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        let p = &self.pgwire;
        let d = &self.duckdb;
        if self.application.name.is_empty()
            || p.max_connections == 0
            || p.max_connections > 4096
            || p.startup_timeout_ms == 0
            || p.query_timeout_ms == 0
            || p.output_timeout_ms == 0
            || p.max_statements == 0
            || p.max_portals == 0
            || d.max_workers == 0
            || d.max_workers > 128
            || d.threads == 0
            || d.threads > 128
            || !(1..=64).contains(&d.result_channel_batches)
            || !(1..=4096).contains(&d.batch_rows)
            || d.acquire_timeout_ms == 0
            || d.pool_max_connections == 0
            || d.pool_wait_timeout_ms == 0
            || self.ddl.source_lock_timeout_ms == 0
            || self.shutdown.grace_period_secs == 0
        {
            return Err(EngineError::config("invalid resource bounds"));
        }
        p.listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| EngineError::config("listen must be an IP socket address"))?;
        if p.tls_required && (p.tls_cert.is_none() || p.tls_key.is_none()) {
            return Err(EngineError::config("TLS requires both certificate and key"));
        }
        if p.tls_cert.is_some() != p.tls_key.is_some() {
            return Err(EngineError::config(
                "TLS certificate/key must be configured together",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut aliases = BTreeSet::new();
        for s in &self.datasources {
            validate_source(s)?;
            if !ids.insert(s.id.clone()) || !aliases.insert(s.catalog_alias.to_lowercase()) {
                return Err(EngineError::config("duplicate source id or catalog alias"));
            }
        }
        let m = &self.management;
        if m.enabled {
            if d.database_path.is_empty() || d.database_path == ":memory:" {
                return Err(EngineError::config(
                    "management requires a file-backed duckdb.database_path",
                ));
            }
            m.listen.parse::<std::net::SocketAddr>().map_err(|_| {
                EngineError::config("management.listen must be an IP socket address")
            })?;
            if !(1..=128).contains(&m.max_concurrent_operations)
                || !(1..=4096).contains(&m.queue_capacity)
                || m.drain_timeout_ms == 0
                || m.request_timeout_ms == 0
                || m.credentials_directory.is_empty()
            {
                return Err(EngineError::config("invalid management resource bounds"));
            }
        }

        let mut users = BTreeSet::new();
        for u in &self.users {
            if u.name.is_empty()
                || !users.insert(u.name.clone())
                || !identifier(&u.password_secret_ref)
            {
                return Err(EngineError::config("invalid/duplicate user"));
            }
        }
        if users.is_empty() {
            return Err(EngineError::config("at least one SCRAM user is required"));
        }
        if !self.virtual_databases.is_empty() && !self.management.enabled {
            return Err(EngineError::config(
                "virtual databases require managed storage",
            ));
        }
        if self.virtual_databases.len() > 256 {
            return Err(EngineError::config("virtual database limit exceeded"));
        }
        let mut databases = BTreeSet::new();
        for db in &self.virtual_databases {
            db.validate(&users)?;
            if !databases.insert(db.name.to_lowercase())
                || aliases.contains(&db.name.to_lowercase())
            {
                return Err(EngineError::config(
                    "duplicate virtual database name or source alias",
                ));
            }
        }
        for g in &self.grants {
            if !users.contains(&g.principal)
                || (!self.management.enabled && !ids.contains(&g.source))
                || g.actions.is_empty()
            {
                return Err(EngineError::config(
                    "grant refers to unknown principal/source or has no actions",
                ));
            }
        }
        let mut capabilities = BTreeSet::new();
        for c in &self.capabilities {
            if (!self.management.enabled && !ids.contains(&c.source))
                || !capabilities.insert(c.source.clone())
                || c.duckdb_version != "v2.0.0"
                || c.extension_version.is_empty()
                || c.remote_version.is_empty()
                || c.evidence.is_empty()
            {
                return Err(EngineError::config(
                    "capability record requires unique source and pinned version/evidence",
                ));
            }
        }
        Ok(())
    }
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}
pub fn quote_identifier(v: &str) -> String {
    format!("\"{}\"", v.replace('"', "\"\""))
}
pub fn quote_literal(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}
