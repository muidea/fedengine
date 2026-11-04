//! Versioned PostgreSQL 16 catalog field contracts, captured from the pinned baseline.
use fedengine_core::*;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, Clone, Deserialize)]
pub struct SystemField {
    pub relname: String,
    pub oid: u32,
    pub attname: String,
    pub atttypid: u32,
    pub atttypmod: i32,
    pub attnum: i16,
    pub pg_type: String,
}
impl SystemField {
    pub fn duckdb_type(&self) -> &'static str {
        match self.atttypid {
            16 => "BOOLEAN",
            21 => "SMALLINT",
            23 => "INTEGER",
            20 => "BIGINT",
            26 | 28 | 29 | 24 | 2205 | 2206 => "UINTEGER",
            700 => "FLOAT",
            701 => "DOUBLE",
            22 | 1005 => "SMALLINT[]",
            30 | 1028 => "UINTEGER[]",
            1007 => "INTEGER[]",
            1016 => "BIGINT[]",
            1000 => "BOOLEAN[]",
            1002 | 1003 | 1009 | 1014 | 1015 | 1034 => "VARCHAR[]",
            1184 => "TIMESTAMPTZ",
            1114 => "TIMESTAMP",
            17 => "BLOB",
            2277 => "VARCHAR[]",
            _ => "VARCHAR",
        }
    }
}
pub fn catalog_schema() -> &'static BTreeMap<String, Vec<SystemField>> {
    static SCHEMA: OnceLock<BTreeMap<String, Vec<SystemField>>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        #[derive(Deserialize)]
        struct Captured {
            fields: Vec<SystemField>,
        }
        let captured: Captured = serde_json::from_str(include_str!("../catalog/pg16-schema.json"))
            .expect("checked-in PG16 schema is valid");
        let mut map: BTreeMap<String, Vec<SystemField>> = BTreeMap::new();
        for field in captured.fields {
            map.entry(field.relname.clone()).or_default().push(field);
        }
        map
    })
}
pub fn physical_catalog(name: &str) -> Result<String> {
    if !catalog_schema().contains_key(name) {
        return Err(EngineError::unsupported(
            "system catalog is outside the compatibility matrix",
        ));
    }
    Ok(format!(
        "_fedengine.{}",
        quote_identifier(&format!("meta_{}", name.replace('.', "_")))
    ))
}
pub fn builtin_types() -> &'static [serde_json::Value] {
    static TYPES: OnceLock<Vec<serde_json::Value>> = OnceLock::new();
    TYPES.get_or_init(|| {
        serde_json::from_str(include_str!("../catalog/pg16-types.json"))
            .expect("checked-in PG16 types are valid")
    })
}
