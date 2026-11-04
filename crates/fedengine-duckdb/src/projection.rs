//! Local PostgreSQL catalog projections, committed with the corresponding immutable snapshot.
use crate::error::engine_error;
use duckdb::{Connection, appender_params_from_iter, params};
use fedengine_core::*;
use fedengine_pg_compat::{builtin_types, catalog_schema, physical_catalog};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

struct Row {
    snapshot: String,
    catalog: String,
    schema: String,
    object: String,
    target_schema: Option<String>,
    target_object: Option<String>,
    value: Value,
}
type Rows = BTreeMap<String, Vec<Row>>;
fn push(
    rows: &mut Rows,
    relation: &str,
    snapshot: &str,
    catalog: &str,
    schema: &str,
    object: &str,
    value: Value,
) {
    rows.entry(relation.into()).or_default().push(Row {
        snapshot: snapshot.into(),
        catalog: catalog.into(),
        schema: schema.into(),
        object: object.into(),
        target_schema: None,
        target_object: None,
        value,
    });
}
fn insert(conn: &Connection, rows: Rows) -> Result<()> {
    // Append text facts in vectors, then cast each catalog in a single statement.
    // Keep this inside the caller's publication transaction: a failed cast cannot
    // expose a partial snapshot, and JSON arrays retain their element types.
    for (relation, rows) in rows {
        let fields = &catalog_schema()[&relation];
        let names = fields
            .iter()
            .map(|f| quote_identifier(&f.attname))
            .collect::<Vec<_>>();
        let definitions = names
            .iter()
            .map(|n| format!("{n} VARCHAR"))
            .collect::<Vec<_>>()
            .join(",");
        conn.execute_batch(&format!("CREATE OR REPLACE TEMP TABLE __fed_projection_stage (_snapshot_id VARCHAR,_catalog VARCHAR,_schema VARCHAR,_object VARCHAR,_target_schema VARCHAR,_target_object VARCHAR,{definitions})")).map_err(engine_error)?;
        {
            let mut appender = conn
                .appender_to_db("__fed_projection_stage", "main")
                .map_err(engine_error)?;
            for row in rows {
                let mut values = vec![
                    Some(row.snapshot),
                    Some(row.catalog),
                    Some(row.schema),
                    Some(row.object),
                    row.target_schema,
                    row.target_object,
                ];
                values.extend(fields.iter().map(|f| match &row.value[&f.attname] {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    v => Some(v.to_string()),
                }));
                appender
                    .append_row(appender_params_from_iter(values))
                    .map_err(engine_error)?;
            }
            appender.flush().map_err(engine_error)?;
        }
        let casts = fields
            .iter()
            .zip(&names)
            .map(|(f, n)| {
                let t = f.duckdb_type();
                if let Some(base) = t.strip_suffix("[]") {
                    format!(
                        "from_json({n}, {})",
                        quote_literal(&format!("[\"{base}\"]"))
                    )
                } else {
                    format!("CAST({n} AS {t})")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        conn.execute_batch(&format!("INSERT INTO {} SELECT _snapshot_id,_catalog,_schema,_object,_target_schema,_target_object,{casts} FROM __fed_projection_stage; DROP TABLE __fed_projection_stage", physical_catalog(&relation)?)).map_err(engine_error)?;
    }
    Ok(())
}
pub(super) fn initialize(conn: &Connection) -> Result<()> {
    for (name, fields) in catalog_schema() {
        let fields = fields
            .iter()
            .map(|f| format!("{} {}", quote_identifier(&f.attname), f.duckdb_type()))
            .collect::<Vec<_>>()
            .join(",");
        conn.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {}(_snapshot_id VARCHAR NOT NULL,_catalog VARCHAR NOT NULL,_schema VARCHAR NOT NULL,_object VARCHAR NOT NULL,_target_schema VARCHAR,_target_object VARCHAR,{fields})",physical_catalog(name)?)).map_err(engine_error)?;
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS _fedengine.metadata_definitions(snapshot_id VARCHAR NOT NULL,oid UINTEGER NOT NULL,kind VARCHAR NOT NULL,definition VARCHAR,items VARCHAR[],schema_name VARCHAR NOT NULL,object_name VARCHAR NOT NULL,target_schema VARCHAR,target_object VARCHAR,dependencies_known BOOLEAN NOT NULL DEFAULT false);
        CREATE TABLE IF NOT EXISTS _fedengine.metadata_relation_statistics(snapshot_id VARCHAR NOT NULL,oid UINTEGER NOT NULL,schema_name VARCHAR NOT NULL,object_name VARCHAR NOT NULL,storage VARCHAR NOT NULL,activity VARCHAR NOT NULL);
        CREATE TABLE IF NOT EXISTS _fedengine.metadata_view_dependencies(snapshot_id VARCHAR NOT NULL,oid UINTEGER NOT NULL,schema_name VARCHAR NOT NULL,object_name VARCHAR NOT NULL);
        CREATE TABLE IF NOT EXISTS _fedengine.metadata_projection_state(snapshot_id VARCHAR PRIMARY KEY);")
        .map_err(engine_error)?;
    // Built-in contracts are deterministic current-version data. Rebuild them
    // on startup instead of retaining historical fixture versions or upgrades.
    for name in catalog_schema().keys() {
        conn.execute(
            &format!(
                "DELETE FROM {} WHERE _snapshot_id=''",
                physical_catalog(name)?
            ),
            [],
        )
        .map_err(engine_error)?;
    }
    let mut rows = Rows::new();
    for (oid, name) in [(11, "pg_catalog"), (13207, "information_schema")] {
        push(
            &mut rows,
            "pg_namespace",
            "",
            "",
            "",
            "",
            json!({"oid":oid,"nspname":name,"nspowner":0}),
        );
    }
    for value in builtin_types() {
        let mut value = value.clone();
        value["typowner"] = json!(0);
        value["typacl"] = Value::Null;
        push(&mut rows, "pg_type", "", "", "", "", value);
    }
    for (name, fields) in catalog_schema() {
        let oid = fields[0].oid;
        let (schema, relname) = if let Some((_, n)) = name.split_once('.') {
            (13207, n)
        } else {
            (11, name.as_str())
        };
        push(
            &mut rows,
            "pg_class",
            "",
            "",
            "",
            "",
            json!({"oid":oid,"relname":relname,"relnamespace":schema,"relkind":"r","relpersistence":"p","relowner":0,"relnatts":fields.len(),"relhasindex":false,"relchecks":0,"relispartition":false,"relrowsecurity":false,"relforcerowsecurity":false,"relispopulated":true}),
        );
        for f in fields {
            push(
                &mut rows,
                "pg_attribute",
                "",
                "",
                "",
                "",
                json!({"attrelid":oid,"attname":f.attname,"atttypid":f.atttypid,"attnum":f.attnum,"atttypmod":f.atttypmod,"attnotnull":false,"attisdropped":false,"attidentity":"","attgenerated":"","attndims":if f.duckdb_type().ends_with("[]"){1}else{0},"atthasdef":false,"attislocal":true,"attinhcount":0,"attlen":type_len(f.atttypid)}),
            );
        }
    }
    for (oid, name, t) in [
        (403, "btree", "i"),
        (405, "hash", "i"),
        (783, "gist", "i"),
        (2742, "gin", "i"),
        (4000, "spgist", "i"),
        (3580, "brin", "i"),
        (2, "heap", "t"),
    ] {
        push(
            &mut rows,
            "pg_am",
            "",
            "",
            "",
            "",
            json!({"oid":oid,"amname":name,"amtype":t}),
        );
    }
    insert(conn, rows)?;
    Ok(())
}
fn type_len(oid: u32) -> i16 {
    builtin_types()
        .iter()
        .find(|v| v["oid"].as_u64() == Some(oid.into()))
        .and_then(|v| v["typlen"].as_i64())
        .and_then(|n| n.try_into().ok())
        .unwrap_or(-1)
}
fn definition(
    conn: &Connection,
    snapshot: &str,
    identity: (u32, &str),
    text: Option<&str>,
    items: &[Option<String>],
    object: &CatalogObject,
    target: Option<&MetadataForeignKey>,
) -> Result<()> {
    let (oid, kind) = identity;
    conn.execute("INSERT INTO _fedengine.metadata_definitions(snapshot_id,oid,kind,definition,items,schema_name,object_name,target_schema,target_object) VALUES (?,?,?,?,from_json(?, '[\"VARCHAR\"]'),?,?,?,?)",params![snapshot,oid,kind,text,serde_json::to_string(items).map_err(|_|EngineError::new("XX000","definition serialization failed"))?,object.schema,object.name,target.map(|t|&t.schema),target.map(|t|&t.table)]).map_err(engine_error)?;
    Ok(())
}
fn pg_text(e: Option<&MetadataExpression>) -> Option<&str> {
    e.and_then(|e| e.pg_text.as_deref())
}
fn am(name: Option<&str>) -> Option<u32> {
    Some(match name? {
        "btree" => 403,
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => return None,
    })
}
fn action(value: Option<&str>) -> Option<&str> {
    Some(match value? {
        "NO ACTION" | "a" => "a",
        "RESTRICT" | "r" => "r",
        "CASCADE" | "c" => "c",
        "SET NULL" | "n" => "n",
        "SET DEFAULT" | "d" => "d",
        _ => return None,
    })
}
fn attnums(object: &CatalogObject, names: &[String]) -> Option<Vec<i16>> {
    names
        .iter()
        .map(|n| object.metadata.columns.get(n).map(|c| c.attnum))
        .collect()
}
pub(super) fn publish(conn: &Connection, snapshot: &CatalogSnapshot, alias: &str) -> Result<()> {
    let started = std::time::Instant::now();
    let id = &snapshot.id;
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM _fedengine.metadata_projection_state WHERE snapshot_id=?",
            [id],
            |r| r.get(0),
        )
        .map_err(engine_error)?;
    if exists != 0 {
        return Ok(());
    }
    {
        // Bulk append under the same publication transaction as the public catalogs.
        let mut appender = conn
            .appender_to_db("metadata_relation_statistics", "_fedengine")
            .map_err(engine_error)?;
        for object in &snapshot.objects {
            let storage = serde_json::to_string(&object.metadata.storage)
                .map_err(|_| EngineError::new("XX000", "invalid storage statistics"))?;
            let mut activity = serde_json::to_value(&object.metadata.activity)
                .map_err(|_| EngineError::new("XX000", "invalid activity statistics"))?;
            activity["scan_count"] = json!(object.metadata.activity.seq_scan);
            let activity = activity.to_string();
            use duckdb::types::Value as V;
            appender
                .append_row(appender_params_from_iter([
                    V::Text(id.clone()),
                    V::UInt(object.metadata.oid),
                    V::Text(object.schema.clone()),
                    V::Text(object.name.clone()),
                    V::Text(storage),
                    V::Text(activity),
                ]))
                .map_err(engine_error)?;
        }
        for object in &snapshot.objects {
            for index in &object.metadata.indexes {
                let storage = serde_json::to_string(&index.storage)
                    .map_err(|_| EngineError::new("XX000", "invalid index storage"))?;
                let mut activity = serde_json::to_value(&index.activity)
                    .map_err(|_| EngineError::new("XX000", "invalid index activity"))?;
                activity["scan_count"] = json!(index.activity.idx_scan);
                use duckdb::types::Value as V;
                appender
                    .append_row(appender_params_from_iter([
                        V::Text(id.clone()),
                        V::UInt(index.oid),
                        V::Text(object.schema.clone()),
                        V::Text(object.name.clone()),
                        V::Text(storage),
                        V::Text(activity.to_string()),
                    ]))
                    .map_err(engine_error)?;
            }
        }
        appender.flush().map_err(engine_error)?;
    }
    let mut rows = Rows::new();
    push(
        &mut rows,
        "pg_database",
        id,
        alias,
        "",
        "",
        json!({"oid":snapshot.database_oid,"datname":alias,"encoding":6,"datistemplate":false,"datallowconn":true,"datconnlimit":-1,"datcollate":"C","datctype":"C","datlocprovider":"c"}),
    );
    for (schema, oid) in &snapshot.schema_oids {
        push(
            &mut rows,
            "pg_namespace",
            id,
            alias,
            schema,
            "",
            json!({"oid":oid,"nspname":schema}),
        );
        push(
            &mut rows,
            "information_schema.schemata",
            id,
            alias,
            schema,
            "",
            json!({"catalog_name":alias,"schema_name":schema}),
        );
    }
    let targets: BTreeMap<_, _> = snapshot
        .objects
        .iter()
        .map(|o| ((&o.schema, &o.name), o))
        .collect();
    let builtin: BTreeSet<_> = builtin_types()
        .iter()
        .filter_map(|t| t["oid"].as_u64().map(|n| n as u32))
        .collect();
    let class_oid = catalog_schema()["pg_class"][0].oid;
    for object in &snapshot.objects {
        let oid = object.metadata.oid;
        let schema = &object.schema;
        let name = &object.name;
        let ns = snapshot.schema_oids[schema];
        let kind = match object.object_type.as_str() {
            "VIEW" => "v",
            "MATERIALIZED VIEW" => "m",
            "FOREIGN TABLE" => "f",
            _ => "r",
        };
        push(
            &mut rows,
            "pg_class",
            id,
            alias,
            schema,
            name,
            json!({"oid":oid,"relname":name,"relnamespace":ns,"relkind":kind,"reltuples":object.metadata.statistics.estimated_rows,"relpages":object.metadata.statistics.pages,"relallvisible":object.metadata.statistics.all_visible_pages,"relpersistence":"p","relnatts":object.metadata.columns.values().map(|c|c.attnum).chain(object.metadata.dropped_attnums.iter().copied()).max().unwrap_or(0),"relhasindex":!object.metadata.indexes.is_empty(),"relchecks":object.metadata.constraints.iter().filter(|c|c.kind=="c").count(),"relispartition":false,"relrowsecurity":false,"relforcerowsecurity":false,"relispopulated":true,"relhasrules":false,"relhastriggers":false}),
        );
        push(
            &mut rows,
            "information_schema.tables",
            id,
            alias,
            schema,
            name,
            json!({"table_catalog":alias,"table_schema":schema,"table_name":name,"table_type":if kind=="v"{"VIEW"}else if kind=="f"{"FOREIGN"}else{"BASE TABLE"},"is_insertable_into":Value::Null,"is_typed":"NO"}),
        );
        if kind != "v" && kind != "m" && kind != "f" {
            push(
                &mut rows,
                "pg_tables",
                id,
                alias,
                schema,
                name,
                json!({"schemaname":schema,"tablename":name,"hasindexes":!object.metadata.indexes.is_empty(),"hasrules":false,"hastriggers":false,"rowsecurity":false}),
            );
        }
        if let Some(comment) = &object.metadata.comment {
            push(
                &mut rows,
                "pg_description",
                id,
                alias,
                schema,
                name,
                json!({"objoid":oid,"classoid":class_oid,"objsubid":0,"description":comment}),
            );
        }
        if matches!(kind, "v" | "m") {
            let text = pg_text(object.metadata.view_definition.as_ref());
            let relation = if kind == "v" {
                "pg_views"
            } else {
                "pg_matviews"
            };
            push(
                &mut rows,
                relation,
                id,
                alias,
                schema,
                name,
                json!({"schemaname":schema,"viewname":name,"matviewname":name,"definition":text,"hasindexes":!object.metadata.indexes.is_empty(),"ispopulated":true}),
            );
            push(
                &mut rows,
                "information_schema.views",
                id,
                alias,
                schema,
                name,
                json!({"table_catalog":alias,"table_schema":schema,"table_name":name,"view_definition":text}),
            );
            definition(conn, id, (oid, "view"), text, &[], object, None)?;
            if let Some(deps) = &object.metadata.view_dependencies {
                let known = deps
                    .iter()
                    .all(|d| targets.contains_key(&(&d.schema, &d.table)));
                conn.execute("UPDATE _fedengine.metadata_definitions SET dependencies_known=? WHERE snapshot_id=? AND oid=? AND kind='view'",params![known,id,oid]).map_err(engine_error)?;
                for dep in deps {
                    conn.execute(
                        "INSERT INTO _fedengine.metadata_view_dependencies VALUES (?,?,?,?)",
                        params![id, oid, dep.schema, dep.table],
                    )
                    .map_err(engine_error)?;
                }
            }
        }
        for (position, col) in object.columns.iter().enumerate() {
            let props = &object.metadata.columns[&col.name];
            let typ = props.type_oid;
            let default = pg_text(props.generated.as_ref().or(props.default.as_ref()));
            push(
                &mut rows,
                "pg_attribute",
                id,
                alias,
                schema,
                name,
                json!({"attrelid":oid,"attname":col.name,"atttypid":typ,"attnum":props.attnum,"atttypmod":props.type_modifier,"attnotnull":!col.nullable,"attisdropped":false,"attidentity":match props.identity.as_deref(){Some("a")=>"a",Some(_)=>"d",None=>""},"attgenerated":if props.generated.is_some(){"s"}else{""},"attndims":0,"atthasdef":default.is_some(),"attislocal":true,"attinhcount":0,"attlen":type_len(typ),"attcacheoff":-1}),
            );
            let typename = builtin_types()
                .iter()
                .find(|t| t["oid"].as_u64() == Some(typ.into()))
                .and_then(|t| t["typname"].as_str())
                .unwrap_or(&col.remote_type);
            push(
                &mut rows,
                "information_schema.columns",
                id,
                alias,
                schema,
                name,
                json!({"table_catalog":alias,"table_schema":schema,"table_name":name,"column_name":col.name,"ordinal_position":position+1,"column_default":default,"is_nullable":if col.nullable{"YES"}else{"NO"},"data_type":typename,"udt_catalog":alias,"udt_schema":if builtin.contains(&typ){"pg_catalog"}else{schema},"udt_name":typename,"is_identity":if props.identity.is_some(){"YES"}else{"NO"},"is_generated":if props.generated.is_some(){"ALWAYS"}else{"NEVER"},"generation_expression":pg_text(props.generated.as_ref())}),
            );
            if !builtin.contains(&typ) {
                push(
                    &mut rows,
                    "pg_type",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"oid":typ,"typname":col.remote_type,"typnamespace":ns,"typlen":-1,"typtype":"b","typcategory":"U","typisdefined":true,"typrelid":0,"typelem":0,"typbasetype":0,"typtypmod":-1,"typnotnull":false}),
                );
            }
            if let Some(comment) = &props.comment {
                push(
                    &mut rows,
                    "pg_description",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"objoid":oid,"classoid":class_oid,"objsubid":props.attnum,"description":comment}),
                );
            }
            if let Some(default) = default {
                // A default's identity is its table/column identity; its OID is separately allocated.
                push(
                    &mut rows,
                    "pg_attrdef",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"oid":props.default_oid,"adrelid":oid,"adnum":props.attnum,"adbin":default}),
                );
            }
        }
        for num in &object.metadata.dropped_attnums {
            push(
                &mut rows,
                "pg_attribute",
                id,
                alias,
                schema,
                name,
                json!({"attrelid":oid,"attname":format!("........pg.dropped.{num}........"),"atttypid":0,"attnum":num,"atttypmod":-1,"attnotnull":false,"attisdropped":true,"attidentity":"","attgenerated":"","atthasdef":false}),
            );
        }
        if matches!(kind, "r" | "m" | "p") {
            let mut activity = serde_json::to_value(&object.metadata.activity)
                .map_err(|_| EngineError::new("XX000", "invalid table activity"))?;
            activity["relid"] = json!(oid);
            activity["schemaname"] = json!(schema);
            activity["relname"] = json!(name);
            for view in ["pg_stat_all_tables", "pg_stat_user_tables"] {
                push(&mut rows, view, id, alias, schema, name, activity.clone());
            }
        }
        for (column, statistics) in &object.metadata.column_statistics {
            if statistics.null_frac.is_some()
                || statistics.avg_width.is_some()
                || statistics.n_distinct.is_some()
            {
                let mut value = serde_json::to_value(statistics)
                    .map_err(|_| EngineError::new("XX000", "invalid column statistics"))?;
                value["schemaname"] = json!(schema);
                value["tablename"] = json!(name);
                value["attname"] = json!(column);
                value["inherited"] = json!(false);
                push(&mut rows, "pg_stats", id, alias, schema, name, value);
            }
        }
        for index in &object.metadata.indexes {
            let mut activity = serde_json::to_value(&index.activity)
                .map_err(|_| EngineError::new("XX000", "invalid index activity"))?;
            activity["relid"] = json!(oid);
            activity["indexrelid"] = json!(index.oid);
            activity["schemaname"] = json!(schema);
            activity["relname"] = json!(name);
            activity["indexrelname"] = json!(index.name);
            for view in ["pg_stat_all_indexes", "pg_stat_user_indexes"] {
                push(&mut rows, view, id, alias, schema, name, activity.clone());
            }
            let keys: Vec<_> = index
                .keys
                .iter()
                .map(|k| {
                    k.column
                        .as_ref()
                        .and_then(|n| object.metadata.columns.get(n))
                        .map(|c| c.attnum)
                        .unwrap_or(0)
                })
                .chain(
                    index
                        .include
                        .iter()
                        .filter_map(|n| object.metadata.columns.get(n).map(|c| c.attnum)),
                )
                .collect();
            let options: Option<Vec<i16>> = index
                .keys
                .iter()
                .map(|k| Some(i16::from(k.descending?) + 2 * i16::from(k.nulls_first?)))
                .collect();
            let all = keys.len();
            push(
                &mut rows,
                "pg_class",
                id,
                alias,
                schema,
                name,
                json!({"oid":index.oid,"relname":index.name,"relnamespace":ns,"relkind":"i","relpersistence":"p","relnatts":all,"relam":am(index.method.as_deref()),"relispartition":false}),
            );
            push(
                &mut rows,
                "pg_index",
                id,
                alias,
                schema,
                name,
                json!({"indexrelid":index.oid,"indrelid":oid,"indnatts":all,"indnkeyatts":index.keys.len(),"indisunique":index.unique,"indisprimary":index.primary,"indisvalid":index.valid,"indisready":index.enabled,"indnullsnotdistinct":index.nulls_not_distinct,"indkey":keys,"indoption":options,"indexprs":index.keys.iter().filter_map(|k|pg_text(k.expression.as_ref())).collect::<Vec<_>>().join(", "),"indpred":pg_text(index.predicate.as_ref())}),
            );
            for (position, key) in index
                .keys
                .iter()
                .map(|k| k.column.as_ref())
                .chain(index.include.iter().map(Some))
                .enumerate()
            {
                let column = key.and_then(|n| object.metadata.columns.get(n));
                push(
                    &mut rows,
                    "pg_attribute",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"attrelid":index.oid,"attname":key.map(String::as_str).unwrap_or("expression"),"atttypid":column.map(|c|c.type_oid).unwrap_or(0),"attnum":position+1,"atttypmod":column.map(|c|c.type_modifier).unwrap_or(-1),"attnotnull":false,"attisdropped":false,"attidentity":"","attgenerated":"","atthasdef":false,"attislocal":true,"attinhcount":0}),
                );
            }
            let text = pg_text(index.definition.as_ref());
            push(
                &mut rows,
                "pg_indexes",
                id,
                alias,
                schema,
                name,
                json!({"schemaname":schema,"tablename":name,"indexname":index.name,"indexdef":text}),
            );
            let items: Vec<_> = index
                .keys
                .iter()
                .map(|k| {
                    k.column
                        .as_ref()
                        .map(|c| quote_identifier(c))
                        .or_else(|| pg_text(k.expression.as_ref()).map(str::to_owned))
                })
                .chain(index.include.iter().map(|c| Some(quote_identifier(c))))
                .collect();
            definition(conn, id, (index.oid, "index"), text, &items, object, None)?;
        }
        for constraint in &object.metadata.constraints {
            let target = constraint
                .foreign_key
                .as_ref()
                .and_then(|f| targets.get(&(&f.schema, &f.table)).copied());
            if constraint.kind == "f" && target.is_none() {
                continue;
            }
            let keys = attnums(object, &constraint.columns);
            let fkkeys = target.and_then(|o| {
                constraint
                    .foreign_key
                    .as_ref()
                    .and_then(|f| attnums(o, &f.columns))
            });
            let backing = constraint
                .backing_index
                .as_ref()
                .and_then(|n| {
                    target
                        .unwrap_or(object)
                        .metadata
                        .indexes
                        .iter()
                        .find(|i| i.name == *n)
                })
                .map(|i| i.oid);
            let text = pg_text(constraint.definition.as_ref());
            let fk = constraint.foreign_key.as_ref();
            let unique: Vec<_> = target
                .into_iter()
                .flat_map(|t| &t.metadata.constraints)
                .filter(|c| {
                    matches!(c.kind.as_str(), "p" | "u")
                        && fk.is_some_and(|f| {
                            c.columns.len() == f.columns.len()
                                && c.columns.iter().all(|n| f.columns.contains(n))
                        })
                        && constraint
                            .backing_index
                            .as_ref()
                            .is_none_or(|name| c.backing_index.as_ref() == Some(name))
                })
                .collect();
            push(
                &mut rows,
                "pg_constraint",
                id,
                alias,
                schema,
                name,
                json!({"oid":constraint.oid,"conname":constraint.name,"connamespace":ns,"contype":constraint.kind,"conrelid":oid,"contypid":0,"conindid":backing,"confrelid":target.map(|o|o.metadata.oid).unwrap_or(0),"conkey":keys,"confkey":fkkeys,"condeferrable":constraint.deferrable,"condeferred":constraint.initially_deferred,"convalidated":constraint.validated,"confupdtype":action(fk.and_then(|f|f.on_update.as_deref())),"confdeltype":action(fk.and_then(|f|f.on_delete.as_deref())),"confmatchtype":fk.and_then(|f|f.match_type.as_deref()).map(|v|match v{"FULL"|"f"=>"f","PARTIAL"|"p"=>"p",_=>"s"}),"conbin":if constraint.kind=="c"{text.and_then(|t|t.strip_prefix("CHECK "))}else{None},"conislocal":true,"coninhcount":0,"connoinherit":false}),
            );
            if let Some(f) = fk {
                let row = rows.get_mut("pg_constraint").unwrap().last_mut().unwrap();
                row.target_schema = Some(f.schema.clone());
                row.target_object = Some(f.table.clone());
            }
            definition(
                conn,
                id,
                (constraint.oid, "constraint"),
                text,
                &[],
                object,
                fk,
            )?;
            let ctype = match constraint.kind.as_str() {
                "p" => "PRIMARY KEY",
                "u" => "UNIQUE",
                "f" => "FOREIGN KEY",
                "c" => "CHECK",
                _ => continue,
            };
            push(
                &mut rows,
                "information_schema.table_constraints",
                id,
                alias,
                schema,
                name,
                json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"table_catalog":alias,"table_schema":schema,"table_name":name,"constraint_type":ctype,"is_deferrable":constraint.deferrable.map(|b|if b{"YES"}else{"NO"}),"initially_deferred":constraint.initially_deferred.map(|b|if b{"YES"}else{"NO"}),"enforced":constraint.enforced.map(|b|if b{"YES"}else{"NO"})}),
            );
            for (position, column) in constraint.columns.iter().enumerate() {
                push(
                    &mut rows,
                    "information_schema.key_column_usage",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"table_catalog":alias,"table_schema":schema,"table_name":name,"column_name":column,"ordinal_position":position+1,"position_in_unique_constraint":fk.and_then(|f| if unique.len()==1 {unique[0].columns.iter().position(|n| n==&f.columns[position]).map(|n|n+1)}else{None})}),
                );
            }
            let usage = target.unwrap_or(object);
            let used_columns = fk.map(|f| &f.columns).unwrap_or(&constraint.columns);
            for column in used_columns {
                push(
                    &mut rows,
                    "information_schema.constraint_column_usage",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"table_catalog":alias,"table_schema":usage.schema,"table_name":usage.name,"column_name":column}),
                );
            }
            push(
                &mut rows,
                "information_schema.constraint_table_usage",
                id,
                alias,
                schema,
                name,
                json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"table_catalog":alias,"table_schema":usage.schema,"table_name":usage.name}),
            );
            if let (Some(fk), Some(_target)) = (fk, target) {
                push(
                    &mut rows,
                    "information_schema.referential_constraints",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"unique_constraint_catalog":if unique.len()==1{Some(alias)}else{None},"unique_constraint_schema":if unique.len()==1{Some(&fk.schema)}else{None},"unique_constraint_name":if unique.len()==1{Some(&unique[0].name)}else{None},"match_option":fk.match_type,"update_rule":fk.on_update,"delete_rule":fk.on_delete}),
                );
            }
            if constraint.kind == "c" {
                push(
                    &mut rows,
                    "information_schema.check_constraints",
                    id,
                    alias,
                    schema,
                    name,
                    json!({"constraint_catalog":alias,"constraint_schema":schema,"constraint_name":constraint.name,"check_clause":text.and_then(|t|t.strip_prefix("CHECK "))}),
                );
            }
            // Every dependent standard row uses the same target visibility restriction.
            if let Some(f) = fk {
                for (relation, items) in &mut rows {
                    if relation.starts_with("information_schema.") {
                        for row in items
                            .iter_mut()
                            .rev()
                            .take_while(|r| r.object == *name && r.schema == *schema)
                        {
                            if row.value["constraint_name"].as_str() == Some(&constraint.name) {
                                row.target_schema = Some(f.schema.clone());
                                row.target_object = Some(f.table.clone());
                            }
                        }
                    }
                }
            }
        }
        // Flush complete object groups so FK visibility annotations remain atomic.
        if rows.values().map(Vec::len).sum::<usize>() >= 4096 {
            insert(conn, std::mem::take(&mut rows))?;
        }
    }
    insert(conn, rows)?;
    conn.execute(
        "INSERT INTO _fedengine.metadata_projection_state VALUES (?)",
        [id],
    )
    .map_err(engine_error)?;
    tracing::debug!(snapshot=%id, objects=snapshot.objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog local projection finished");
    Ok(())
}
/// Describe actual FedEngine external bindings without exposing credentials or remote indexes.
pub(super) fn foreign_binding(
    conn: &Connection,
    snapshot: &CatalogSnapshot,
    alias: &str,
    schema: &str,
    wrapper: u32,
    server: u32,
) -> Result<()> {
    let mut rows = Rows::new();
    push(
        &mut rows,
        "pg_foreign_data_wrapper",
        &snapshot.id,
        alias,
        schema,
        "",
        json!({"oid":wrapper,"fdwname":format!("fedengine_{wrapper}"),"fdwhandler":0,"fdwvalidator":0}),
    );
    push(
        &mut rows,
        "pg_foreign_server",
        &snapshot.id,
        alias,
        schema,
        "",
        json!({"oid":server,"srvname":schema,"srvfdw":wrapper,"srvtype":"FedEngine schema binding"}),
    );
    for object in &snapshot.objects {
        push(
            &mut rows,
            "pg_foreign_table",
            &snapshot.id,
            alias,
            schema,
            &object.name,
            json!({"ftrelid":object.metadata.oid,"ftserver":server}),
        );
    }
    insert(conn, rows)
}
pub(super) fn remove(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM _fedengine.metadata_relation_statistics WHERE snapshot_id=?",
        [id],
    )
    .map_err(engine_error)?;
    for name in catalog_schema().keys() {
        conn.execute(
            &format!(
                "DELETE FROM {} WHERE _snapshot_id=?",
                physical_catalog(name)?
            ),
            [id],
        )
        .map_err(engine_error)?;
    }
    conn.execute(
        "DELETE FROM _fedengine.metadata_definitions WHERE snapshot_id=?",
        [id],
    )
    .map_err(engine_error)?;
    conn.execute(
        "DELETE FROM _fedengine.metadata_view_dependencies WHERE snapshot_id=?",
        [id],
    )
    .map_err(engine_error)?;
    conn.execute(
        "DELETE FROM _fedengine.metadata_projection_state WHERE snapshot_id=?",
        [id],
    )
    .map_err(engine_error)?;
    Ok(())
}
pub(super) fn prune(
    conn: &Connection,
    before: &ManagementState,
    after: &ManagementState,
) -> Result<()> {
    for id in before
        .catalogs
        .keys()
        .filter(|id| !after.catalogs.contains_key(*id))
    {
        remove(conn, id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::{params_from_iter, types::Value as DuckValue};
    use fedengine_contract::*;
    use fedengine_pg_compat::{CompatibilityPlan, plan_compatibility};
    #[test]
    fn current_builtin_projection_is_idempotent_and_preserves_business_identity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch("INSERT INTO _fedengine.meta_pg_database(_snapshot_id,_catalog,_schema,_object,oid,datname,datallowconn)
                VALUES ('business-snapshot','business','','',200000007,'business',true);
            INSERT INTO _fedengine.metadata_projection_state VALUES ('business-snapshot')").unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap(); // Idempotent across later restarts.
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: "business".into(),
            principal: "test".into(),
            catalogs: vec![],
        };
        let namespaces: Vec<String> = conn
            .prepare(&format!(
                "SELECT nspname FROM ({}) ORDER BY nspname",
                fedengine_pg_compat::scoped_catalog("pg_namespace", &scope).unwrap()
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(namespaces, ["information_schema", "pg_catalog"]);
        let oid: u32 = conn.query_row("SELECT oid FROM _fedengine.meta_pg_database WHERE _snapshot_id='business-snapshot'",[],|r|r.get(0)).unwrap();
        assert_eq!(oid, 200000007);
        // SQL enumeration has no invented entry, even when there are no visible sources.
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM ({})",
                    fedengine_pg_compat::scoped_catalog("pg_database", &scope).unwrap()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn jdbc_catalog_statements_bind_and_describe() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine; SET lambda_syntax='ENABLE_SINGLE_ARROW'")
            .unwrap();
        initialize(&conn).unwrap();
        let session = SessionContext {
            database: "pg_baseline".into(),
            ..Default::default()
        };
        let page: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/metadata/published-pg16-directory.json"
        ))
        .unwrap();
        let mut snapshot = CatalogSnapshot {
            id: "fixture".into(),
            incarnation: "fixture".into(),
            revision: 1,
            generation: 1,
            refreshed_at: "fixture".into(),
            complete: true,
            stale: false,
            structure_digest: "fixture".into(),
            database_oid: 200000000,
            schema_oids: [("sales".into(), 200000001)].into_iter().collect(),
            schemas: vec!["sales".into()],
            objects: serde_json::from_value(page["objects"].clone()).unwrap(),
        };
        for object in &mut snapshot.objects {
            object.metadata.storage = RelationStorage {
                main_bytes: Some(8192),
                fsm_bytes: Some(4096),
                vm_bytes: Some(0),
                init_bytes: Some(0),
                table_bytes: Some(12288),
                indexes_bytes: Some(4096),
                total_bytes: Some(16384),
                ..Default::default()
            };
            object.metadata.activity.seq_scan = Some(5);
        }
        publish(&conn, &snapshot, "pg_baseline").unwrap();
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: session.database.clone(),
            principal: "test".into(),
            catalogs: vec![VisibleCatalog {
                id: DataSourceId("baseline".into()),
                catalog: "pg_baseline".into(),
                snapshot_id: "fixture".into(),
                database_oid: snapshot.database_oid,
                schemas: snapshot.schemas.clone(),
                default_schema: "sales".into(),
                grants: vec![Grant {
                    principal: "test".into(),
                    source: DataSourceId("baseline".into()),
                    schema: "*".into(),
                    object: "*".into(),
                    actions: ["SELECT".into()].into_iter().collect(),
                }],
                stale: false,
                mapping: None,
            }],
        };
        let queries = include_str!("../../../tests/fixtures/metadata/jdbc-pg16-queries.txt");
        for line in queries.lines().filter(|s| s.starts_with("execute ")) {
            let sql = line.split_once(": ").unwrap().1;
            let CompatibilityPlan::Metadata(plan) =
                plan_compatibility(sql, &session, &scope, 1).unwrap()
            else {
                panic!("not metadata: {sql}")
            };
            if let Err(e) = crate::metadata::describe(
                &conn,
                &plan.sql,
                sql.matches('$').count(),
                Some(&plan.columns),
            ) {
                let mut st = conn.prepare(&format!("DESCRIBE {}", plan.sql)).unwrap();
                let n = st.parameter_count();
                let params = vec![DuckValue::Null; n];
                let types = st
                    .query_map(params_from_iter(params.iter()), |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap();
                panic!("{e:?} {types:?} {:?}", plan.columns);
            }
            let mut stmt = conn
                .prepare(&plan.sql)
                .unwrap_or_else(|e| panic!("{e}\n{}", plan.sql));
            let n = stmt.parameter_count();
            let values = (0..n)
                .map(|i| DuckValue::Text(if i == 0 { "sales" } else { "orders" }.into()))
                .collect::<Vec<_>>();
            let mut rows = stmt
                .query(params_from_iter(values.iter()))
                .unwrap_or_else(|e| panic!("{e}\n{}", plan.sql));
            while rows
                .next()
                .unwrap_or_else(|e| panic!("{e}\n{}", plan.sql))
                .is_some()
            {}
            drop(rows);
            drop(stmt);
            let mut stmt = conn.prepare(&plan.sql).unwrap();
            let mut stream = stmt.stream_arrow(params_from_iter(values.iter())).unwrap();
            for batch in &mut stream {
                let batch = batch.unwrap();
                for i in 0..batch.num_rows() {
                    for a in batch.columns() {
                        crate::codec::cell(a.as_ref(), i)
                            .unwrap_or_else(|e| panic!("{e:?} {:?} {}", a.data_type(), sql));
                    }
                }
            }
        }
        let gui: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/metadata/dbeaver-25.2.5-queries.json"
        ))
        .unwrap();
        for query in gui["queries"].as_array().unwrap() {
            let sql = query["sql"].as_str().unwrap();
            if !sql.to_ascii_lowercase().starts_with("select") {
                continue;
            }
            let CompatibilityPlan::Metadata(plan) = plan_compatibility(sql, &session, &scope, 1)
                .unwrap_or_else(|e| panic!("{e}: {sql}"))
            else {
                panic!("GUI query not metadata: {sql}")
            };
            let count = conn
                .prepare(&plan.sql)
                .unwrap_or_else(|e| panic!("{e}: {sql}"))
                .parameter_count();
            crate::metadata::describe(&conn, &plan.sql, count, Some(&plan.columns))
                .unwrap_or_else(|e| panic!("{e}: {sql}"));
            let mut stmt = conn.prepare(&plan.sql).unwrap();
            let params = vec![DuckValue::Null; count];
            let mut rows = stmt
                .query(params_from_iter(params.iter()))
                .unwrap_or_else(|e| panic!("{e}: {sql}"));
            while rows
                .next()
                .unwrap_or_else(|e| panic!("{e}: {sql}"))
                .is_some()
            {}
        }
        let mut limited = scope.clone();
        limited.catalogs[0].grants[0].schema = "sales".into();
        limited.catalogs[0].grants[0].object = "orders".into();
        let measurement = |sql: &str, scope: &MetadataScope| -> Option<i64> {
            let CompatibilityPlan::Metadata(p) =
                plan_compatibility(sql, &session, scope, 1).unwrap()
            else {
                panic!("not metadata");
            };
            assert_eq!(p.columns[0].as_ref().unwrap().data_type, PgType::Int8);
            crate::metadata::describe(&conn, &p.sql, 0, Some(&p.columns)).unwrap();
            conn.query_row(&p.sql, [], |r| r.get(0)).unwrap()
        };
        for (function, expected) in [
            ("pg_relation_size", 8192),
            ("pg_table_size", 12288),
            ("pg_indexes_size", 4096),
            ("pg_total_relation_size", 16384),
            ("pg_stat_get_numscans", 5),
        ] {
            if function != "pg_stat_get_numscans" {
                assert_eq!(
                    measurement(&format!("SELECT {function}('sales.orders')"), &scope),
                    Some(expected)
                );
            }
            assert_eq!(
                measurement(
                    &format!("SELECT {function}('sales.orders'::regclass)"),
                    &scope
                ),
                Some(expected)
            );
            let oid = snapshot
                .objects
                .iter()
                .find(|o| o.name == "customers")
                .unwrap()
                .metadata
                .oid;
            assert_eq!(
                measurement(&format!("SELECT {function}({oid})"), &limited),
                None
            );
            assert_eq!(
                measurement(&format!("SELECT {function}(NULL)"), &scope),
                None
            );
        }
        assert_eq!(
            measurement("SELECT pg_relation_size('sales.orders','fsm')", &scope),
            Some(4096)
        );
        assert_eq!(
            measurement("SELECT pg_relation_size('sales.orders','vm')", &scope),
            Some(0)
        );
        assert_eq!(
            measurement("SELECT pg_relation_size('sales.orders',NULL)", &scope),
            None
        );
        assert_eq!(
            measurement("SELECT pg_total_relation_size(0)", &scope),
            None
        );
        for (sql, check_scope) in [
            ("SELECT pg_total_relation_size('sales.missing')", &scope),
            ("SELECT pg_total_relation_size('sales.customers')", &limited),
            ("SELECT 'sales.missing'::regclass", &scope),
        ] {
            let CompatibilityPlan::Metadata(plan) =
                plan_compatibility(sql, &session, check_scope, 1).unwrap()
            else {
                panic!("not metadata");
            };
            let error = conn
                .query_row(&plan.sql, [], |r| r.get::<_, Option<i64>>(0))
                .unwrap_err();
            assert_eq!(engine_error(error).sqlstate, "42P01");
        }
        assert_eq!(
            measurement("SELECT pg_relation_size(NULL,'bad')", &scope),
            None
        );
        assert_eq!(
            measurement("SELECT pg_database_size(current_database())", &scope),
            None
        );
        let CompatibilityPlan::Metadata(invalid_fork) = plan_compatibility(
            "SELECT pg_relation_size('sales.orders','bad')",
            &session,
            &scope,
            1,
        )
        .unwrap() else {
            panic!("not metadata")
        };
        let error = conn
            .query_row(&invalid_fork.sql, [], |r| r.get::<_, Option<i64>>(0))
            .unwrap_err();
        assert_eq!(engine_error(error).sqlstate, "22023");
        for (sql, code) in [
            ("SELECT pg_table_size(1,2)", "42883"),
            (
                "SELECT * FROM _fedengine.metadata_relation_statistics",
                "42501",
            ),
        ] {
            assert_eq!(
                plan_compatibility(sql, &session, &scope, 1)
                    .err()
                    .unwrap()
                    .sqlstate,
                code
            );
        }
        let scalar = |sql: &str, scope: &MetadataScope| -> i64 {
            let CompatibilityPlan::Metadata(p) =
                plan_compatibility(sql, &session, scope, 1).unwrap()
            else {
                panic!("not metadata")
            };
            conn.query_row(&p.sql, [], |r| r.get(0)).unwrap()
        };
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_class WHERE relname='customers'",
                &limited
            ),
            0
        );
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_constraint WHERE contype='f'",
                &limited
            ),
            0
        );
        assert_eq!(
            scalar(
                "SELECT count(*) FROM information_schema.referential_constraints",
                &limited
            ),
            0
        );
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_constraint WHERE contype='f'",
                &scope
            ),
            1
        );
        limited.catalogs[0].grants[0].object = "order_totals".into();
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_views WHERE definition IS NOT NULL",
                &limited
            ),
            0
        );
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_views WHERE definition IS NOT NULL",
                &scope
            ),
            1
        );
        // A changed bound snapshot cannot accidentally read a newer projection.
        limited.catalogs[0].snapshot_id = "retired".into();
        assert_eq!(
            measurement("SELECT pg_table_size(200000002)", &limited),
            None
        );
        assert_eq!(
            scalar(
                "SELECT count(*) FROM pg_class WHERE relname='order_totals'",
                &limited
            ),
            0
        );
    }
    #[test]
    fn planner_statistics_keep_fractional_estimates_and_unknowns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        let mut rows = Rows::new();
        push(
            &mut rows,
            "pg_class",
            "stats",
            "test",
            "public",
            "t",
            json!({"oid":200000000,"relname":"t","reltuples":1.25,"relpages":7,"relallvisible":null}),
        );
        insert(&conn, rows).unwrap();
        let (tuples,pages,visible): (f32,i32,Option<i32>) = conn.query_row("SELECT reltuples,relpages,relallvisible FROM _fedengine.meta_pg_class WHERE _snapshot_id='stats'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(tuples, 1.25);
        assert_eq!(pages, 7);
        assert_eq!(visible, None);
        let stats: RelationStatistics =
            serde_json::from_value(json!({"estimated_rows":1.25})).unwrap();
        assert_eq!(
            serde_json::to_value(stats).unwrap()["estimated_rows"],
            json!(1.25)
        );
    }
    #[test]
    fn bulk_projection_failure_rolls_back_all_catalogs() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch("BEGIN TRANSACTION").unwrap();
        let mut rows = Rows::new();
        push(
            &mut rows,
            "pg_am",
            "failed",
            "test",
            "",
            "",
            json!({"oid":200000000,"amname":"test","amtype":"i"}),
        );
        push(
            &mut rows,
            "pg_attribute",
            "failed",
            "test",
            "",
            "",
            json!({"attnum":"invalid"}),
        );
        assert!(insert(&conn, rows).is_err());
        conn.execute_batch("ROLLBACK").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_am WHERE _snapshot_id='failed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
    #[test]
    #[ignore = "local catalog scale baseline; run explicitly with --ignored --nocapture"]
    fn catalog_scale_baseline() {
        let n = std::env::var("FEDENGINE_BENCH_OBJECTS")
            .ok()
            .map(|s| s.parse::<usize>().unwrap())
            .unwrap_or(1000);
        assert!((1..=10000).contains(&n));
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "SET threads=2; SET memory_limit='512MB'; CREATE SCHEMA _fedengine; BEGIN TRANSACTION",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let objects = (0..n)
            .map(|i| CatalogObject {
                schema: "sales".into(),
                name: format!("table_{i:05}"),
                object_type: "BASE TABLE".into(),
                indexes: vec![],
                columns: vec![CatalogColumn {
                    name: "id".into(),
                    ordinal: 1,
                    remote_type: "integer".into(),
                    mapped_type: Some("INTEGER".into()),
                    supported: true,
                    nullable: false,
                }],
                metadata: ObjectMetadata {
                    oid: 200000002 + i as u32,
                    columns: [(
                        "id".into(),
                        ColumnProperties {
                            attnum: 1,
                            type_oid: 23,
                            type_modifier: -1,
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            })
            .collect();
        let snapshot = CatalogSnapshot {
            id: "scale".into(),
            incarnation: "scale".into(),
            revision: 1,
            generation: 1,
            refreshed_at: "scale".into(),
            complete: true,
            stale: false,
            structure_digest: "scale".into(),
            database_oid: 200000000,
            schema_oids: [("sales".into(), 200000001)].into_iter().collect(),
            schemas: vec!["sales".into()],
            objects,
        };
        let started = std::time::Instant::now();
        publish(&conn, &snapshot, "scale").unwrap();
        conn.execute_batch("COMMIT").unwrap();
        let publish_ms = started.elapsed().as_secs_f64() * 1000.;
        let session = SessionContext {
            database: "scale".into(),
            ..Default::default()
        };
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: "scale".into(),
            principal: "bench".into(),
            catalogs: vec![VisibleCatalog {
                id: DataSourceId("scale".into()),
                catalog: "scale".into(),
                snapshot_id: "scale".into(),
                database_oid: 200000000,
                schemas: vec!["sales".into()],
                default_schema: "sales".into(),
                grants: vec![Grant {
                    principal: "bench".into(),
                    source: DataSourceId("scale".into()),
                    schema: "*".into(),
                    object: "*".into(),
                    actions: ["SELECT".into()].into_iter().collect(),
                }],
                stale: false,
                mapping: None,
            }],
        };
        let mut times = Vec::new();
        for _ in 0..30 {
            let start = std::time::Instant::now();
            let CompatibilityPlan::Metadata(p)=plan_compatibility("SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='sales' AND c.relkind='r'",&session,&scope,1).unwrap()else{panic!("not metadata")};
            assert_eq!(
                conn.query_row(&p.sql, [], |r| r.get::<_, i64>(0)).unwrap(),
                n as i64
            );
            times.push(start.elapsed().as_secs_f64() * 1000.);
        }
        times.sort_by(f64::total_cmp);
        let start = std::time::Instant::now();
        publish(&conn, &snapshot, "scale").unwrap();
        eprintln!(
            "catalog_baseline objects={n} columns={n} publish_ms={publish_ms:.2} unchanged_projection_ms={:.2} query_p50_ms={:.2} query_p95_ms={:.2} remote_requests=0",
            start.elapsed().as_secs_f64() * 1000.,
            times[15],
            times[28]
        );
    }
}
