//! Published relational facts. Unknown facts stay optional; expressions are never executable input.
use crate::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSupport {
    Supported,
    Partial,
    Unsupported,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataScanState {
    #[default]
    NotScanned,
    Success,
    Partial,
    Forbidden,
    Failed,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataStatus {
    pub support: MetadataSupport,
    pub scan_state: MetadataScanState,
    /// Complete enumeration permits retiring missing facts; partial results never do.
    pub complete: bool,
    pub stale: bool,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub error_code: Option<String>,
    pub unsupported_fields: Vec<String>,
}
impl MetadataStatus {
    pub fn success() -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            support: MetadataSupport::Supported,
            scan_state: MetadataScanState::Success,
            complete: true,
            stale: false,
            last_attempt_at: Some(now.clone()),
            last_success_at: Some(now),
            ..Self::default()
        }
    }
    pub fn failure(error: &EngineError) -> Self {
        Self {
            scan_state: if error.sqlstate == "42501" {
                MetadataScanState::Forbidden
            } else {
                MetadataScanState::Failed
            },
            stale: true,
            last_attempt_at: Some(chrono::Utc::now().to_rfc3339()),
            error_code: Some(error.sqlstate.into()),
            ..Self::default()
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationStatus {
    Exact,
    Translated,
    Unsupported,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataExpression {
    pub source_text: String,
    pub source_dialect: String,
    pub pg_text: Option<String>,
    pub translation_status: TranslationStatus,
}
impl MetadataExpression {
    pub fn source(text: String, kind: DataSourceKind) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        let postgres = kind == DataSourceKind::Postgres;
        Some(Self {
            pg_text: postgres.then(|| text.clone()),
            source_text: text,
            source_dialect: kind.extension().into(),
            translation_status: if postgres {
                TranslationStatus::Exact
            } else {
                TranslationStatus::Unsupported
            },
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnProperties {
    pub default_oid: u32,
    pub type_oid: u32,
    pub type_modifier: i32,
    pub attnum: i16,
    pub source_id: Option<String>,
    pub default: Option<MetadataExpression>,
    pub identity: Option<String>,
    pub generated: Option<MetadataExpression>,
    pub comment: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataReferenceState {
    Resolved,
    OutsideScope,
    #[default]
    Unresolved,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataRelationReference {
    pub schema: String,
    pub table: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataForeignKey {
    pub reference_state: MetadataReferenceState,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
    pub match_type: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataConstraint {
    pub oid: u32,
    pub source_id: Option<String>,
    pub name: String,
    /// PostgreSQL fact codes p/u/f/c. Unsupported constraint kinds remain unprojected.
    pub kind: String,
    pub columns: Vec<String>,
    pub backing_index: Option<String>,
    pub foreign_key: Option<MetadataForeignKey>,
    pub definition: Option<MetadataExpression>,
    pub enforced: Option<bool>,
    pub validated: Option<bool>,
    pub deferrable: Option<bool>,
    pub initially_deferred: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataIndexItem {
    pub column: Option<String>,
    pub expression: Option<MetadataExpression>,
    pub descending: Option<bool>,
    pub nulls_first: Option<bool>,
    pub prefix_length: Option<u32>,
    pub collation: Option<String>,
    pub operator_class: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataIndex {
    pub oid: u32,
    pub source_id: Option<String>,
    pub name: String,
    pub method: Option<String>,
    pub unique: Option<bool>,
    pub primary: Option<bool>,
    pub valid: Option<bool>,
    pub enabled: Option<bool>,
    pub nulls_not_distinct: Option<bool>,
    pub keys: Vec<MetadataIndexItem>,
    pub include: Vec<String>,
    pub predicate: Option<MetadataExpression>,
    pub definition: Option<MetadataExpression>,
    pub storage: RelationStorage,
    pub activity: IndexActivity,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexActivity {
    pub idx_scan: Option<i64>,
    pub idx_tup_read: Option<i64>,
    pub idx_tup_fetch: Option<i64>,
}
/// No histogram or most-common business values are collected here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnStatistics {
    pub null_frac: Option<serde_json::Number>,
    pub avg_width: Option<i32>,
    pub n_distinct: Option<serde_json::Number>,
}
/// Source planner estimates, captured from catalogs without reading business rows.
/// These describe the remote relation, not local DuckDB storage or an exact COUNT.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelationStatistics {
    pub estimated_rows: Option<serde_json::Number>,
    pub pages: Option<i32>,
    pub all_visible_pages: Option<i32>,
}
/// Source storage measurements at refresh time. MySQL lengths are kept separate
/// from PostgreSQL relation forks: their storage semantics are not interchangeable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelationStorage {
    pub main_bytes: Option<i64>,
    pub fsm_bytes: Option<i64>,
    pub vm_bytes: Option<i64>,
    pub init_bytes: Option<i64>,
    pub table_bytes: Option<i64>,
    pub indexes_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub data_length: Option<i64>,
    pub index_length: Option<i64>,
    pub data_free: Option<i64>,
}
impl RelationStorage {
    pub fn validate(&self) -> Result<()> {
        if [
            self.main_bytes,
            self.fsm_bytes,
            self.vm_bytes,
            self.init_bytes,
            self.table_bytes,
            self.indexes_bytes,
            self.total_bytes,
            self.data_length,
            self.index_length,
            self.data_free,
        ]
        .into_iter()
        .flatten()
        .any(|v| v < 0)
        {
            return Err(EngineError::new(
                "55000",
                "negative relation storage measurement",
            ));
        }
        Ok(())
    }
}
/// Remote PostgreSQL cumulative counters/estimates, never local query counters.
/// A source restart or statistics reset can decrease these values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelationActivity {
    pub seq_scan: Option<i64>,
    pub seq_tup_read: Option<i64>,
    pub idx_scan: Option<i64>,
    pub idx_tup_fetch: Option<i64>,
    pub n_tup_ins: Option<i64>,
    pub n_tup_upd: Option<i64>,
    pub n_tup_del: Option<i64>,
    pub n_tup_hot_upd: Option<i64>,
    pub n_live_tup: Option<i64>,
    pub n_dead_tup: Option<i64>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectMetadata {
    pub statistics: RelationStatistics,
    pub storage: RelationStorage,
    pub activity: RelationActivity,
    pub column_statistics: BTreeMap<String, ColumnStatistics>,
    pub oid: u32,
    pub dropped_attnums: Vec<i16>,
    pub source_id: Option<String>,
    pub comment: Option<String>,
    pub view_definition: Option<MetadataExpression>,
    pub view_dependencies: Option<Vec<MetadataRelationReference>>,
    pub columns: BTreeMap<String, ColumnProperties>,
    pub constraints: Vec<MetadataConstraint>,
    pub indexes: Vec<MetadataIndex>,
    pub status: BTreeMap<String, MetadataStatus>,
}
impl ObjectMetadata {
    pub fn validate(&self, columns: &[CatalogColumn]) -> Result<()> {
        let names: std::collections::BTreeSet<_> =
            columns.iter().map(|c| c.name.as_str()).collect();
        self.storage.validate()?;
        if [
            self.activity.seq_scan,
            self.activity.seq_tup_read,
            self.activity.idx_scan,
            self.activity.idx_tup_fetch,
            self.activity.n_tup_ins,
            self.activity.n_tup_upd,
            self.activity.n_tup_del,
            self.activity.n_tup_hot_upd,
            self.activity.n_live_tup,
            self.activity.n_dead_tup,
        ]
        .into_iter()
        .flatten()
        .any(|v| v < 0)
        {
            return Err(EngineError::new(
                "55000",
                "negative relation activity counter",
            ));
        }
        for (column, stats) in &self.column_statistics {
            if !names.contains(column.as_str())
                || stats.avg_width.is_some_and(|v| v < 0)
                || stats
                    .null_frac
                    .as_ref()
                    .and_then(|v| v.as_f64())
                    .is_some_and(|v| !(0.0..=1.0).contains(&v))
                || stats
                    .n_distinct
                    .as_ref()
                    .and_then(|v| v.as_f64())
                    .is_some_and(|v| v < -1.0)
            {
                return Err(EngineError::new(
                    "55000",
                    "invalid non-value column statistic",
                ));
            }
        }
        for c in &self.constraints {
            if c.columns.iter().any(|n| !names.contains(n.as_str()))
                || c.foreign_key
                    .as_ref()
                    .is_some_and(|f| f.columns.len() != c.columns.len())
            {
                return Err(EngineError::new(
                    "55000",
                    "metadata constraint references inconsistent columns",
                ));
            }
        }
        for i in &self.indexes {
            i.storage.validate()?;
            if [
                i.activity.idx_scan,
                i.activity.idx_tup_read,
                i.activity.idx_tup_fetch,
            ]
            .into_iter()
            .flatten()
            .any(|v| v < 0)
            {
                return Err(EngineError::new("55000", "negative index activity counter"));
            }
            if i.keys
                .iter()
                .filter_map(|k| k.column.as_ref())
                .chain(i.include.iter())
                .any(|n| !names.contains(n.as_str()))
            {
                return Err(EngineError::new(
                    "55000",
                    "metadata index references inconsistent columns",
                ));
            }
        }
        Ok(())
    }
}
