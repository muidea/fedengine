use crate::{
    codec::type_name,
    error::{engine_error, metadata_text},
};
use duckdb::{Connection, params_from_iter, types::Value};
use fedengine_contract::{ColumnMeta, RemoteIndex};
use fedengine_core::*;
pub(super) fn describe(
    conn: &Connection,
    sql: &str,
    count: usize,
    overrides: Option<&[Option<ColumnMeta>]>,
) -> Result<Vec<ColumnMeta>> {
    let mut statement = conn
        .prepare(&format!("DESCRIBE {sql}"))
        .map_err(engine_error)?;
    let nulls = vec![Value::Null; count];
    let mut rows = statement
        .query(params_from_iter(nulls.iter()))
        .map_err(engine_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        let name: String = row.get(0).map_err(engine_error)?;
        let data_type: String = row.get(1).map_err(engine_error)?;
        let column = if let Some(Some(column)) = overrides.and_then(|o| o.get(result.len())) {
            column.clone()
        } else {
            let mut column = ColumnMeta::new(name.clone(), type_name(&data_type)?);
            column.type_modifier = crate::codec::type_modifier(&data_type);
            column
        };
        result.push(column);
    }
    Ok(result)
}
pub(super) fn discover_indexes(
    conn: &Connection,
    source: &DataSourceDefinition,
) -> Result<Vec<RemoteIndex>> {
    let remote = if source.kind == DataSourceKind::Postgres {
        "SELECT n.nspname AS schema_name, i.relname AS index_name, t.relname AS table_name, (x.indisprimary OR EXISTS(SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid=i.oid)) AS protected FROM pg_catalog.pg_index x JOIN pg_catalog.pg_class i ON i.oid=x.indexrelid JOIN pg_catalog.pg_class t ON t.oid=x.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=t.relnamespace"
    } else {
        "SELECT table_schema AS schema_name,index_name,table_name,(index_name='PRIMARY' OR non_unique=0) AS protected FROM information_schema.statistics GROUP BY table_schema,index_name,table_name,non_unique"
    };
    let sql = format!(
        "SELECT schema_name,index_name,table_name,protected FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(remote)
    );
    let mut stmt = conn.prepare(&sql).map_err(engine_error)?;
    let mut rows = stmt.query([]).map_err(engine_error)?;
    let mut indexes = Vec::new();
    while let Some(r) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(r, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            continue;
        }
        indexes.push(RemoteIndex {
            source: source.id.clone(),
            catalog: source.catalog_alias.clone(),
            schema,
            index: metadata_text(r, 1)?,
            table: metadata_text(r, 2)?,
            protected: r.get(3).map_err(engine_error)?,
        });
        if indexes.len() > 10000 {
            return Err(EngineError::new("54000", "index metadata limit exceeded"));
        }
    }
    Ok(indexes)
}
