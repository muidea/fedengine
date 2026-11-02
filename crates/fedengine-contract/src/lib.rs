//! Versioned control messages and protocol independent streaming contracts.
use fedengine_core::{DataSourceId, EngineError, Result};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const QUERY_TOPIC: &str = "query/execute/v1";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Query,
    Insert,
    Update,
    Delete,
    Ddl,
}
impl StatementKind {
    pub fn writes(self) -> bool {
        self != Self::Query
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgType {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Date,
    Time,
    Timestamp,
    Timestamptz,
    Uuid,
    Json,
}
impl PgType {
    pub fn duckdb_name(self) -> &'static str {
        match self {
            Self::Bool => "BOOLEAN",
            Self::Int2 => "SMALLINT",
            Self::Int4 => "INTEGER",
            Self::Int8 => "BIGINT",
            Self::Float4 => "FLOAT",
            Self::Float8 => "DOUBLE",
            Self::Numeric => "DECIMAL(38,10)",
            Self::Text => "VARCHAR",
            Self::Bytea => "BLOB",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Timestamp => "TIMESTAMP",
            Self::Timestamptz => "TIMESTAMPTZ",
            Self::Uuid => "UUID",
            Self::Json => "JSON",
        }
    }
    pub fn oid(self) -> u32 {
        match self {
            Self::Bool => 16,
            Self::Int2 => 21,
            Self::Int4 => 23,
            Self::Int8 => 20,
            Self::Float4 => 700,
            Self::Float8 => 701,
            Self::Numeric => 1700,
            Self::Text => 25,
            Self::Bytea => 17,
            Self::Date => 1082,
            Self::Time => 1083,
            Self::Timestamp => 1114,
            Self::Timestamptz => 1184,
            Self::Uuid => 2950,
            Self::Json => 114,
        }
    }
    pub fn from_oid(oid: u32) -> Option<Self> {
        Some(match oid {
            16 => Self::Bool,
            21 => Self::Int2,
            23 => Self::Int4,
            20 => Self::Int8,
            700 => Self::Float4,
            701 => Self::Float8,
            1700 => Self::Numeric,
            25 | 1043 | 1042 => Self::Text,
            17 => Self::Bytea,
            1082 => Self::Date,
            1083 => Self::Time,
            1114 => Self::Timestamp,
            1184 => Self::Timestamptz,
            2950 => Self::Uuid,
            114 => Self::Json,
            _ => return None,
        })
    }
}
#[derive(Debug, Clone)]
pub enum Cell {
    Null,
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Numeric(String),
    Date(i32),
    Time(i64),
    Timestamp(i64),
    Uuid([u8; 16]),
}
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: PgType,
}
#[derive(Debug, Clone)]
pub struct PreparedDescriptor {
    pub sql: String,
    pub parameters: Vec<PgType>,
    pub columns: Vec<ColumnMeta>,
    pub generations: BTreeMap<DataSourceId, u64>,
    pub kind: StatementKind,
}
#[derive(Debug, Clone)]
pub enum QueryMode {
    Prepare {
        parameter_types: Vec<Option<PgType>>,
    },
    Execute {
        parameters: Vec<Cell>,
        parameter_types: Vec<PgType>,
        expected: BTreeMap<DataSourceId, u64>,
    },
}
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub principal: String,
    pub session_id: i32,
    pub sql: String,
    pub mode: QueryMode,
    pub deadline: Instant,
    pub cancel: CancellationToken,
}
pub enum QueryReply {
    Prepared(PreparedDescriptor),
    Accepted(AcceptedExecution),
}
pub struct AcceptedExecution {
    pub columns: Vec<ColumnMeta>,
    pub kind: StatementKind,
    stream: Mutex<Option<ExecutionStream>>,
}
impl AcceptedExecution {
    pub fn new(columns: Vec<ColumnMeta>, kind: StatementKind, stream: ExecutionStream) -> Self {
        Self {
            columns,
            kind,
            stream: Mutex::new(Some(stream)),
        }
    }
    pub fn take_stream(&self) -> Result<ExecutionStream> {
        self.stream
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| EngineError::new("55000", "execution stream already consumed"))
    }
}
#[derive(Debug, Clone)]
pub struct Completion {
    pub command: String,
    pub affected_rows: Option<u64>,
    pub outcome: ExecutionOutcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Completed,
    CommitConfirmed,
    Indeterminate,
}
pub enum ResultChunk {
    Rows(Vec<Vec<Cell>>),
    Complete(Completion),
    Error(EngineError),
}
/// Dropping a receiver requests cooperative cancellation; it never asserts rollback.
pub struct ExecutionStream {
    pub receiver: mpsc::Receiver<ResultChunk>,
    pub cancel: CancellationToken,
}
impl Drop for ExecutionStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[derive(Clone)]
pub struct ExecutionService {
    pub port: Arc<dyn ExecutionPort>,
}
pub struct ExecuteRequest {
    pub principal: String,
    pub session_id: i32,
    pub sql: String,
    pub kind: StatementKind,
    pub parameters: Vec<Cell>,
    pub parameter_types: Vec<Option<PgType>>,
    pub prepare_only: bool,
    pub generations: BTreeMap<DataSourceId, u64>,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub ddl_source: Option<DataSourceId>,
    pub command: String,
}
pub enum RuntimeReply {
    Prepared(PreparedDescriptor),
    Accepted(AcceptedExecution),
}
#[async_trait::async_trait]
pub trait ExecutionPort: Send + Sync {
    /// Ownership contains service leases and source fences, retained until actual worker exit.
    async fn submit(
        &self,
        request: ExecuteRequest,
        ownership: Box<dyn Send>,
    ) -> Result<RuntimeReply>;
    async fn indexes(
        &self,
        source: DataSourceId,
        ownership: Box<dyn Send>,
    ) -> Result<Vec<RemoteIndex>>;
}

