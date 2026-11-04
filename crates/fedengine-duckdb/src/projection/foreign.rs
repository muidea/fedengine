//! Project logical foreign bindings from typed source catalogs without copying
//! the source object graph. All rows remain in the caller's unpublished transaction.
use super::*;
use crate::identity::OidBatch;

pub(crate) struct ForeignProjection<'a> {
    pub snapshot: &'a CatalogSnapshot,
    pub source: &'a CatalogSnapshot,
    pub alias: &'a str,
    pub binding: &'a VirtualSchema,
}

impl ForeignProjection<'_> {
    pub fn publish(&self, conn: &Connection, oids: &mut OidBatch<'_>) -> Result<()> {
        let _memory = crate::memory::Stage::new("virtual_projection");
        let Self {
            snapshot,
            source,
            alias,
            binding,
        } = self;
        super::publish(conn, snapshot, alias)?;
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_foreign_relations(remote_oid UINTEGER,local_oid UINTEGER,object VARCHAR)").map_err(engine_error)?;
        {
            let mut statement = conn.prepare("SELECT oid,relname FROM _fedengine.meta_pg_class WHERE _snapshot_id=? AND _schema=? AND relkind IN ('r','v','m','p','f') ORDER BY relname").map_err(engine_error)?;
            let mut rows = statement
                .query(params![source.id, binding.config.remote_schema])
                .map_err(engine_error)?;
            let mut app = conn
                .appender_to_db("__fed_foreign_relations", "main")
                .map_err(engine_error)?;
            let epoch = format!("virtual-schema:{}", binding.oid);
            while let Some(row) = rows.next().map_err(engine_error)? {
                let remote: u32 = row.get(0).map_err(engine_error)?;
                let name: String = row.get(1).map_err(engine_error)?;
                let local = oids.oid(&epoch, &[&source.incarnation, &remote.to_string()])?;
                app.append_row(params![remote, local, name])
                    .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        let id = quote_literal(&snapshot.id);
        let alias = quote_literal(alias);
        let schema = quote_literal(&binding.config.name);
        for relation in [
            "pg_class",
            "pg_attribute",
            "pg_type",
            "pg_description",
            "pg_stats",
            "information_schema.tables",
            "information_schema.columns",
        ] {
            let fields = &catalog_schema()[relation];
            let expressions = fields.iter().map(|field| {
                let name = field.attname.as_str();
                match (relation, name) {
                    ("pg_class", "oid") | ("pg_attribute", "attrelid") | ("pg_description", "objoid") => "m.local_oid".into(),
                    ("pg_class", "relnamespace") | ("pg_type", "typnamespace") => binding.oid.to_string(),
                    ("pg_class", "relkind") => "'f'".into(),
                    ("pg_class", "relhasindex" | "relhasrules" | "relhastriggers") | ("pg_attribute", "atthasdef") => "false".into(),
                    ("pg_class", "relchecks") => "0".into(),
                    ("pg_attribute", "attidentity" | "attgenerated") => "''".into(),
                    ("information_schema.tables", "table_catalog") | ("information_schema.columns", "table_catalog" | "udt_catalog") => alias.clone(),
                    ("information_schema.tables", "table_schema") | ("information_schema.columns", "table_schema") | ("pg_stats", "schemaname") => schema.clone(),
                    ("information_schema.tables", "table_type") => "'FOREIGN'".into(),
                    ("information_schema.columns", "column_default" | "generation_expression") => "NULL".into(),
                    ("information_schema.columns", "is_identity") => "'NO'".into(),
                    ("information_schema.columns", "is_generated") => "'NEVER'".into(),
                    ("information_schema.columns", "udt_schema") => format!("CASE WHEN p.udt_schema='pg_catalog' THEN 'pg_catalog' ELSE {schema} END"),
                    _ => format!("p.{}", quote_identifier(name)),
                }
            }).collect::<Vec<_>>().join(",");
            let filter = match relation {
                "pg_class" => " AND p.oid=m.remote_oid",
                "pg_attribute" => " AND p.attrelid=m.remote_oid",
                "pg_description" => " AND p.objoid=m.remote_oid",
                _ => "",
            };
            let table = physical_catalog(relation)?;
            conn.execute(&format!("INSERT INTO {table} SELECT {id},{alias},{schema},p._object,NULL,NULL,{expressions} FROM {table} p JOIN __fed_foreign_relations m ON p._object=m.object WHERE p._snapshot_id=? AND p._schema=?{filter}"), params![source.id, binding.config.remote_schema]).map_err(engine_error)?;
        }
        // Foreign bindings have no local storage or scan activity. Keep typed
        // unknown facts rather than copying the source's physical counters.
        let storage = serde_json::to_string(&RelationStorage::default())
            .map_err(|_| EngineError::new("XX000", "invalid storage statistics"))?;
        let mut activity = serde_json::to_value(RelationActivity::default())
            .map_err(|_| EngineError::new("XX000", "invalid activity statistics"))?;
        activity["scan_count"] = Value::Null;
        conn.execute(&format!("INSERT INTO _fedengine.metadata_relation_statistics SELECT {id},local_oid,{schema},object,?,? FROM __fed_foreign_relations"), params![storage, activity.to_string()]).map_err(engine_error)?;
        let epoch = format!("virtual-schema:{}", binding.oid);
        let wrapper = oids.oid(&epoch, &["wrapper"])?;
        let server = oids.oid(&epoch, &["server"])?;
        super::foreign_binding(
            conn,
            snapshot,
            self.alias,
            &binding.config.name,
            wrapper,
            server,
        )?;
        conn.execute(&format!("INSERT INTO _fedengine.meta_pg_foreign_table(_snapshot_id,_catalog,_schema,_object,ftrelid,ftserver) SELECT {id},{alias},{schema},object,local_oid,? FROM __fed_foreign_relations"), [server]).map_err(engine_error)?;
        conn.execute_batch("DROP TABLE __fed_foreign_relations")
            .map_err(engine_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_mapping_preserves_logical_facts_without_source_storage_or_definitions() {
        let config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        let page: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/metadata/published-pg16-directory.json"
        ))
        .unwrap();
        let mut source = CatalogSnapshot {
            id: "source".into(),
            incarnation: "source-epoch".into(),
            revision: 1,
            generation: 1,
            database_oid: 200000000,
            schema_oids: [("sales".into(), 200000001)].into(),
            refreshed_at: "now".into(),
            complete: true,
            stale: false,
            structure_digest: "v1".into(),
            schemas: vec!["sales".into()],
            objects: serde_json::from_value(page["objects"].clone()).unwrap(),
        };
        let object = source
            .objects
            .iter_mut()
            .find(|o| o.name == "customers")
            .unwrap();
        object.metadata.dropped_attnums.push(4);
        object.metadata.comment = Some("table comment".into());
        let properties = object.metadata.columns.get_mut("name").unwrap();
        properties.comment = Some("column comment".into());
        properties.identity = Some("a".into());
        properties.generated =
            MetadataExpression::source("'generated'".into(), DataSourceKind::Postgres);
        // Exercise a source type outside the built-in PG type set.
        properties.type_oid = 210000000;
        object.metadata.column_statistics.insert(
            "name".into(),
            ColumnStatistics {
                null_frac: Some(serde_json::Number::from_f64(0.25).unwrap()),
                avg_width: Some(12),
                n_distinct: Some(5.into()),
            },
        );
        object.metadata.storage.main_bytes = Some(8192);
        object.metadata.activity.seq_scan = Some(42);
        super::super::publish(&conn, &source, "remote").unwrap();
        let mut mapped = source.clone();
        mapped.id = "mapped".into();
        mapped.objects.clear();
        mapped.schema_oids = [("crm".into(), 200000002)].into();
        mapped.schemas = vec!["crm".into()];
        let binding = VirtualSchema {
            config: VirtualSchemaConfig {
                name: "crm".into(),
                source: DataSourceId("remote".into()),
                remote_schema: "sales".into(),
            },
            oid: 200000002,
            snapshot_id: None,
        };
        let epoch = format!("virtual-schema:{}", binding.oid);
        conn.execute_batch("BEGIN TRANSACTION").unwrap();
        let mut oids = OidBatch::load(&conn, &[epoch]).unwrap();
        ForeignProjection {
            snapshot: &mapped,
            source: &source,
            alias: "account",
            binding: &binding,
        }
        .publish(&conn, &mut oids)
        .unwrap();
        oids.flush().unwrap();
        conn.execute_batch("COMMIT").unwrap();
        let (local,kind,has_index,checks): (u32,String,bool,i16) = conn.query_row("SELECT oid,relkind,relhasindex,relchecks FROM _fedengine.meta_pg_class WHERE _snapshot_id='mapped' AND relname='customers'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_ne!(
            local,
            source
                .objects
                .iter()
                .find(|o| o.name == "customers")
                .unwrap()
                .metadata
                .oid
        );
        assert_eq!((kind.as_str(), has_index, checks), ("f", false, 0));
        let (attributes,foreign_attributes): (i64,i64) = conn.query_row("SELECT count(*),count(*) FILTER (WHERE attrelid<>?) FROM _fedengine.meta_pg_attribute WHERE _snapshot_id='mapped' AND _object='customers'", [local], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((attributes, foreign_attributes), (4, 0)); // Three columns plus dropped; no index attributes.
        let (has_default,identity,generated): (bool,String,String) = conn.query_row("SELECT atthasdef,attidentity,attgenerated FROM _fedengine.meta_pg_attribute WHERE _snapshot_id='mapped' AND _object='customers' AND attname='name'", [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(
            (has_default, identity.as_str(), generated.as_str()),
            (false, "", "")
        );
        let info: (String,String,Option<String>,String,String,Option<String>) = conn.query_row("SELECT table_catalog,table_schema,column_default,is_identity,is_generated,generation_expression FROM _fedengine.meta_information_schema_columns WHERE _snapshot_id='mapped' AND table_name='customers' AND column_name='name'", [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(
            info,
            (
                "account".into(),
                "crm".into(),
                None,
                "NO".into(),
                "NEVER".into(),
                None
            )
        );
        let comments: i64 = conn.query_row("SELECT count(*) FROM _fedengine.meta_pg_description WHERE _snapshot_id='mapped' AND objoid=? AND description IN ('table comment','column comment')", [local], |r|r.get(0)).unwrap();
        assert_eq!(comments, 2);
        let stats: (String,i32,f32) = conn.query_row("SELECT schemaname,avg_width,null_frac FROM _fedengine.meta_pg_stats WHERE _snapshot_id='mapped' AND tablename='customers' AND attname='name'", [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(stats, ("crm".into(), 12, 0.25));
        let type_namespace: u32 = conn.query_row("SELECT typnamespace FROM _fedengine.meta_pg_type WHERE _snapshot_id='mapped' AND oid=210000000",[],|r|r.get(0)).unwrap();
        assert_eq!(type_namespace, binding.oid);
        for relation in [
            "pg_constraint",
            "pg_index",
            "pg_attrdef",
            "pg_indexes",
            "pg_stat_all_tables",
            "pg_stat_all_indexes",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT count(*) FROM {} WHERE _snapshot_id='mapped'",
                        physical_catalog(relation).unwrap()
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "{relation}");
        }
        let (storage,activity): (String,String) = conn.query_row("SELECT storage,activity FROM _fedengine.metadata_relation_statistics WHERE snapshot_id='mapped' AND oid=?",[local],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(
            serde_json::from_str::<RelationStorage>(&storage).unwrap(),
            RelationStorage::default()
        );
        assert!(serde_json::from_str::<Value>(&activity).unwrap()["scan_count"].is_null());
    }
}
