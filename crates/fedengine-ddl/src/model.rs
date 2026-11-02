use fedengine_core::{DataSourceId, quote_identifier};
use std::collections::BTreeSet;
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub source: DataSourceId,
    pub catalog: String,
    pub schema: String,
    pub name: String,
}
impl ObjectRef {
    pub fn sql(&self) -> String {
        [
            self.catalog.as_str(),
            self.schema.as_str(),
            self.name.as_str(),
        ]
        .map(quote_identifier)
        .join(".")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Insert,
    Update,
    Delete,
    Ddl,
}
#[derive(Debug, Clone)]
pub struct Analysis {
    pub sql: String,
    pub kind: Kind,
    pub action: String,
    pub capability: String,
    pub command: String,
    pub objects: BTreeSet<ObjectRef>,
    pub sources: BTreeSet<DataSourceId>,
    pub primary: Option<ObjectRef>,
    pub index: Option<IndexIntent>,
}
#[derive(Debug, Clone)]
pub struct IndexOwner {
    pub index: ObjectRef,
    pub table: ObjectRef,
    pub protected: bool,
}
#[derive(Debug, Clone)]
pub struct IndexIntent {
    pub table: ObjectRef,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub create: bool,
    pub if_exists: bool,
}