#[derive(Debug, Clone)]
pub struct RemoteIndex {
    pub source: DataSourceId,
    pub catalog: String,
    pub schema: String,
    pub index: String,
    pub table: String,
    pub protected: bool,
}

/// Process-local counters. Exporters can read snapshots without querying business SQL.
#[derive(Default)]
pub struct EngineMetrics {
    pub active_sessions: Arc<std::sync::atomic::AtomicU64>,
    pub active_workers: Arc<std::sync::atomic::AtomicU64>,
    pub worker_waiters: Arc<std::sync::atomic::AtomicU64>,
    pub admitted: std::sync::atomic::AtomicU64,
    pub completed: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub canceled: std::sync::atomic::AtomicU64,
    pub unknown_writes: std::sync::atomic::AtomicU64,
    pub ddl_confirmed: std::sync::atomic::AtomicU64,
    pub metadata_stale: std::sync::atomic::AtomicU64,
    pub rows_streamed: std::sync::atomic::AtomicU64,
    pub execution_micros: std::sync::atomic::AtomicU64,
}
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub active_sessions: u64,
    pub active_workers: u64,
    pub worker_waiters: u64,
    pub admitted: u64,
    pub completed: u64,
    pub failed: u64,
    pub canceled: u64,
    pub unknown_writes: u64,
    pub ddl_confirmed: u64,
    pub metadata_stale: u64,
    pub rows_streamed: u64,
    pub execution_micros: u64,
}
impl EngineMetrics {
    /// Gauges and counters are individually atomic; a snapshot is not a global transaction.
    pub fn snapshot(&self) -> MetricsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        MetricsSnapshot {
            active_sessions: self.active_sessions.load(Relaxed),
            active_workers: self.active_workers.load(Relaxed),
            worker_waiters: self.worker_waiters.load(Relaxed),
            admitted: self.admitted.load(Relaxed),
            completed: self.completed.load(Relaxed),
            failed: self.failed.load(Relaxed),
            canceled: self.canceled.load(Relaxed),
            unknown_writes: self.unknown_writes.load(Relaxed),
            ddl_confirmed: self.ddl_confirmed.load(Relaxed),
            metadata_stale: self.metadata_stale.load(Relaxed),
            rows_streamed: self.rows_streamed.load(Relaxed),
            execution_micros: self.execution_micros.load(Relaxed),
        }
    }
}
/// Returns a gauge to its prior value on actual scope exit, including unwinding.
pub struct GaugeGuard(Arc<std::sync::atomic::AtomicU64>);
impl GaugeGuard {
    pub fn enter(gauge: Arc<std::sync::atomic::AtomicU64>) -> Self {
        gauge.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(gauge)
    }
}
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}
