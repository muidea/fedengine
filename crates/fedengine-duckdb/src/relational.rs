//! Bounded source-dialect collectors. Each category is applied only after full enumeration.
use crate::error::{engine_error, metadata_text};
use duckdb::Connection;
use fedengine_core::*;
use serde_json::Value;
use std::collections::BTreeMap;

type Objects = BTreeMap<(String, String), CatalogObject>;
type Row = (String, String, Value);
fn rows(
    conn: &Connection,
    source: &DataSourceDefinition,
    sql: &str,
    budget: usize,
) -> Result<Vec<Row>> {
    let sql = format!(
        "SELECT * FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(sql)
    );
    let mut stmt = conn.prepare(&sql).map_err(engine_error)?;
    let mut stream = stmt.query([]).map_err(engine_error)?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = stream.next().map_err(engine_error)? {
        let schema = metadata_text(row, 0)?;
        if !source.allowed_schemas.contains(&schema) {
            return Err(EngineError::new(
                "55000",
                "collector returned an out-of-scope schema",
            ));
        }
        let table = metadata_text(row, 1)?;
        let json = metadata_text(row, 2)?;
        bytes += json.len() + schema.len() + table.len();
        if out.len() >= 100_000 || bytes > budget {
            return Err(EngineError::new(
                "54000",
                "relational metadata result budget exceeded",
            ));
        }
        let mut value: Value = serde_json::from_str(&json)
            .map_err(|_| EngineError::new("55000", "invalid collector JSON"))?;
        if source.kind == DataSourceKind::Mysql {
            for field in [
                "unique",
                "primary",
                "valid",
                "enabled",
                "descending",
                "nulls_first",
            ] {
                if let Some(n) = value[field].as_u64() {
                    if n > 1 {
                        return Err(EngineError::new("55000", "invalid boolean collector fact"));
                    }
                    value[field] = Value::Bool(n == 1);
                }
            }
        }
        out.push((schema, table, value));
    }
    Ok(out)
}
fn string(v: &Value, name: &str) -> Option<String> {
    v.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v)
        .map_err(|_| EngineError::new("55000", "invalid relational metadata fact"))
}
fn expr(v: &Value, name: &str, kind: DataSourceKind) -> Option<MetadataExpression> {
    string(v, name)
        .and_then(|s| MetadataExpression::source(s, kind))
        .map(|mut e| {
            if v["_definition_safe"].as_bool() == Some(false) {
                e.pg_text = None;
                e.translation_status = TranslationStatus::Unsupported;
            }
            e
        })
}
// MySQL 5.7/8 catalog literal defaults carry values, not SQL string syntax.
// Translate only types whose literal representation is unambiguous; functions,
// generated expressions, binary and temporal defaults retain an unsupported state.
fn mysql_default(v: &Value) -> Option<MetadataExpression> {
    let text = v.get("_default")?.as_str()?;
    let mut expression = MetadataExpression::source(text.to_owned(), DataSourceKind::Mysql)
        .unwrap_or(MetadataExpression {
            source_text: String::new(),
            source_dialect: "mysql".into(),
            pg_text: None,
            translation_status: TranslationStatus::Unsupported,
        });
    let typ = v["_literal_type"].as_str().unwrap_or("");
    let pg = match typ {
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            Some(quote_literal(text))
        }
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal"
        | "numeric" | "float" | "double" => {
            let number = text.strip_prefix(['-', '+']).unwrap_or(text);
            let digits = number.bytes().filter(u8::is_ascii_digit).count();
            (digits > 0
                && number.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && number.bytes().filter(|b| *b == b'.').count() <= 1)
                .then(|| text.to_owned())
        }
        _ => None,
    };
    if let Some(pg) = pg {
        expression.pg_text = Some(pg);
        expression.translation_status = TranslationStatus::Translated;
    }
    Some(expression)
}
fn object<'a>(
    objects: &'a mut Objects,
    schema: &str,
    table: &str,
) -> Result<&'a mut CatalogObject> {
    objects
        .get_mut(&(schema.into(), table.into()))
        .ok_or_else(|| EngineError::new("55000", "remote DDL changed the scanned relation set"))
}
fn apply(
    objects: &mut Objects,
    category: &str,
    facts: Vec<Row>,
    kind: DataSourceKind,
) -> Result<()> {
    let mut groups: BTreeMap<(String, String, String), Vec<Value>> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for (schema, table, value) in facts {
        if !seen.insert((schema.clone(), table.clone()))
            && matches!(category, "details" | "statistics" | "storage" | "activity")
        {
            return Err(EngineError::new("55000", "duplicate relation fact"));
        }
        let obj = object(objects, &schema, &table)?;
        match category {
            "statistics" => {
                obj.metadata.statistics = decode(value)?;
            }
            "storage" => obj.metadata.storage = decode(value)?,
            "activity" => obj.metadata.activity = decode(value)?,
            "column_statistics" => {
                let name = string(&value, "name").ok_or_else(|| {
                    EngineError::new("55000", "column statistic has no column name")
                })?;
                if !obj.columns.iter().any(|c| c.name == name)
                    || obj
                        .metadata
                        .column_statistics
                        .insert(name, decode(value)?)
                        .is_some()
                {
                    return Err(EngineError::new(
                        "55000",
                        "duplicate or unknown column statistic",
                    ));
                }
            }
            "details" => {
                obj.metadata.source_id = string(&value, "source_id");
                obj.metadata.comment = string(&value, "comment");
                obj.metadata.view_definition = expr(&value, "_definition", kind);
                obj.metadata.view_dependencies = value
                    .get("dependencies")
                    .filter(|v| !v.is_null())
                    .map(|v| decode(v.clone()))
                    .transpose()?;
            }
            "column_properties" => {
                let name = string(&value, "name")
                    .ok_or_else(|| EngineError::new("55000", "column name missing"))?;
                if !obj.columns.iter().any(|c| c.name == name) {
                    return Err(EngineError::new(
                        "55000",
                        "remote DDL changed scanned columns",
                    ));
                }
                let mut props: ColumnProperties = decode(value.clone())?;
                props.default = if kind == DataSourceKind::Mysql {
                    mysql_default(&value)
                } else {
                    expr(&value, "_default", kind)
                };
                props.generated = expr(&value, "_generated", kind);
                if obj.metadata.columns.insert(name, props).is_some() {
                    return Err(EngineError::new("55000", "duplicate column fact"));
                }
            }
            _ => {
                let name = string(&value, "name")
                    .ok_or_else(|| EngineError::new("55000", "relational object name missing"))?;
                groups.entry((schema, table, name)).or_default().push(value);
            }
        }
    }
    for ((schema, table, _), items) in groups {
        let obj = object(objects, &schema, &table)?;
        let first = &items[0];
        if category == "constraints" {
            let mut fact: MetadataConstraint = decode(first.clone())?;
            fact.definition = expr(first, "_definition", kind);
            let referenced_schema = string(first, "referenced_schema");
            if let Some(schema) = referenced_schema {
                fact.foreign_key = Some(MetadataForeignKey {
                    schema,
                    table: string(first, "referenced_table")
                        .ok_or_else(|| EngineError::new("55000", "foreign key target missing"))?,
                    on_update: string(first, "on_update"),
                    on_delete: string(first, "on_delete"),
                    match_type: string(first, "match_type"),
                    ..Default::default()
                });
            }
            for (i, item) in items.iter().enumerate() {
                if let Some(column) = string(item, "column") {
                    if item["position"].as_u64() != Some((i + 1) as u64) {
                        return Err(EngineError::new(
                            "55000",
                            "constraint key order is inconsistent",
                        ));
                    }
                    fact.columns.push(column);
                    if let Some(fk) = &mut fact.foreign_key {
                        fk.columns
                            .push(string(item, "referenced_column").ok_or_else(|| {
                                EngineError::new("55000", "foreign key column missing")
                            })?);
                    }
                }
            }
            obj.metadata.constraints.push(fact);
        } else if category == "indexes" {
            let mut fact: MetadataIndex = decode(first.clone())?;
            fact.definition = expr(first, "_definition", kind);
            fact.predicate = expr(first, "_predicate", kind);
            for (i, item) in items.iter().enumerate() {
                if item["position"].as_u64() != Some((i + 1) as u64) {
                    return Err(EngineError::new("55000", "index key order is inconsistent"));
                }
                let column = string(item, "column");
                if item["included"].as_bool() == Some(true) {
                    fact.include.push(
                        column
                            .ok_or_else(|| EngineError::new("55000", "included column missing"))?,
                    );
                } else {
                    let mut key: MetadataIndexItem = decode(item.clone())?;
                    key.column = column;
                    key.expression = if key.column.is_none() {
                        expr(item, "_expression", kind)
                    } else {
                        None
                    };
                    fact.keys.push(key);
                }
            }
            obj.metadata.indexes.push(fact);
        }
    }
    if matches!(category, "details" | "statistics" | "storage" | "activity")
        && objects.keys().any(|k| !seen.contains(k))
    {
        return Err(EngineError::new(
            "55000",
            "relation detail enumeration is incomplete",
        ));
    }
    if category == "column_properties"
        && objects
            .values()
            .any(|o| o.metadata.columns.len() != o.columns.len())
    {
        return Err(EngineError::new(
            "55000",
            "column property enumeration is incomplete",
        ));
    }
    if category == "column_statistics"
        && objects
            .values()
            .any(|o| o.metadata.column_statistics.len() != o.columns.len())
    {
        return Err(EngineError::new(
            "55000",
            "column statistics enumeration is incomplete",
        ));
    }
    for obj in objects.values() {
        obj.metadata.validate(&obj.columns)?;
    }
    Ok(())
}
/// Move only the affected category aside. Failed decoding restores it without copying the catalog.
fn take_category(metadata: &mut ObjectMetadata, category: &str) -> ObjectMetadata {
    let mut out = ObjectMetadata::default();
    match category {
        "statistics" => out.statistics = std::mem::take(&mut metadata.statistics),
        "storage" => out.storage = std::mem::take(&mut metadata.storage),
        "activity" => out.activity = std::mem::take(&mut metadata.activity),
        "column_statistics" => {
            out.column_statistics = std::mem::take(&mut metadata.column_statistics)
        }
        "column_properties" => out.columns = std::mem::take(&mut metadata.columns),
        "constraints" => out.constraints = std::mem::take(&mut metadata.constraints),
        "indexes" => out.indexes = std::mem::take(&mut metadata.indexes),
        "details" => {
            out.source_id = metadata.source_id.take();
            out.comment = metadata.comment.take();
            out.view_definition = metadata.view_definition.take();
            out.view_dependencies = metadata.view_dependencies.take();
        }
        _ => unreachable!("known collector category"),
    }
    out
}
fn restore_category(metadata: &mut ObjectMetadata, mut old: ObjectMetadata, category: &str) {
    let _ = take_category(metadata, category);
    match category {
        "statistics" => metadata.statistics = old.statistics,
        "storage" => metadata.storage = old.storage,
        "activity" => metadata.activity = old.activity,
        "column_statistics" => metadata.column_statistics = old.column_statistics,
        "column_properties" => metadata.columns = old.columns,
        "constraints" => metadata.constraints = old.constraints,
        "indexes" => metadata.indexes = old.indexes,
        "details" => {
            metadata.source_id = old.source_id.take();
            metadata.comment = old.comment.take();
            metadata.view_definition = old.view_definition.take();
            metadata.view_dependencies = old.view_dependencies.take();
        }
        _ => unreachable!("known collector category"),
    }
}
fn apply_atomic(
    objects: &mut Objects,
    category: &str,
    facts: Vec<Row>,
    kind: DataSourceKind,
) -> Result<()> {
    let saved: Vec<_> = objects
        .values_mut()
        .map(|o| take_category(&mut o.metadata, category))
        .collect();
    let result = apply(objects, category, facts, kind);
    if result.is_err() {
        for (object, old) in objects.values_mut().zip(saved) {
            restore_category(&mut object.metadata, old, category);
        }
    }
    result
}
fn pg_queries(version: u32, condition: &str) -> Vec<(&'static str, String)> {
    let identity = if version >= 100000 {
        "nullif(a.attidentity::text,'')"
    } else {
        "NULL::text"
    };
    let generated = if version >= 120000 {
        "CASE WHEN a.attgenerated<>'' THEN pg_get_expr(d.adbin,d.adrelid) END"
    } else {
        "NULL::text"
    };
    let default = if version >= 120000 {
        "CASE WHEN a.attgenerated='' THEN pg_get_expr(d.adbin,d.adrelid) END"
    } else {
        "pg_get_expr(d.adbin,d.adrelid)"
    };
    let keys = if version >= 110000 {
        "i.indnkeyatts"
    } else {
        "i.indnatts"
    };
    let nulls = if version >= 150000 {
        "i.indnullsnotdistinct"
    } else {
        "false"
    };
    vec![
        (
            "storage",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('main_bytes',pg_relation_size(c.oid,'main'),'fsm_bytes',pg_relation_size(c.oid,'fsm'),'vm_bytes',pg_relation_size(c.oid,'vm'),'init_bytes',pg_relation_size(c.oid,'init'),'table_bytes',pg_table_size(c.oid),'indexes_bytes',pg_indexes_size(c.oid),'total_bytes',pg_total_relation_size(c.oid))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') AND {condition} ORDER BY 1,2"
            ),
        ),
        (
            "activity",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('seq_scan',s.seq_scan,'seq_tup_read',s.seq_tup_read,'idx_scan',s.idx_scan,'idx_tup_fetch',s.idx_tup_fetch,'n_tup_ins',s.n_tup_ins,'n_tup_upd',s.n_tup_upd,'n_tup_del',s.n_tup_del,'n_tup_hot_upd',s.n_tup_hot_upd,'n_live_tup',s.n_live_tup,'n_dead_tup',s.n_dead_tup)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_stat_all_tables s ON s.relid=c.oid WHERE c.relkind IN ('r','v','m','f','p') AND {condition} ORDER BY 1,2"
            ),
        ),
        (
            "statistics",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('estimated_rows',c.reltuples,'pages',c.relpages,'all_visible_pages',c.relallvisible)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') AND {condition} ORDER BY 1,2"
            ),
        ),
        (
            "details",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('source_id',c.oid::text,'comment',obj_description(c.oid,'pg_class'),'dependencies',CASE WHEN c.relkind IN ('v','m') AND NOT EXISTS (SELECT 1 FROM pg_rewrite xr JOIN pg_depend xd ON xd.classid='pg_rewrite'::regclass AND xd.objid=xr.oid WHERE xr.ev_class=c.oid AND xd.refclassid<>'pg_class'::regclass AND xd.deptype='n') THEN coalesce((SELECT json_agg(json_build_object('schema',dn.nspname,'table',dc.relname)) FROM (SELECT DISTINCT d.refobjid FROM pg_rewrite rw JOIN pg_depend d ON d.classid='pg_rewrite'::regclass AND d.objid=rw.oid AND d.refclassid='pg_class'::regclass WHERE rw.ev_class=c.oid AND d.refobjid<>c.oid) deps JOIN pg_class dc ON dc.oid=deps.refobjid JOIN pg_namespace dn ON dn.oid=dc.relnamespace),'[]'::json) END,'_definition',CASE WHEN c.relkind IN ('v','m') THEN pg_get_viewdef(c.oid,false) END)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') AND {condition} ORDER BY 1,2"
            ),
        ),
        (
            "column_statistics",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('name',a.attname,'null_frac',s.null_frac,'avg_width',s.avg_width,'n_distinct',s.n_distinct)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid LEFT JOIN pg_stats s ON s.schemaname=n.nspname AND s.tablename=c.relname AND s.attname=a.attname AND NOT s.inherited WHERE c.relkind IN ('r','v','m','f','p') AND a.attnum>0 AND NOT a.attisdropped AND {condition} ORDER BY 1,2,a.attnum"
            ),
        ),
        (
            "column_properties",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('name',a.attname,'_definition_safe',NOT EXISTS (SELECT 1 FROM pg_depend xd WHERE xd.classid='pg_attrdef'::regclass AND xd.objid=d.oid AND xd.deptype='n' AND NOT (xd.refclassid='pg_class'::regclass AND xd.refobjid=c.oid)),'source_id',a.attnum::text,'identity',{identity},'_default',{default},'_generated',{generated},'comment',col_description(c.oid,a.attnum))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum WHERE c.relkind IN ('r','v','m','f','p') AND a.attnum>0 AND NOT a.attisdropped AND {condition} ORDER BY 1,2,a.attnum"
            ),
        ),
        (
            "constraints",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('_definition_safe',NOT EXISTS (SELECT 1 FROM pg_depend xd WHERE xd.classid='pg_constraint'::regclass AND xd.objid=x.oid AND xd.deptype='n' AND NOT (xd.refclassid='pg_class'::regclass AND xd.refobjid IN (c.oid,x.confrelid))),'source_id',x.oid::text,'name',x.conname,'kind',x.contype::text,'backing_index',ic.relname,'deferrable',x.condeferrable,'initially_deferred',x.condeferred,'validated',x.convalidated,'enforced',true,'_definition',pg_get_constraintdef(x.oid,false),'position',k.ord,'column',a.attname,'referenced_schema',rn.nspname,'referenced_table',rc.relname,'referenced_column',ra.attname,'on_update',x.confupdtype::text,'on_delete',x.confdeltype::text,'match_type',x.confmatchtype::text)::text FROM pg_constraint x JOIN pg_class c ON c.oid=x.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_class ic ON ic.oid=x.conindid LEFT JOIN LATERAL unnest(x.conkey) WITH ORDINALITY k(num,ord) ON true LEFT JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum=k.num LEFT JOIN LATERAL unnest(x.confkey) WITH ORDINALITY rk(num,ord) ON rk.ord=k.ord LEFT JOIN pg_class rc ON rc.oid=x.confrelid LEFT JOIN pg_namespace rn ON rn.oid=rc.relnamespace LEFT JOIN pg_attribute ra ON ra.attrelid=rc.oid AND ra.attnum=rk.num WHERE x.contype IN ('p','u','f','c') AND {condition} ORDER BY 1,2,x.conname,k.ord"
            ),
        ),
        (
            "indexes",
            format!(
                "SELECT n.nspname::text,c.relname::text,json_build_object('_definition_safe',NOT EXISTS (SELECT 1 FROM pg_depend xd WHERE xd.classid='pg_class'::regclass AND xd.objid=i.indexrelid AND xd.deptype='n' AND NOT (xd.refclassid='pg_class'::regclass AND xd.refobjid=c.oid)),'source_id',i.indexrelid::text,'storage',json_build_object('main_bytes',pg_relation_size(ic.oid,'main'),'fsm_bytes',pg_relation_size(ic.oid,'fsm'),'vm_bytes',pg_relation_size(ic.oid,'vm'),'init_bytes',pg_relation_size(ic.oid,'init'),'table_bytes',pg_table_size(ic.oid),'indexes_bytes',pg_indexes_size(ic.oid),'total_bytes',pg_total_relation_size(ic.oid)),'activity',json_build_object('idx_scan',st.idx_scan,'idx_tup_read',st.idx_tup_read,'idx_tup_fetch',st.idx_tup_fetch),'name',ic.relname,'method',am.amname,'unique',i.indisunique,'primary',i.indisprimary,'valid',i.indisvalid,'enabled',i.indisready,'nulls_not_distinct',{nulls},'_definition',pg_get_indexdef(i.indexrelid),'_predicate',pg_get_expr(i.indpred,i.indrelid),'position',k.ord,'included',k.ord>{keys},'column',a.attname,'_expression',pg_get_indexdef(i.indexrelid,k.ord::int,true),'descending',CASE WHEN am.amname='btree' THEN (coalesce(opt.value,0)&1)<>0 END,'nulls_first',CASE WHEN am.amname='btree' THEN (coalesce(opt.value,0)&2)<>0 END,'collation',coll.collname,'operator_class',opc.opcname)::text FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_class ic ON ic.oid=i.indexrelid LEFT JOIN pg_stat_all_indexes st ON st.indexrelid=ic.oid JOIN pg_am am ON am.oid=ic.relam JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(num,ord) ON true LEFT JOIN LATERAL unnest(i.indoption) WITH ORDINALITY opt(value,ord) ON opt.ord=k.ord LEFT JOIN LATERAL unnest(i.indcollation) WITH ORDINALITY co(value,ord) ON co.ord=k.ord LEFT JOIN pg_collation coll ON coll.oid=co.value LEFT JOIN LATERAL unnest(i.indclass) WITH ORDINALITY op(value,ord) ON op.ord=k.ord LEFT JOIN pg_opclass opc ON opc.oid=op.value LEFT JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum=k.num WHERE {condition} ORDER BY 1,2,ic.relname,k.ord"
            ),
        ),
    ]
}
fn mysql_queries(
    checks: bool,
    expressions: bool,
    visible: bool,
    literal_defaults: bool,
    condition: &str,
) -> Vec<(&'static str, String)> {
    let literal_type = if literal_defaults {
        "CASE WHEN c.extra NOT LIKE '%DEFAULT_GENERATED%' THEN c.data_type END"
    } else {
        "NULL"
    };
    let check_join = if checks {
        "LEFT JOIN information_schema.check_constraints cc ON cc.constraint_schema=x.constraint_schema AND cc.constraint_name=x.constraint_name"
    } else {
        ""
    };
    let check_expr = if checks { "cc.check_clause" } else { "NULL" };
    let expression = if expressions { "s.expression" } else { "NULL" };
    let enabled = if visible {
        "s.is_visible='YES'"
    } else {
        "NULL"
    };
    vec![
        (
            "statistics",
            format!(
                "SELECT t.table_schema,t.table_name,CAST(JSON_OBJECT('estimated_rows',t.table_rows) AS CHAR) FROM information_schema.tables t WHERE t.table_schema IN ({condition}) ORDER BY 1,2"
            ),
        ),
        (
            "storage",
            format!(
                "SELECT t.table_schema,t.table_name,CAST(JSON_OBJECT('data_length',t.data_length,'index_length',t.index_length,'data_free',t.data_free) AS CHAR) FROM information_schema.tables t WHERE t.table_schema IN ({condition}) ORDER BY 1,2"
            ),
        ),
        (
            "details",
            format!(
                "SELECT t.table_schema,t.table_name,CAST(JSON_OBJECT('comment',t.table_comment,'_definition',v.view_definition) AS CHAR) FROM information_schema.tables t LEFT JOIN information_schema.views v ON v.table_schema=t.table_schema AND v.table_name=t.table_name WHERE t.table_schema IN ({condition}) ORDER BY 1,2"
            ),
        ),
        (
            "column_properties",
            format!(
                "SELECT c.table_schema,c.table_name,CAST(JSON_OBJECT('name',c.column_name,'_default',c.column_default,'_literal_type',{literal_type},'_generated',nullif(c.generation_expression,''),'identity',CASE WHEN c.extra LIKE '%auto_increment%' THEN 'by_default' ELSE NULL END,'comment',nullif(c.column_comment,'')) AS CHAR) FROM information_schema.columns c WHERE c.table_schema IN ({condition}) ORDER BY 1,2,c.ordinal_position"
            ),
        ),
        (
            "constraints",
            format!(
                "SELECT x.table_schema,x.table_name,CAST(JSON_OBJECT('name',x.constraint_name,'kind',CASE x.constraint_type WHEN 'PRIMARY KEY' THEN 'p' WHEN 'UNIQUE' THEN 'u' WHEN 'FOREIGN KEY' THEN 'f' WHEN 'CHECK' THEN 'c' END,'backing_index',CASE WHEN x.constraint_type IN ('PRIMARY KEY','UNIQUE') THEN x.constraint_name ELSE rc.unique_constraint_name END,'_definition',{check_expr},'position',k.ordinal_position,'column',k.column_name,'referenced_schema',k.referenced_table_schema,'referenced_table',k.referenced_table_name,'referenced_column',k.referenced_column_name,'on_update',rc.update_rule,'on_delete',rc.delete_rule,'match_type',rc.match_option) AS CHAR) FROM information_schema.table_constraints x LEFT JOIN information_schema.key_column_usage k ON k.constraint_schema=x.constraint_schema AND k.table_name=x.table_name AND k.constraint_name=x.constraint_name LEFT JOIN information_schema.referential_constraints rc ON rc.constraint_schema=x.constraint_schema AND rc.table_name=x.table_name AND rc.constraint_name=x.constraint_name {check_join} WHERE x.table_schema IN ({condition}) AND x.constraint_type IN ('PRIMARY KEY','UNIQUE','FOREIGN KEY','CHECK') ORDER BY 1,2,x.constraint_name,k.ordinal_position"
            ),
        ),
        (
            "indexes",
            format!(
                "SELECT s.table_schema,s.table_name,CAST(JSON_OBJECT('name',s.index_name,'method',lower(s.index_type),'unique',s.non_unique=0,'primary',s.index_name='PRIMARY','valid',NULL,'enabled',{enabled},'position',s.seq_in_index,'column',s.column_name,'_expression',{expression},'descending',CASE s.collation WHEN 'A' THEN false WHEN 'D' THEN true ELSE NULL END,'nulls_first',CASE s.collation WHEN 'A' THEN true WHEN 'D' THEN false ELSE NULL END,'prefix_length',s.sub_part) AS CHAR) FROM information_schema.statistics s WHERE s.table_schema IN ({condition}) ORDER BY 1,2,s.index_name,s.seq_in_index"
            ),
        ),
    ]
}
pub(super) fn capture(
    conn: &Connection,
    source: &DataSourceDefinition,
    objects: &mut Objects,
    budget: usize,
) -> Result<()> {
    let scope = source
        .allowed_schemas
        .iter()
        .map(|s| quote_literal(s))
        .collect::<Vec<_>>()
        .join(",");
    let scope = if scope.is_empty() {
        "NULL".into()
    } else {
        scope
    };
    let probe = if source.kind == DataSourceKind::Postgres {
        "SELECT current_setting('server_version_num')::text"
    } else {
        "SELECT VERSION()"
    };
    let probe = format!(
        "SELECT * FROM {}_query({}, {})",
        source.kind.extension(),
        quote_literal(&source.catalog_alias),
        quote_literal(probe)
    );
    let version: String = conn
        .query_row(&probe, [], |r| r.get(0))
        .map_err(engine_error)?;
    let mut mysql_checks = false;
    let queries = if source.kind == DataSourceKind::Postgres {
        pg_queries(
            version
                .parse()
                .map_err(|_| EngineError::new("55000", "invalid PostgreSQL capability version"))?,
            &format!("n.nspname IN ({scope})"),
        )
    } else {
        let capability = "SELECT upper(table_name),upper(column_name) FROM information_schema.columns WHERE table_schema='information_schema' AND upper(table_name) IN ('CHECK_CONSTRAINTS','STATISTICS')";
        let mut stmt = conn
            .prepare(&format!(
                "SELECT * FROM mysql_query({}, {})",
                quote_literal(&source.catalog_alias),
                quote_literal(capability)
            ))
            .map_err(engine_error)?;
        let mut r = stmt.query([]).map_err(engine_error)?;
        let (mut expression, mut visible) = (false, false);
        while let Some(row) = r.next().map_err(engine_error)? {
            let table = metadata_text(row, 0)?;
            let col = metadata_text(row, 1)?;
            mysql_checks |= table == "CHECK_CONSTRAINTS";
            expression |= table == "STATISTICS" && col == "EXPRESSION";
            visible |= table == "STATISTICS" && col == "IS_VISIBLE";
        }
        mysql_queries(
            mysql_checks,
            expression,
            visible,
            version.starts_with("5.7.") || version.starts_with("8."),
            &scope,
        )
    };
    for (category, sql) in queries {
        let result = rows(conn, source, &sql, budget)
            .and_then(|facts| apply_atomic(objects, category, facts, source.kind));
        if let Err(error) = &result
            && error.sqlstate == "54000"
        {
            return Err(error.clone());
        }
        crate::catalog::enforce_budget(&objects.values().collect::<Vec<_>>(), budget)?;
        let mut status = match result {
            Ok(()) => MetadataStatus::success(),
            Err(error) => MetadataStatus::failure(&error),
        };
        if category == "constraints"
            && source.kind == DataSourceKind::Mysql
            && !mysql_checks
            && status.complete
        {
            status.support = MetadataSupport::Partial;
            status.scan_state = MetadataScanState::Partial;
            status.unsupported_fields.push("check_constraints".into());
        }
        if category == "statistics" && source.kind == DataSourceKind::Mysql && status.complete {
            status.support = MetadataSupport::Partial;
            status.unsupported_fields.push("page_counts".into());
        }
        if category == "storage" && source.kind == DataSourceKind::Mysql && status.complete {
            status.support = MetadataSupport::Partial;
            status.scan_state = MetadataScanState::Partial;
            status
                .unsupported_fields
                .push("postgresql_storage_sizes".into());
        }
        for obj in objects.values_mut() {
            let mut status = status.clone();
            let expressions: Vec<&MetadataExpression> = match category {
                "details" => obj.metadata.view_definition.iter().collect(),
                "column_properties" => obj
                    .metadata
                    .columns
                    .values()
                    .flat_map(|c| c.default.iter().chain(c.generated.iter()))
                    .collect(),
                "constraints" => obj
                    .metadata
                    .constraints
                    .iter()
                    .filter_map(|c| c.definition.as_ref())
                    .collect(),
                "indexes" => obj
                    .metadata
                    .indexes
                    .iter()
                    .flat_map(|i| {
                        i.definition
                            .iter()
                            .chain(i.predicate.iter())
                            .chain(i.keys.iter().filter_map(|k| k.expression.as_ref()))
                    })
                    .collect(),
                _ => vec![],
            };
            if status.complete && expressions.iter().any(|e| e.pg_text.is_none()) {
                status.support = MetadataSupport::Partial;
                status
                    .unsupported_fields
                    .push("postgres_expression_translation".into());
            }
            if status.complete
                && category == "details"
                && obj.metadata.view_definition.is_some()
                && obj.metadata.view_dependencies.is_none()
            {
                status.support = MetadataSupport::Partial;
                status
                    .unsupported_fields
                    .push("view_definition_dependencies".into());
            }
            obj.metadata.status.insert(category.into(), status);
        }
    }
    let targets: std::collections::BTreeSet<_> = objects.keys().cloned().collect();
    for obj in objects.values_mut() {
        if source.kind == DataSourceKind::Mysql {
            obj.metadata.status.insert(
                "identity".into(),
                MetadataStatus {
                    support: MetadataSupport::Partial,
                    unsupported_fields: vec!["offline_identity_continuity_unproven".into()],
                    ..MetadataStatus::default()
                },
            );
            obj.metadata.status.insert(
                "column_statistics".into(),
                MetadataStatus {
                    support: MetadataSupport::Unsupported,
                    unsupported_fields: vec!["non_value_column_statistics".into()],
                    ..MetadataStatus::default()
                },
            );
            obj.metadata.status.insert(
                "activity".into(),
                MetadataStatus {
                    support: MetadataSupport::Unsupported,
                    unsupported_fields: vec!["postgresql_activity_counters".into()],
                    ..MetadataStatus::default()
                },
            );
        }
        for constraint in &mut obj.metadata.constraints {
            if let Some(fk) = &mut constraint.foreign_key {
                fk.reference_state = if !source.allowed_schemas.contains(&fk.schema) {
                    MetadataReferenceState::OutsideScope
                } else if targets.contains(&(fk.schema.clone(), fk.table.clone())) {
                    MetadataReferenceState::Resolved
                } else {
                    MetadataReferenceState::Unresolved
                };
            }
        }
        let details = obj.metadata.status["details"].clone();
        obj.metadata
            .status
            .insert("view_definitions".into(), details.clone());
        let columns = &obj.metadata.status["column_properties"];
        let mut comments = if !columns.complete {
            columns.clone()
        } else {
            details
        };
        if comments.complete {
            comments.support = MetadataSupport::Supported;
            comments.unsupported_fields.clear();
        }
        obj.metadata.status.insert("comments".into(), comments);
        obj.metadata
            .status
            .insert("core".into(), MetadataStatus::success());
        obj.indexes = obj
            .metadata
            .indexes
            .iter()
            .map(|i| CatalogIndex {
                name: i.name.clone(),
                definition: i
                    .definition
                    .as_ref()
                    .map(|e| e.source_text.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "{}:{}",
                            i.method.as_deref().unwrap_or("unknown"),
                            i.keys
                                .iter()
                                .filter_map(|k| k.column.as_deref())
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    }),
                protected: i.primary == Some(true)
                    || obj
                        .metadata
                        .constraints
                        .iter()
                        .any(|c| c.backing_index.as_ref() == Some(&i.name)),
                supported: i.method.as_deref() == Some("btree")
                    && i.include.is_empty()
                    && i.predicate.is_none()
                    && i.keys
                        .iter()
                        .all(|k| k.column.is_some() && k.prefix_length.is_none()),
            })
            .collect();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn failed_category_rolls_back_partial_changes_without_losing_other_facts() {
        let object = CatalogObject {
            schema: "public".into(),
            name: "a".into(),
            object_type: "BASE TABLE".into(),
            columns: vec![],
            indexes: vec![],
            metadata: ObjectMetadata {
                comment: Some("retain".into()),
                storage: RelationStorage {
                    main_bytes: Some(100),
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let mut other = object.clone();
        other.name = "b".into();
        let mut objects: Objects = [
            (("public".into(), "a".into()), object),
            (("public".into(), "b".into()), other),
        ]
        .into();
        let before = objects.clone();
        let facts = vec![
            ("public".into(), "a".into(), json!({"main_bytes":200})),
            ("public".into(), "b".into(), json!({"main_bytes":-1})),
        ];
        assert!(apply_atomic(&mut objects, "storage", facts, DataSourceKind::Postgres).is_err());
        assert_eq!(objects, before);
        apply_atomic(
            &mut objects,
            "storage",
            vec![
                ("public".into(), "a".into(), json!({})),
                ("public".into(), "b".into(), json!({})),
            ],
            DataSourceKind::Postgres,
        )
        .unwrap();
        assert_eq!(
            objects[&("public".into(), "a".into())]
                .metadata
                .storage
                .main_bytes,
            None
        );
        assert_eq!(
            objects[&("public".into(), "a".into())]
                .metadata
                .comment
                .as_deref(),
            Some("retain")
        );
    }
    #[test]
    fn storage_and_activity_require_complete_unique_typed_enumeration() {
        let objects = [(
            ("public".into(), "t".into()),
            CatalogObject {
                schema: "public".into(),
                name: "t".into(),
                object_type: "BASE TABLE".into(),
                columns: vec![],
                indexes: vec![],
                metadata: ObjectMetadata::default(),
            },
        )]
        .into_iter()
        .collect::<Objects>();
        for category in ["storage", "activity"] {
            assert!(
                apply(
                    &mut objects.clone(),
                    category,
                    vec![],
                    DataSourceKind::Postgres
                )
                .is_err()
            );
            let fact = ("public".into(), "t".into(), json!({}));
            assert!(
                apply(
                    &mut objects.clone(),
                    category,
                    vec![fact.clone(), fact],
                    DataSourceKind::Postgres
                )
                .is_err()
            );
        }
        for (category, fact) in [
            ("storage", json!({"main_bytes":"invalid"})),
            ("activity", json!({"seq_scan":9223372036854775808u64})),
        ] {
            assert!(
                apply(
                    &mut objects.clone(),
                    category,
                    vec![("public".into(), "t".into(), fact)],
                    DataSourceKind::Postgres
                )
                .is_err()
            );
        }
        // An uncollected fact is unknown, never a measured zero.
        let unknown = ObjectMetadata::default();
        assert_eq!(unknown.storage, RelationStorage::default());
        assert_eq!(unknown.activity, RelationActivity::default());
    }

    #[test]
    fn mysql_literal_defaults_preserve_empty_values_and_reject_expressions() {
        for (typ, value, pg) in [
            ("varchar", "", "''"),
            ("varchar", "O'Reilly\\path", "'O''Reilly\\path'"),
            ("varchar", "CURRENT_TIMESTAMP", "'CURRENT_TIMESTAMP'"),
            ("decimal", "-123.4500", "-123.4500"),
            ("int", "0", "0"),
        ] {
            let e = mysql_default(&json!({"_literal_type":typ,"_default":value})).unwrap();
            assert_eq!(e.source_text, value);
            assert_eq!(e.pg_text.as_deref(), Some(pg));
        }
        for (typ, value) in [
            ("int", "1;DROP TABLE x"),
            ("int", "."),
            ("decimal", "1.2.3"),
            ("timestamp", "CURRENT_TIMESTAMP"),
            ("", "(uuid())"),
        ] {
            assert!(
                mysql_default(&json!({"_literal_type":typ,"_default":value}))
                    .unwrap()
                    .pg_text
                    .is_none()
            );
        }
        assert!(mysql_default(&json!({"_literal_type":"varchar","_default":null})).is_none());
    }
}
