use crate::*;
use std::collections::BTreeMap;

/// MySQL catalogs do not provide a reliable table lifetime identifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlIdentityPolicy {
    #[default]
    Preserve,
    Strict,
}
impl MysqlIdentityPolicy {
    pub fn resets(self, kind: DataSourceKind) -> bool {
        self == Self::Strict && kind == DataSourceKind::Mysql
    }
}

/// Startup settings; source definitions themselves are imported only once.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagementConfig {
    pub enabled: bool,
    pub listen: String,
    pub credentials_directory: String,
    pub queue_capacity: usize,
    pub max_concurrent_operations: usize,
    pub drain_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub mysql_identity_policy: MysqlIdentityPolicy,
}
impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:15433".into(),
            credentials_directory: "./secrets".into(),
            queue_capacity: 64,
            max_concurrent_operations: 2,
            drain_timeout_ms: 30_000,
            request_timeout_ms: 10_000,
            mysql_identity_policy: MysqlIdentityPolicy::Preserve,
        }
    }
}

/// Identity and version captured by every prepared binding.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceVersion {
    pub incarnation: String,
    pub revision: u64,
    pub generation: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum SourceStatus {
    Registered,
    Activating,
    Active,
    Draining,
    Disabled,
    MetadataStale,
    Degraded,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaBinding {
    pub remote_schema: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceRevision {
    pub incarnation: String,
    pub revision: u64,
    pub definition: DataSourceDefinition,
    pub bindings: Vec<SchemaBinding>,
}
impl SourceRevision {
    pub fn effective_definition(&self) -> DataSourceDefinition {
        let mut definition = self.definition.clone();
        definition.allowed_schemas = self
            .bindings
            .iter()
            .filter(|b| b.enabled)
            .map(|b| b.remote_schema.clone())
            .collect();
        definition.enabled = true;
        definition
    }
    pub fn key(&self) -> String {
        revision_key(&self.incarnation, self.revision)
    }
}
pub fn revision_key(incarnation: &str, revision: u64) -> String {
    format!("{incarnation}:{revision}")
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ManagedSource {
    pub id: DataSourceId,
    pub incarnation: String,
    pub kind: DataSourceKind,
    pub catalog_alias: String,
    pub desired_revision: u64,
    pub applied_revision: Option<u64>,
    pub desired_enabled: bool,
    pub status: SourceStatus,
    pub generation: u64,
    pub deleted: bool,
    pub current_operation: Option<String>,
    pub last_error: Option<String>,
    pub catalog_snapshot: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum OperationStatus {
    Pending,
    Running,
    WaitingForDrain,
    Succeeded,
    Failed,
    Interrupted,
}
impl OperationStatus {
    pub fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}
/// All existing-source mutations and probes compare both lifetime identity and revision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTarget {
    pub incarnation: String,
    pub expected_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementAction {
    Register {
        definition: DataSourceDefinition,
    },
    Patch {
        id: DataSourceId,
        target: SourceTarget,
        secret_ref: String,
    },
    Schemas {
        id: DataSourceId,
        target: SourceTarget,
        bindings: Vec<SchemaBinding>,
        default_schema: Option<String>,
    },
    Apply {
        id: DataSourceId,
        target: SourceTarget,
    },
    Disable {
        id: DataSourceId,
        target: SourceTarget,
    },
    Delete {
        id: DataSourceId,
        target: SourceTarget,
    },
    Probe {
        id: DataSourceId,
        target: SourceTarget,
    },
    Discover {
        id: DataSourceId,
        target: SourceTarget,
    },
    Refresh {
        id: DataSourceId,
        target: SourceTarget,
    },
}
impl ManagementAction {
    pub fn id(&self) -> &DataSourceId {
        match self {
            Self::Register { definition } => &definition.id,
            Self::Patch { id, .. }
            | Self::Schemas { id, .. }
            | Self::Apply { id, .. }
            | Self::Disable { id, .. }
            | Self::Delete { id, .. }
            | Self::Probe { id, .. }
            | Self::Discover { id, .. }
            | Self::Refresh { id, .. } => id,
        }
    }
    pub fn target(&self) -> Option<&SourceTarget> {
        match self {
            Self::Register { .. } => None,
            Self::Patch { target, .. }
            | Self::Schemas { target, .. }
            | Self::Apply { target, .. }
            | Self::Disable { target, .. }
            | Self::Delete { target, .. }
            | Self::Probe { target, .. }
            | Self::Discover { target, .. }
            | Self::Refresh { target, .. } => Some(target),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ManagementOperation {
    pub id: String,
    pub idempotency_key: String,
    pub parameter_digest: String,
    pub incarnation: String,
    pub target_revision: u64,
    pub action: ManagementAction,
    pub status: OperationStatus,
    pub phase: String,
    pub result_schemas: Option<Vec<String>>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogColumn {
    pub name: String,
    pub ordinal: u32,
    pub remote_type: String,
    pub mapped_type: Option<String>,
    pub nullable: bool,
    pub supported: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogIndex {
    pub name: String,
    pub definition: String,
    pub supported: bool,
    pub protected: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogObject {
    pub metadata: ObjectMetadata,
    pub schema: String,
    pub name: String,
    pub object_type: String,
    pub columns: Vec<CatalogColumn>,
    pub indexes: Vec<CatalogIndex>,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogSnapshot {
    pub database_oid: u32,
    pub schema_oids: BTreeMap<String, u32>,
    pub id: String,
    pub incarnation: String,
    pub revision: u64,
    pub generation: u64,
    pub refreshed_at: String,
    pub complete: bool,
    pub stale: bool,
    pub structure_digest: String,
    pub schemas: Vec<String>,
    pub objects: Vec<CatalogObject>,
}
/// Persisted state, exposed only through the controlled storage service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ManagementState {
    pub sources: BTreeMap<String, ManagedSource>,
    pub revisions: BTreeMap<String, SourceRevision>,
    pub operations: BTreeMap<String, ManagementOperation>,
    pub catalogs: BTreeMap<String, CatalogSnapshot>,
}
impl ManagementState {
    /// Keep complete current snapshots and bounded historical snapshots per lifetime.
    pub fn prune_catalogs(&mut self, keep: usize) {
        let mut retained = BTreeSet::new();
        for source in self.sources.values() {
            if let Some(id) = &source.catalog_snapshot {
                retained.insert(id.clone());
            }
            let mut snapshots: Vec<_> = self
                .catalogs
                .values()
                .filter(|c| c.incarnation == source.incarnation)
                .collect();
            snapshots.sort_by(|a, b| b.refreshed_at.cmp(&a.refreshed_at));
            retained.extend(snapshots.into_iter().take(keep).map(|s| s.id.clone()));
        }
        self.catalogs.retain(|id, _| retained.contains(id));
    }
    pub fn source(&self, id: &DataSourceId) -> Result<&ManagedSource> {
        self.sources
            .values()
            .find(|s| !s.deleted && s.id == *id)
            .ok_or_else(|| EngineError::new("42704", "source does not exist"))
    }
    pub fn revision(&self, incarnation: &str, revision: u64) -> Result<&SourceRevision> {
        self.revisions
            .get(&revision_key(incarnation, revision))
            .ok_or_else(|| EngineError::new("42704", "source revision does not exist"))
    }
}

pub fn valid_secret_ref(value: &str) -> bool {
    if let Some(path) = value.strip_prefix("file:") {
        !path.is_empty()
            && !Path::new(path).is_absolute()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
    } else {
        identifier(value)
    }
}
pub fn allowed_namespace(value: &str) -> bool {
    identifier(value)
        && ![
            "pg_catalog",
            "information_schema",
            "sys",
            "mysql",
            "_fedengine",
        ]
        .contains(&value.to_ascii_lowercase().as_str())
        && !value.to_ascii_lowercase().starts_with("pg_toast")
        && !value.to_ascii_lowercase().starts_with("pg_temp")
}
pub fn validate_source(source: &DataSourceDefinition) -> Result<()> {
    let mut schemas = BTreeSet::new();
    if source.id.0.is_empty()
        || source.id.0.len() > 128
        || !identifier(&source.id.0)
        || !identifier(&source.catalog_alias)
        || [
            "memory",
            "main",
            "temp",
            "system",
            "pg_catalog",
            "information_schema",
            "_fedengine",
        ]
        .contains(&source.catalog_alias.to_ascii_lowercase().as_str())
        || !valid_secret_ref(&source.secret_ref)
        || source
            .allowed_schemas
            .iter()
            .any(|s| !allowed_namespace(s) || !schemas.insert(s))
        || (!source.default_schema.is_empty()
            && !source.allowed_schemas.contains(&source.default_schema))
    {
        return Err(EngineError::config(
            "invalid source identity, namespace or secret reference",
        ));
    }
    Ok(())
}
