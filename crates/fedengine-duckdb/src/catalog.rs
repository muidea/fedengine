use crate::error::{engine_error, metadata_text};
use duckdb::Connection;
use fedengine_core::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Hash serialized facts without retaining a second full JSON payload.
fn fact_digest(value: &impl serde::Serialize) -> String {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).expect("serializable metadata facts");
    writer
        .0
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bound accounted metadata payload without allocating another serialized copy.
pub(super) fn enforce_budget(value: &impl serde::Serialize, limit: usize) -> Result<()> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(std::io::Error::other("metadata budget exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { bytes: 0, limit }, value)
        .map_err(|_| EngineError::new("54000", "metadata operation payload budget exceeded"))
}
fn remote_sql(source: &DataSourceDefinition, query: &str) -> String {
    format!(
        "SELECT * FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(query)
    )
}
pub(super) fn discover(conn: &Connection, source: &DataSourceDefinition) -> Result<Vec<String>> {
    let query = if source.kind == DataSourceKind::Postgres {
        "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name"
    } else {
        "SELECT DATABASE() AS schema_name"
    };
    let mut statement = conn
        .prepare(&remote_sql(source, query))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut schemas = Vec::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if allowed_namespace(&schema) {
            schemas.push(schema);
        }
        if schemas.len() > 10_000 {
            return Err(EngineError::new("54000", "schema discovery limit exceeded"));
        }
    }
    Ok(schemas)
}
pub(super) fn capture(conn: &Connection, revision: &SourceRevision) -> Result<CatalogSnapshot> {
    capture_bounded(conn, revision, 64 * 1024 * 1024)
}
pub(super) fn capture_bounded(
    conn: &Connection,
    revision: &SourceRevision,
    budget: usize,
) -> Result<CatalogSnapshot> {
    for attempt in 0..3 {
        if attempt > 0 {
            conn.execute_batch(&format!(
                "CALL {}_clear_cache()",
                revision.definition.kind.extension()
            ))
            .map_err(engine_error)?;
        }
        match capture_once(conn, revision, budget) {
            Err(error) if error.sqlstate == "40001" && attempt < 2 => continue,
            result => return result,
        }
    }
    unreachable!()
}
fn capture_once(
    conn: &Connection,
    revision: &SourceRevision,
    budget: usize,
) -> Result<CatalogSnapshot> {
    let started = std::time::Instant::now();
    let source = revision.effective_definition();
    let available = discover(conn, &source)?;
    if source
        .allowed_schemas
        .iter()
        .any(|s| !available.contains(s))
    {
        return Err(EngineError::config(
            "selected schema does not exist in the target database",
        ));
    }
    let mut objects = read_core(conn, &source, budget)?;
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog core enumeration finished");
    enforce_budget(&objects.values().collect::<Vec<_>>(), budget)?;
    let before = core_identity(&objects);
    let mapping_started = std::time::Instant::now();
    // Ask the pinned extension for its actual mapping; do not guess remote types.
    for object in objects.values_mut() {
        let sql = format!(
            "DESCRIBE SELECT * FROM {}.{}.{} LIMIT 0",
            quote_identifier(&source.catalog_alias),
            quote_identifier(&object.schema),
            quote_identifier(&object.name)
        );
        let mappings = (|| -> Result<BTreeMap<String, String>> {
            let mut statement = conn.prepare(&sql).map_err(engine_error)?;
            let mut rows = statement.query([]).map_err(engine_error)?;
            let mut mappings = BTreeMap::new();
            while let Some(row) = rows.next().map_err(engine_error)? {
                mappings.insert(
                    row.get(0).map_err(engine_error)?,
                    row.get(1).map_err(engine_error)?,
                );
            }
            Ok(mappings)
        })();
        match mappings {
            Ok(mappings) => {
                for column in &mut object.columns {
                    column.mapped_type = mappings.get(&column.name).cloned();
                    column.supported = column
                        .mapped_type
                        .as_ref()
                        .is_some_and(|t| crate::codec::type_name(t).is_ok());
                }
            }
            Err(error) if error.sqlstate == "0A000" => {}
            Err(error) => return Err(error),
        }
    }
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=mapping_started.elapsed().as_secs_f64()*1000., "catalog type mapping finished");
    let facts_started = std::time::Instant::now();
    crate::relational::capture(conn, &source, &mut objects, budget)?;
    tracing::debug!(source=%source.id.0, elapsed_ms=facts_started.elapsed().as_secs_f64()*1000., "catalog relational facts finished");
    // Extension queries may use different remote transactions. A second bounded
    // enumeration detects observed relation/column drift before any publication.
    if core_identity(&read_core(conn, &source, budget)?) != before {
        return Err(EngineError::new(
            "40001",
            "remote DDL changed the scanned catalog; retry refresh",
        ));
    }
    let objects: Vec<_> = objects.into_values().collect();
    let core: Vec<_> = objects
        .iter()
        .map(|o| {
            (
                &o.schema,
                &o.name,
                &o.object_type,
                &o.metadata.source_id,
                &o.columns,
            )
        })
        .collect();
    let structure_digest = fact_digest(&(&source.allowed_schemas, core));
    tracing::debug!(source=%source.id.0, objects=objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog remote capture finished");
    Ok(CatalogSnapshot {
        database_oid: 0,
        schema_oids: BTreeMap::new(),
        id: uuid::Uuid::new_v4().to_string(),
        incarnation: revision.incarnation.clone(),
        revision: revision.revision,
        generation: 0,
        refreshed_at: chrono::Utc::now().to_rfc3339(),
        complete: true,
        stale: false,
        structure_digest,
        schemas: source.allowed_schemas,
        objects,
    })
}
type CoreObjects = BTreeMap<(String, String), CatalogObject>;
fn core_identity(objects: &CoreObjects) -> String {
    // Only source identities and original core facts; mapped types and scan times
    // are deliberately excluded from the remote consistency comparison.
    let facts = objects
        .values()
        .map(|o| {
            (
                &o.schema,
                &o.name,
                &o.object_type,
                &o.metadata.source_id,
                o.columns
                    .iter()
                    .map(|c| (&c.name, c.ordinal, &c.remote_type, c.nullable))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    fact_digest(&facts)
}
fn read_core(
    conn: &Connection,
    source: &DataSourceDefinition,
    budget: usize,
) -> Result<CoreObjects> {
    let mut objects = BTreeMap::new();
    let query = if source.kind == DataSourceKind::Postgres {
        "SELECT n.nspname AS table_schema,c.relname AS table_name,CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'BASE TABLE' END AS table_type,c.oid::text AS source_id FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') ORDER BY n.nspname,c.relname"
    } else {
        "SELECT table_schema, table_name, table_type,NULL AS source_id FROM information_schema.tables WHERE table_schema=DATABASE() ORDER BY table_schema, table_name"
    };
    let query = scoped_query(source, query);
    let mut statement = conn
        .prepare(&remote_sql(source, &query))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            continue;
        }
        let name = metadata_text(row, 1)?;
        objects.insert(
            (schema.clone(), name.clone()),
            CatalogObject {
                metadata: ObjectMetadata {
                    source_id: row.get(3).map_err(engine_error)?,
                    ..Default::default()
                },
                schema,
                name,
                object_type: metadata_text(row, 2)?,
                columns: vec![],
                indexes: vec![],
            },
        );
        if objects.len() > 10_000 {
            return Err(EngineError::new("54000", "catalog object limit exceeded"));
        }
    }
    let query = if source.kind == DataSourceKind::Postgres {
        "SELECT n.nspname AS table_schema,c.relname AS table_name,a.attname AS column_name,a.attnum AS ordinal_position,pg_catalog.format_type(a.atttypid,a.atttypmod) AS data_type,CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN ('r','v','m','f','p') ORDER BY n.nspname,c.relname,a.attnum"
    } else {
        "SELECT table_schema,table_name,column_name,ordinal_position,column_type AS data_type,is_nullable FROM information_schema.columns WHERE table_schema=DATABASE() ORDER BY table_schema,table_name,ordinal_position"
    };
    let query = scoped_query(source, query);
    let mut statement = conn
        .prepare(&remote_sql(source, &query))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut count = 0;
    let mut bytes = 0usize;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let key: (String, String) = (metadata_text(row, 0)?, metadata_text(row, 1)?);
        let object = objects.get_mut(&key).ok_or_else(|| {
            EngineError::new("40001", "remote DDL changed the scanned relation set")
        })?;
        let remote_type = metadata_text(row, 4)?;

        object.columns.push(CatalogColumn {
            name: metadata_text(row, 2)?,
            ordinal: row.get::<_, u32>(3).map_err(engine_error)?,
            remote_type,
            supported: false,
            mapped_type: None,
            nullable: metadata_text(row, 5)? == "YES",
        });
        count += 1;
        let col = object.columns.last().unwrap();
        bytes = bytes.saturating_add(
            col.name.len() + col.remote_type.len() + std::mem::size_of::<CatalogColumn>() + 128,
        );
        if count > 100_000 || bytes > budget {
            return Err(EngineError::new("54000", "catalog column limit exceeded"));
        }
    }
    Ok(objects)
}
fn scoped_query(source: &DataSourceDefinition, query: &str) -> String {
    if source.kind == DataSourceKind::Postgres {
        let schemas = source
            .allowed_schemas
            .iter()
            .map(|s| quote_literal(s))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = format!(
            "n.nspname IN ({})",
            if schemas.is_empty() { "NULL" } else { &schemas }
        );
        let (sql, order) = query.split_once(" ORDER BY ").unwrap_or((query, ""));
        format!(
            "{sql} {} {predicate} ORDER BY {order}",
            if sql.contains(" WHERE ") {
                "AND"
            } else {
                "WHERE"
            }
        )
    } else {
        query.into()
    }
}
