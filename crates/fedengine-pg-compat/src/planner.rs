//! AST-only routing and scoped local catalog plans. User SQL never names physical storage.
use crate::{SERVER_VERSION, catalog_schema, effective_path, parse_pg, physical_catalog, setting};
use fedengine_contract::*;
use fedengine_core::*;
use sqlparser::{ast::*, dialect::DuckDbDialect, parser::Parser};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
};

macro_rules! attempt {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(error) => return ControlFlow::Break(error),
        }
    };
}

pub struct MetadataPlan {
    pub sql: String,
    pub columns: Vec<Option<ColumnMeta>>,
    pub command: String,
    /// PG signature/cast provenance, retained before lowering catalog types to DuckDB.
    pub parameter_types: BTreeMap<usize, PgType>,
}
pub enum CompatibilityPlan {
    Business(String),
    Metadata(MetadataPlan),
}
fn ident(i: &Ident) -> String {
    if i.quote_style.is_some() {
        i.value.clone()
    } else {
        i.value.to_lowercase()
    }
}
fn parts(n: &ObjectName) -> Result<Vec<String>> {
    n.0.iter()
        .map(|p| {
            if let ObjectNamePart::Identifier(i) = p {
                Ok(ident(i))
            } else {
                Err(EngineError::unsupported(
                    "computed catalog names are unsupported",
                ))
            }
        })
        .collect()
}
fn relation(n: &ObjectName) -> Result<Option<String>> {
    let p = parts(n)?;
    if p.iter()
        .any(|s| s.eq_ignore_ascii_case("_fedengine") || s.starts_with("__fed_"))
    {
        return Err(EngineError::denied(
            "internal catalog storage is not accessible",
        ));
    }
    let name = match p.as_slice() {
        [name] if name.starts_with("pg_") => name.clone(),
        [schema, name] if schema == "pg_catalog" => name.clone(),
        [schema, name] if schema == "information_schema" => {
            format!("{schema}.{name}")
        }
        _ => return Ok(None),
    };
    if !catalog_schema().contains_key(&name) {
        return Err(EngineError::unsupported(
            "system relation is outside the compatibility matrix",
        ));
    }
    Ok(Some(name))
}
fn function_name(f: &Function) -> Result<String> {
    let p = parts(&f.name)?;
    match p.as_slice() {
        [name] => Ok(name.clone()),
        [schema, name] if schema == "pg_catalog" => Ok(name.clone()),
        [schema, name] if schema == "information_schema" && name == "_pg_expandarray" => {
            Ok(name.clone())
        }
        _ => Err(EngineError::denied(
            "function namespace is not allowed in metadata queries",
        )),
    }
}
fn args(f: &Function) -> Result<Vec<&Expr>> {
    match &f.args {
        FunctionArguments::None => Ok(vec![]),
        FunctionArguments::List(l) if l.clauses.is_empty() => l
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(EngineError::unsupported(
                    "metadata function argument form is unsupported",
                )),
            })
            .collect(),
        _ => Err(EngineError::unsupported(
            "metadata function argument form is unsupported",
        )),
    }
}
fn string(e: &Expr) -> Option<&str> {
    if let Expr::Value(v) = e
        && let Value::SingleQuotedString(s) = &v.value
    {
        return Some(s);
    }
    None
}
fn parse_expr(sql: &str) -> Result<Expr> {
    let mut parsed = Parser::parse_sql(&DuckDbDialect {}, &format!("SELECT {sql}"))
        .map_err(|_| EngineError::new("XX000", "generated metadata expression is invalid"))?;
    let Statement::Query(q) = parsed.remove(0) else {
        unreachable!()
    };
    let SetExpr::Select(mut s) = *q.body else {
        unreachable!()
    };
    let SelectItem::UnnamedExpr(e) = s.projection.remove(0) else {
        unreachable!()
    };
    Ok(e)
}
fn permission(
    alias: &str,
    grants: &[fedengine_core::Grant],
    schema_field: &str,
    object_field: &str,
) -> String {
    let cases = grants
        .iter()
        .map(|g| {
            format!(
                "({} AND {})",
                if g.schema == "*" {
                    "true".into()
                } else {
                    format!("{alias}.{schema_field}={}", quote_literal(&g.schema))
                },
                if g.object == "*" {
                    "true".into()
                } else {
                    format!("{alias}.{object_field}={}", quote_literal(&g.object))
                }
            )
        })
        .collect::<Vec<_>>();
    if cases.is_empty() {
        "false".into()
    } else {
        format!("({})", cases.join(" OR "))
    }
}
fn where_scope(name: &str, scope: &MetadataScope) -> Result<String> {
    let mut cases = vec!["p._snapshot_id=''".to_string()];
    let databases = if name == "pg_database" {
        scope.databases.iter().collect::<Vec<_>>()
    } else {
        scope.virtual_database.iter().collect()
    };
    for db in databases {
        cases.push(format!(
            "p._snapshot_id={}",
            quote_literal(&format!("virtual:{}:public", db.oid))
        ));
    }
    for c in &scope.catalogs {
        if name == "pg_database" && c.mapping.is_some() {
            continue;
        }
        if name != "pg_database" && c.catalog != scope.database {
            continue;
        }
        let visible = if name == "pg_database" {
            "true".into()
        } else if matches!(
            name,
            "pg_namespace"
                | "information_schema.schemata"
                | "pg_foreign_server"
                | "pg_foreign_data_wrapper"
        ) {
            let schema_grants:Vec<_>=c.grants.iter().map(|g|{
                let schema=if g.schema=="*"{"true".into()}else{format!("p._schema={}",quote_literal(&g.schema))};
                let object=if g.object=="*"{"true".into()}else{format!("EXISTS(SELECT 1 FROM {} q WHERE q._snapshot_id=p._snapshot_id AND q._schema=p._schema AND q._object={} AND q.relkind IN ('r','v','m','f','p'))",physical_catalog("pg_class")?,quote_literal(&g.object))};
                Ok(format!("({schema} AND {object})"))
            }).collect::<Result<_>>()?;
            format!("({})", schema_grants.join(" OR "))
        } else {
            permission("p", &c.grants, "_schema", "_object")
        };
        let target = permission("p", &c.grants, "_target_schema", "_target_object");
        cases.push(format!(
            "(p._snapshot_id={} AND {visible} AND (p._target_schema IS NULL OR ({target})))",
            quote_literal(&c.snapshot_id)
        ));
    }
    Ok(format!("({})", cases.join(" OR ")))
}
pub fn scoped_catalog(name: &str, scope: &MetadataScope) -> Result<String> {
    let fields = &catalog_schema()[name];
    let columns=fields.iter().map(|f|{
        if name=="pg_database" && f.attname=="datdba" {
            let owned=scope.databases.iter().filter(|d|d.config.owner==scope.principal).map(|d|quote_literal(&d.config.name)).collect::<Vec<_>>();
            return if owned.is_empty() { "p.datdba".into() } else { format!("CASE WHEN p.datname IN ({}) THEN 10::UINTEGER ELSE p.datdba END AS datdba",owned.join(",")) };
        }
        if matches!(name,"pg_views"|"pg_matviews"|"information_schema.views") && matches!(f.attname.as_str(),"definition"|"view_definition") {
            format!("(SELECT d.definition FROM ({}) d WHERE d.snapshot_id=p._snapshot_id AND d.schema_name=p._schema AND d.object_name=p._object AND d.kind='view' LIMIT 1) AS {}",scoped_definitions(scope),quote_identifier(&f.attname))
        }else{format!("p.{}",quote_identifier(&f.attname))}
    }).collect::<Vec<_>>().join(",");
    let mut sql = format!(
        "SELECT DISTINCT {columns} FROM {} p WHERE {}",
        physical_catalog(name)?,
        where_scope(name, scope)?
    );
    if name == "pg_roles" {
        let user = fields
            .iter()
            .map(|f| {
                format!(
                    "CAST({} AS {})",
                    match f.attname.as_str() {
                        "oid" => "10".into(),
                        "rolname" => quote_literal(&scope.principal),
                        "rolcanlogin" => "true".into(),
                        "rolsuper" | "rolinherit" | "rolcreaterole" | "rolcreatedb"
                        | "rolreplication" | "rolbypassrls" => "false".into(),
                        _ => "NULL".into(),
                    },
                    f.duckdb_type()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" UNION ALL SELECT {user}"));
    }
    if name == "pg_settings" {
        let values = fields
            .iter()
            .map(|f| {
                format!(
                    "CAST({} AS {})",
                    match f.attname.as_str() {
                        "name" => "'max_index_keys'",
                        "setting" => "'32'",
                        "vartype" => "'integer'",
                        "context" => "'internal'",
                        _ => "NULL",
                    },
                    f.duckdb_type()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" UNION ALL SELECT {values}"));
    }
    Ok(sql)
}
fn scoped_definitions(scope: &MetadataScope) -> String {
    let cases=scope.catalogs.iter().filter(|c|c.catalog==scope.database).map(|c|{
        let schemas=c.schemas.iter().map(|s|quote_literal(s)).collect::<Vec<_>>().join(",");
        let dependencies=format!("(d.kind<>'view' OR (d.dependencies_known AND NOT EXISTS(SELECT 1 FROM _fedengine.metadata_view_dependencies v WHERE v.snapshot_id=d.snapshot_id AND v.oid=d.oid AND NOT (v.schema_name IN ({}) AND {}))))",if schemas.is_empty(){"NULL"}else{&schemas},permission("v",&c.grants,"schema_name","object_name"));
        format!("(d.snapshot_id={} AND {} AND (d.target_schema IS NULL OR {}) AND {dependencies})",quote_literal(&c.snapshot_id),permission("d",&c.grants,"schema_name","object_name"),permission("d",&c.grants,"target_schema","target_object"))
    }).collect::<Vec<_>>();
    format!(
        "SELECT * FROM _fedengine.metadata_definitions d WHERE {}",
        if cases.is_empty() {
            "false".into()
        } else {
            cases.join(" OR ")
        }
    )
}
fn scoped_relation_statistics(scope: &MetadataScope) -> String {
    let cases = scope
        .catalogs
        .iter()
        .filter(|c| c.catalog == scope.database)
        .map(|c| {
            format!(
                "(s.snapshot_id={} AND {})",
                quote_literal(&c.snapshot_id),
                permission("s", &c.grants, "schema_name", "object_name")
            )
        })
        .collect::<Vec<_>>();
    format!(
        "SELECT DISTINCT s.* FROM _fedengine.metadata_relation_statistics s WHERE {}",
        if cases.is_empty() {
            "false".into()
        } else {
            cases.join(" OR ")
        }
    )
}
fn session_function(
    name: &str,
    a: &[&Expr],
    session: &SessionContext,
    scope: &MetadataScope,
    pid: i32,
) -> Result<Option<String>> {
    let paths = effective_path(session, scope);
    Ok(Some(match (name, a.len()) {
        ("version", 0) => quote_literal(SERVER_VERSION),
        ("current_database" | "current_catalog", 0) => quote_literal(&session.database),
        ("current_schema", 0) => paths
            .first()
            .map(|s| quote_literal(s))
            .unwrap_or("NULL::VARCHAR".into()),
        ("current_user" | "session_user" | "user", 0) => quote_literal(&scope.principal),
        ("pg_backend_pid", 0) => pid.to_string(),
        ("current_schemas", 1) => {
            let mut paths = paths;
            if matches!(a[0],Expr::Value(v) if v.value==Value::Boolean(true))
                && !paths.iter().any(|p| p == "pg_catalog")
            {
                paths.insert(0, "pg_catalog".into());
            }
            format!(
                "CAST([{}] AS VARCHAR[])",
                paths
                    .iter()
                    .map(|s| quote_literal(s))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        ("current_setting", 1 | 2) => {
            let name = string(a[0]).ok_or_else(|| {
                EngineError::unsupported("current_setting requires a literal parameter name")
            })?;
            match setting(session, scope, name) {
                Ok(v) => quote_literal(&v),
                Err(e)
                    if a.len() == 2
                        && matches!(a[1],Expr::Value(v) if v.value==Value::Boolean(true)) =>
                {
                    let _ = e;
                    "NULL::VARCHAR".into()
                }
                Err(e) => return Err(e),
            }
        }
        _ => return Ok(None),
    }))
}
struct Classify {
    scopes: Vec<BTreeSet<String>>,
    system: bool,
    business: bool,
    depth: usize,
    builtin: Option<ObjectName>,
}
impl Visitor for Classify {
    type Break = EngineError;
    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        if self.depth > 32 {
            return ControlFlow::Break(EngineError::new("54000", "query nesting exceeds 32"));
        }
        let names = q
            .with
            .as_ref()
            .map(|w| w.cte_tables.iter().map(|c| ident(&c.alias.name)).collect())
            .unwrap_or_default();
        self.scopes.push(names);
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        self.scopes.pop();
        ControlFlow::Continue(())
    }
    fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<Self::Break> {
        match t {
            TableFactor::Table {
                name,
                args: Some(_),
                ..
            }
            | TableFactor::Function { name, .. } => {
                let p = attempt!(parts(name));
                if matches!(p.as_slice(),[n] if n=="pg_get_keywords")
                    || matches!(p.as_slice(),[s,n] if s=="pg_catalog"&&n=="pg_get_keywords")
                {
                    self.system = true;
                }
                self.builtin = Some(name.clone());
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, n: &ObjectName) -> ControlFlow<Self::Break> {
        if self.builtin.as_ref() == Some(n) {
            self.builtin = None;
            return ControlFlow::Continue(());
        }
        if let Ok(p) = parts(n)
            && p.len() == 1
            && self.scopes.iter().rev().any(|s| s.contains(&p[0]))
        {
            return ControlFlow::Continue(());
        }
        match relation(n) {
            Ok(Some(_)) => self.system = true,
            Ok(None) => self.business = true,
            Err(e) => return ControlFlow::Break(e),
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<Self::Break> {
        if let Expr::Cast { data_type, .. } = e
            && matches!(
                data_type.to_string().to_ascii_lowercase().as_str(),
                "regclass"
                    | "pg_catalog.regclass"
                    | "regproc"
                    | "pg_catalog.regproc"
                    | "regtype"
                    | "pg_catalog.regtype"
            )
        {
            self.system = true;
        }
        if let Expr::Function(f) = e
            && let Ok(name) = function_name(f)
            && (name.starts_with("pg_")
                || matches!(
                    name.as_str(),
                    "version"
                        | "current_database"
                        | "current_catalog"
                        | "current_schema"
                        | "current_schemas"
                        | "current_setting"
                        | "current_user"
                        | "session_user"
                        | "user"
                        | "format_type"
                        | "to_regclass"
                        | "obj_description"
                        | "col_description"
                        | "has_database_privilege"
                        | "has_schema_privilege"
                        | "has_table_privilege"
                ))
        {
            self.system = true;
        }
        ControlFlow::Continue(())
    }
}
pub fn plan_compatibility(
    sql: &str,
    session: &SessionContext,
    scope: &MetadataScope,
    pid: i32,
) -> Result<CompatibilityPlan> {
    plan_with_parameters(sql, session, scope, pid, &BTreeMap::new())
}
/// Plan against the original client parameter declarations, before SQL lowering.
pub fn plan_with_parameters(
    sql: &str,
    session: &SessionContext,
    scope: &MetadataScope,
    pid: i32,
    declared: &BTreeMap<usize, PgType>,
) -> Result<CompatibilityPlan> {
    let mut parsed = parse_pg(sql)?;
    if parsed.len() != 1 {
        return Err(EngineError::unsupported("one statement is required"));
    }
    let mut statement = parsed.remove(0);
    let types = visit_expressions(&statement, |expr| {
        if let Expr::Cast {
            data_type: DataType::JSONB,
            ..
        } = expr
        {
            return ControlFlow::Break(EngineError::unsupported(
                "JSONB encoding is not implemented",
            ));
        }
        ControlFlow::Continue(())
    });
    if let ControlFlow::Break(error) = types {
        return Err(error);
    }
    let normalized = visit_relations_mut(&mut statement, |n| {
        let p = attempt!(parts(n));
        if let [database, schema, _] = p.as_slice()
            && matches!(schema.as_str(), "pg_catalog" | "information_schema")
        {
            if database != &session.database {
                return ControlFlow::Break(EngineError::unsupported(
                    "cross-database references are not implemented",
                ));
            }
            n.0.remove(0);
        }
        ControlFlow::<EngineError>::Continue(())
    });
    if let ControlFlow::Break(e) = normalized {
        return Err(e);
    }
    if let Statement::ShowVariable { variable } = &statement {
        let name = variable.iter().map(ident).collect::<Vec<_>>().join(".");
        return Ok(CompatibilityPlan::Metadata(MetadataPlan {
            sql: format!(
                "SELECT {} AS {}",
                quote_literal(&setting(session, scope, &name)?),
                quote_identifier(&name)
            ),
            columns: vec![Some(ColumnMeta::new(name, PgType::Text))],
            command: "SHOW".into(),
            parameter_types: BTreeMap::new(),
        }));
    }
    let mut classify = Classify {
        scopes: vec![],
        system: false,
        business: false,
        depth: 0,
        builtin: None,
    };
    if let ControlFlow::Break(e) = Visit::visit(&statement, &mut classify) {
        return Err(e);
    }
    if declared.values().any(|t| matches!(t, PgType::Catalog(_))) {
        classify.system = true;
    }
    // Business functions use the same session values, while the existing Analyzer retains policy control.
    if !classify.system || classify.business {
        let mut referenced_system = false;
        let flow = visit_relations(&statement, |n| {
            if attempt!(relation(n)).is_some() {
                referenced_system = true;
            }
            ControlFlow::<EngineError>::Continue(())
        });
        if let ControlFlow::Break(e) = flow {
            return Err(e);
        }
        if referenced_system {
            return Err(EngineError::unsupported(
                "mixed system catalog and business relations are unsupported",
            ));
        }
        let flow = visit_expressions_mut(&mut statement, |e| {
            if let Expr::Function(f) = e
                && let Ok(name) = function_name(f)
                && matches!(
                    name.as_str(),
                    "version"
                        | "current_database"
                        | "current_catalog"
                        | "current_schema"
                        | "current_schemas"
                        | "current_setting"
                        | "pg_backend_pid"
                        | "current_user"
                        | "session_user"
                        | "user"
                )
            {
                let replacement = attempt!(session_function(
                    &name,
                    &attempt!(args(f)),
                    session,
                    scope,
                    pid
                ));
                if let Some(sql) = replacement {
                    *e = attempt!(parse_expr(&sql));
                }
            }
            ControlFlow::<EngineError>::Continue(())
        });
        if let ControlFlow::Break(e) = flow {
            return Err(e);
        }
        return Ok(CompatibilityPlan::Business(statement.to_string()));
    }
    if !matches!(statement, Statement::Query(_)) {
        return Err(EngineError::unsupported("system catalogs are read-only"));
    }
    let parameter_types = metadata_parameter_types(&statement, declared)?;
    let mut provenance = statement.clone();
    crate::result_types::preserve_names(&mut provenance);
    let _ = visit_expressions_mut(&mut provenance, |e| {
        if let Expr::Value(v) = e
            && let Value::Placeholder(p) = &v.value
            && let Some(typ) = p
                .strip_prefix('$')
                .and_then(|p| p.parse::<usize>().ok())
                .and_then(|n| parameter_types.get(&n))
            && let Some(name) = crate::builtin_types()
                .iter()
                .find(|t| t["oid"].as_u64() == Some(typ.oid().into()))
                .and_then(|t| t["typname"].as_str())
        {
            *e = parse_expr(&format!("CAST({p} AS {name})")).expect("trusted builtin type cast");
        }
        ControlFlow::<()>::Continue(())
    });
    let columns = crate::result_types::result_columns(&provenance)?;
    // Unknown physical types still need original PG labels in native DESCRIBE.
    crate::result_types::preserve_names(&mut statement);
    let mut rewrite = Rewrite {
        session,
        scope,
        pid,
        parameter_types: &parameter_types,
        regclass_text_casts: vec![],
        ctes: vec![],
        bindings: vec![],
        depth: 0,
    };
    if let ControlFlow::Break(e) = VisitMut::visit(&mut statement, &mut rewrite) {
        return Err(e);
    }
    Ok(CompatibilityPlan::Metadata(MetadataPlan {
        sql: statement.to_string(),
        columns,
        command: "SELECT".into(),
        parameter_types,
    }))
}
fn metadata_parameter_types(
    statement: &Statement,
    declared: &BTreeMap<usize, PgType>,
) -> Result<BTreeMap<usize, PgType>> {
    fn parameter(expr: &Expr) -> Option<usize> {
        if let Expr::Value(v) = expr
            && let Value::Placeholder(p) = &v.value
        {
            p.strip_prefix('$')?.parse().ok()
        } else {
            None
        }
    }
    let mut types = declared.clone();
    // Explicit casts take precedence over a function's implicit argument type.
    let _ = visit_expressions(statement, |expr| {
        if let Expr::Cast {
            expr, data_type, ..
        } = expr
            && let Some(n) = parameter(expr)
            && let Some(typ) = crate::result_types::type_from_name(&data_type.to_string())
        {
            types.entry(n).or_insert(typ);
        }
        ControlFlow::<()>::Continue(())
    });
    let explicit = types.clone();
    let flow = visit_expressions(statement, |expr| {
        if let Expr::Function(f) = expr
            && let Ok(name) = function_name(f)
        {
            let Ok(a) = args(f) else {
                return ControlFlow::Continue(());
            };
            // Signatures belong to the original PG AST, including arguments
            // erased by typed-NULL and constant compatibility rewrites.
            let signature: &[PgType] = match name.as_str() {
                "pg_relation_size" => &[PgType::Catalog(2205), PgType::Text],
                "pg_table_size" | "pg_indexes_size" | "pg_total_relation_size" => {
                    &[PgType::Catalog(2205)]
                }
                "pg_database_size" => &[PgType::Catalog(19)],
                "pg_get_userbyid"
                | "pg_stat_get_numscans"
                | "pg_get_partkeydef"
                | "pg_tablespace_location"
                | "pg_table_is_visible" => &[PgType::Catalog(26)],
                "pg_get_ruledef" | "pg_get_constraintdef" => &[PgType::Catalog(26), PgType::Bool],
                "pg_get_viewdef" => {
                    let known = |i: usize| {
                        a.get(i).and_then(|e| {
                            parameter(e)
                                .and_then(|p| explicit.get(&p).copied())
                                .or_else(|| {
                                    crate::result_types::infer(e, &vec![]).map(|c| c.data_type)
                                })
                        })
                    };
                    if a.len() == 2 && known(1).is_none() && known(0) != Some(PgType::Text) {
                        return ControlFlow::Break(EngineError::new(
                            "42725",
                            "function pg_get_viewdef is not unique; add explicit casts",
                        ));
                    }
                    if known(1) == Some(PgType::Int4) {
                        &[PgType::Catalog(26), PgType::Int4]
                    } else if known(0).is_some_and(|t| {
                        matches!(t, PgType::Catalog(26) | PgType::Int4 | PgType::Int8)
                    }) {
                        &[PgType::Catalog(26), PgType::Bool]
                    } else {
                        &[PgType::Text, PgType::Bool]
                    }
                }
                "pg_get_indexdef" => &[PgType::Catalog(26), PgType::Int4, PgType::Bool],
                "pg_get_expr" => &[PgType::Catalog(194), PgType::Catalog(26), PgType::Bool],
                "format_type" => &[PgType::Catalog(26), PgType::Int4],
                "obj_description" => &[PgType::Catalog(26), PgType::Catalog(19)],
                "col_description" => &[PgType::Catalog(26), PgType::Int4],
                "pg_encoding_to_char" => &[PgType::Int4],
                "current_schemas" => &[PgType::Bool],
                "current_setting" => &[PgType::Text, PgType::Bool],
                "to_regclass" | "lower" | "upper" | "length" | "char_length" => &[PgType::Text],
                "trim" | "ltrim" | "rtrim" => &[PgType::Text, PgType::Text],
                "replace" => &[PgType::Text, PgType::Text, PgType::Text],
                "substring" | "substr" => &[PgType::Text, PgType::Int4, PgType::Int4],
                "has_database_privilege" | "has_schema_privilege" | "has_table_privilege" => {
                    &[PgType::Text, PgType::Text]
                }
                _ => &[],
            };
            for (arg, typ) in a.iter().zip(signature) {
                if let Some(n) = parameter(arg) {
                    if let Some(previous) = types.get(&n)
                        && previous != typ
                        && !explicit.contains_key(&n)
                    {
                        return ControlFlow::Break(EngineError::new(
                            "42P08",
                            "inconsistent types deduced for parameter",
                        ));
                    }
                    types.entry(n).or_insert(*typ);
                }
            }
        }
        ControlFlow::<EngineError>::Continue(())
    });
    if let ControlFlow::Break(error) = flow {
        return Err(error);
    }
    Ok(types)
}
struct Rewrite<'a> {
    session: &'a SessionContext,
    scope: &'a MetadataScope,
    pid: i32,
    parameter_types: &'a BTreeMap<usize, PgType>,
    regclass_text_casts: Vec<bool>,
    ctes: Vec<BTreeSet<String>>,
    bindings: Vec<crate::result_types::Bindings>,
    depth: usize,
}
impl VisitorMut for Rewrite<'_> {
    type Break = EngineError;
    fn pre_visit_statement(&mut self, s: &mut Statement) -> ControlFlow<Self::Break> {
        if !matches!(s, Statement::Query(_)) {
            return ControlFlow::Break(EngineError::unsupported(
                "metadata statements are read-only",
            ));
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        if self.depth > 32 || !q.locks.is_empty() || q.with.as_ref().is_some_and(|w| w.recursive) {
            return ControlFlow::Break(EngineError::unsupported(
                "recursive, locking or deeply nested metadata query is unsupported",
            ));
        }
        self.bindings
            .push(attempt!(crate::result_types::bindings(q)));
        self.ctes.push(
            q.with
                .as_ref()
                .map(|w| w.cte_tables.iter().map(|c| ident(&c.alias.name)).collect())
                .unwrap_or_default(),
        );
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        self.ctes.pop();
        self.bindings.pop();
        ControlFlow::Continue(())
    }
    fn pre_visit_table_factor(&mut self, t: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table {
            name,
            args: Some(a),
            alias,
            ..
        } = t
        {
            let p = attempt!(parts(name));
            if matches!(p.as_slice(),[n] if n=="pg_get_keywords")
                || matches!(p.as_slice(),[s,n] if s=="pg_catalog"&&n=="pg_get_keywords")
            {
                if !a.args.is_empty() {
                    return ControlFlow::Break(EngineError::unsupported(
                        "pg_get_keywords takes no arguments",
                    ));
                }
                let words: Vec<serde_json::Value> =
                    serde_json::from_str(include_str!("../catalog/pg16-keywords.json"))
                        .expect("checked-in PG16 keywords");
                let values = words
                    .iter()
                    .map(|w| {
                        format!(
                            "({},{},{},{},{})",
                            quote_literal(w["word"].as_str().unwrap()),
                            quote_literal(w["catcode"].as_str().unwrap()),
                            w["barelabel"],
                            quote_literal(w["catdesc"].as_str().unwrap()),
                            quote_literal(w["baredesc"].as_str().unwrap())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let mut statements=attempt!(Parser::parse_sql(&DuckDbDialect{},&format!("SELECT * FROM (VALUES {values}) AS pg_get_keywords(word,catcode,barelabel,catdesc,baredesc)")).map_err(|_|EngineError::new("XX000","invalid keyword fixture")));
                let Statement::Query(q) = statements.remove(0) else {
                    unreachable!()
                };
                let SetExpr::Select(mut select) = *q.body else {
                    unreachable!()
                };
                let mut replacement = select.from.remove(0).relation;
                if let Some(original) = alias.clone()
                    && let TableFactor::Derived { alias, .. } = &mut replacement
                {
                    *alias = Some(original);
                }
                *t = replacement;
                return ControlFlow::Continue(());
            }
        }
        let (name, a) = match t {
            TableFactor::Table {
                name,
                args: Some(a),
                ..
            } => (name, &mut a.args),
            TableFactor::Function { name, args, .. } => (name, args),
            _ => return ControlFlow::Continue(()),
        };
        if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Function(f)))) = a.get_mut(1)
            && attempt!(function_name(f)) == "array_upper"
        {
            let fa = attempt!(args(f));
            if fa.len() == 2
                && matches!(fa[1],Expr::Value(v) if v.value==Value::Number("1".into(),false))
                && let Expr::Function(path) = fa[0]
                && attempt!(function_name(path)) == "current_schemas"
                && let [Expr::Value(v)] = attempt!(args(path)).as_slice()
                && let Value::Boolean(implicit) = v.value
            {
                let n = effective_path(self.session, self.scope).len() + usize::from(implicit);
                a[1] = FunctionArg::Unnamed(FunctionArgExpr::Expr(attempt!(parse_expr(
                    &n.to_string()
                ))));
            }
        }
        attempt!(generator(name, a));
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, s: &mut Select) -> ControlFlow<Self::Break> {
        if s.into.is_some() {
            return ControlFlow::Break(EngineError::unsupported("SELECT INTO is disabled"));
        }
        ControlFlow::Continue(())
    }
    fn post_visit_table_factor(&mut self, t: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } = t
        {
            let p = attempt!(parts(name));
            if p.len() == 1 && self.ctes.iter().any(|s| s.contains(&p[0])) {
                return ControlFlow::Continue(());
            }
            let Some(key) = attempt!(relation(name)) else {
                return ControlFlow::Break(EngineError::unsupported(
                    "mixed catalog and business relation",
                ));
            };
            let alias = alias.clone().unwrap_or(TableAlias {
                name: Ident::with_quote('"', p.last().unwrap()),
                columns: vec![],
                explicit: true,
                at: None,
            });
            let select = attempt!(scoped_catalog(&key, self.scope));
            let mut parsed = attempt!(
                Parser::parse_sql(
                    &DuckDbDialect {},
                    &format!("SELECT * FROM ({select}) AS __fed_catalog")
                )
                .map_err(|_| EngineError::new("XX000", "invalid scoped catalog projection"))
            );
            let Statement::Query(q) = parsed.remove(0) else {
                unreachable!()
            };
            let SetExpr::Select(mut s) = *q.body else {
                unreachable!()
            };
            let TableFactor::Derived {
                alias: generated, ..
            } = &mut s.from[0].relation
            else {
                unreachable!()
            };
            *generated = Some(alias);
            *t = s.from.remove(0).relation;
        } else if let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = t
        {
            attempt!(generator(name, &args.args));
            let p = attempt!(parts(name));
            let n = p.last().map(String::as_str).unwrap_or("");
            if !matches!(n, "generate_series" | "unnest") {
                return ControlFlow::Break(EngineError::unsupported(
                    "metadata table function is unsupported",
                ));
            }
            *name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(n))]);
        } else if let TableFactor::Function { name, args, .. } = t {
            attempt!(generator(name, args));
            let n = parts(name).unwrap().pop().unwrap();
            *name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(n))]);
        } else if !matches!(
            t,
            TableFactor::Derived { .. } | TableFactor::NestedJoin { .. }
        ) {
            return ControlFlow::Break(EngineError::unsupported(
                "metadata table factor is unsupported",
            ));
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<Self::Break> {
        let text_cast = if let Expr::Cast {
            expr, data_type, ..
        } = e
        {
            let ty = crate::result_types::type_from_name(&data_type.to_string());
            let source = crate::result_types::infer(expr, self.bindings.last().unwrap_or(&vec![]))
                .map(|c| c.data_type)
                .or_else(|| {
                    if let Expr::Value(v) = expr.as_ref()
                        && let Value::Placeholder(p) = &v.value
                    {
                        p.strip_prefix('$')
                            .and_then(|p| p.parse::<usize>().ok())
                            .and_then(|n| self.parameter_types.get(&n).copied())
                    } else {
                        None
                    }
                });
            ty == Some(PgType::Text) && source == Some(PgType::Catalog(2205))
        } else {
            false
        };
        self.regclass_text_casts.push(text_cast);
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<Self::Break> {
        if self.regclass_text_casts.pop().unwrap_or(false)
            && let Expr::Cast { expr, .. } = e
        {
            *e = attempt!(parse_expr(&attempt!(regclass_text_expression(
                &expr.to_string(),
                self.session,
                self.scope
            ))));
            return ControlFlow::Continue(());
        }
        let replaced = attempt!(self.expression(e));
        if let Some(sql) = replaced {
            *e = attempt!(parse_expr(&sql));
        }
        ControlFlow::Continue(())
    }
}
impl Rewrite<'_> {
    fn expression(&self, e: &mut Expr) -> Result<Option<String>> {
        if let Expr::CompoundFieldAccess { root, access_chain } = e {
            let vector = self
                .bindings
                .last()
                .and_then(|b| crate::result_types::access_source(root, access_chain, b))
                .is_some_and(|c| matches!(c.data_type, PgType::Catalog(22 | 30)));
            if vector {
                for access in access_chain {
                    match access {
                        AccessExpr::Subscript(Subscript::Index { index }) => {
                            *index = parse_expr(&format!("({index})+1"))?;
                            break;
                        }
                        AccessExpr::Subscript(_) => {
                            return Err(EngineError::unsupported(
                                "metadata vector slices are unsupported",
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Expr::BinaryOp { left, op, right } = e
            && matches!(
                op,
                BinaryOperator::PGRegexMatch
                    | BinaryOperator::PGRegexIMatch
                    | BinaryOperator::PGRegexNotMatch
                    | BinaryOperator::PGRegexNotIMatch
            )
        {
            let not = matches!(
                op,
                BinaryOperator::PGRegexNotMatch | BinaryOperator::PGRegexNotIMatch
            );
            let insensitive = matches!(
                op,
                BinaryOperator::PGRegexIMatch | BinaryOperator::PGRegexNotIMatch
            );
            return Ok(Some(format!(
                "{}regexp_matches({left},{right}{})",
                if not { "NOT " } else { "" },
                if insensitive { ",'i'" } else { "" }
            )));
        }
        if let Expr::Cast {
            expr, data_type, ..
        } = e
        {
            let ty = data_type.to_string().to_ascii_lowercase();
            if matches!(
                ty.as_str(),
                "regclass" | "pg_catalog.regclass" | "regproc" | "pg_catalog.regproc"
            ) && let Some(name) = string(expr)
            {
                if ty.ends_with("regclass") {
                    return Ok(Some(required_regclass(name, self.session, self.scope)?));
                }
                let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
                let oid = if ty.ends_with("regclass") {
                    catalog_schema().get(name).map(|f| f[0].oid)
                } else {
                    match name {
                        "array_in" => Some(750),
                        _ => None,
                    }
                };
                if let Some(oid) = oid {
                    return Ok(Some(format!("CAST({oid} AS UINTEGER)")));
                }
            }
            if let Some(PgType::Catalog(oid @ (1005 | 1007 | 1016 | 1028 | 1009))) =
                crate::result_types::type_from_name(&ty)
                && let Some(text) = string(expr)
            {
                let Cell::Array {
                    lower_bound,
                    values,
                } = crate::decode_parameter(Some(text.as_bytes()), PgType::Catalog(oid), false)?
                else {
                    unreachable!()
                };
                if lower_bound != 1 {
                    return Err(EngineError::unsupported(
                        "metadata array literals require a lower bound of one",
                    ));
                }
                let items = values
                    .iter()
                    .map(|v| match v {
                        Cell::Null => Ok("NULL".into()),
                        Cell::Text(t) => Ok(quote_literal(t)),
                        Cell::I16(n) => Ok(n.to_string()),
                        Cell::I32(n) => Ok(n.to_string()),
                        Cell::I64(n) => Ok(n.to_string()),
                        Cell::Oid(n) => Ok(n.to_string()),
                        _ => Err(EngineError::unsupported(
                            "unsupported metadata array literal",
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(Some(format!("[{}]", items.join(","))));
            }
            if ty.ends_with("regclass") {
                let source_type = crate::result_types::infer(expr, &vec![])
                    .map(|c| c.data_type)
                    .or_else(|| {
                        if let Expr::Value(v) = expr.as_ref()
                            && let Value::Placeholder(p) = &v.value
                        {
                            p.strip_prefix('$')
                                .and_then(|p| p.parse::<usize>().ok())
                                .and_then(|p| self.parameter_types.get(&p).copied())
                        } else {
                            None
                        }
                    });
                if matches!(source_type, Some(PgType::Text | PgType::Catalog(19))) {
                    return Ok(Some(regclass_expression(
                        &expr.to_string(),
                        self.session,
                        self.scope,
                        true,
                        true,
                    )?));
                }
            }
            let duck = match ty.as_str() {
                "oid" | "regclass" | "regtype" | "regproc" | "xid" => Some("UINTEGER"),
                "name" | "\"char\"" | "pg_node_tree" => Some("VARCHAR"),
                "int2vector" => Some("SMALLINT[]"),
                "oidvector" => Some("UINTEGER[]"),
                _ => None,
            };
            if let Some(duck) = duck {
                return Ok(Some(format!("CAST({expr} AS {duck})")));
            }
        }
        let Expr::Function(f) = e else {
            return Ok(None);
        };
        let name = function_name(f)?;
        if name == "count"
            && matches!(&f.args,FunctionArguments::List(l) if l.clauses.is_empty()&&l.args==vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)])
        {
            f.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new("count"))]);
            return Ok(None);
        }
        let a = args(f)?;
        if let Some(sql) = session_function(&name, &a, self.session, self.scope, self.pid)? {
            return Ok(Some(sql));
        }
        let p = |i: usize| -> Result<String> {
            a.get(i).map(ToString::to_string).ok_or_else(|| {
                EngineError::new("42883", "metadata function signature is unsupported")
            })
        };
        let sql = match (name.as_str(), a.len()) {
            ("to_regclass", 1) => {
                regclass_expression(&p(0)?, self.session, self.scope, false, true)?
            }
            ("pg_get_expr", 2 | 3) => p(0)?,
            ("pg_get_ruledef", 1 | 2) | ("pg_get_partkeydef" | "pg_tablespace_location", 1) => {
                "NULL::VARCHAR".into()
            }
            ("pg_database_size", 1) => "NULL::BIGINT".into(),
            ("pg_relation_size", 1 | 2)
            | (
                "pg_total_relation_size"
                | "pg_table_size"
                | "pg_indexes_size"
                | "pg_stat_get_numscans",
                1,
            ) => {
                let (category, field) = match name.as_str() {
                    "pg_stat_get_numscans" => ("activity", "scan_count"),
                    "pg_table_size" => ("storage", "table_bytes"),
                    "pg_indexes_size" => ("storage", "indexes_bytes"),
                    "pg_total_relation_size" => ("storage", "total_bytes"),
                    _ => ("storage", "main_bytes"),
                };
                let oid = if let Some(relation) = string(a[0]) {
                    if name == "pg_stat_get_numscans" {
                        format!("CAST({} AS UINTEGER)", quote_literal(relation))
                    } else {
                        required_regclass(relation, self.session, self.scope)?
                    }
                } else {
                    p(0)?
                };
                let statistics = scoped_relation_statistics(self.scope);
                let lookup = |field: &str| {
                    format!(
                        "(SELECT CAST(json_extract_string(s.{category},'$.{field}') AS BIGINT) FROM ({statistics}) s WHERE s.oid=({oid}) LIMIT 1)"
                    )
                };
                if name == "pg_relation_size" && a.len() == 2 {
                    let fork = p(1)?;
                    let class = scoped_catalog("pg_class", self.scope)?;
                    format!(
                        "CASE WHEN ({oid}) IS NULL OR ({fork}) IS NULL THEN NULL::BIGINT WHEN NOT EXISTS(SELECT 1 FROM ({class}) c WHERE c.oid=({oid})) THEN NULL::BIGINT ELSE CASE CAST(({fork}) AS VARCHAR) WHEN 'main' THEN {} WHEN 'fsm' THEN {} WHEN 'vm' THEN {} WHEN 'init' THEN {} ELSE CAST(error('__fed_pg_invalid_fork') AS BIGINT) END END",
                        lookup("main_bytes"),
                        lookup("fsm_bytes"),
                        lookup("vm_bytes"),
                        lookup("init_bytes")
                    )
                } else {
                    format!(
                        "CASE WHEN ({oid}) IS NULL THEN NULL::BIGINT ELSE {} END",
                        lookup(field)
                    )
                }
            }
            (
                "pg_relation_size"
                | "pg_total_relation_size"
                | "pg_table_size"
                | "pg_indexes_size"
                | "pg_stat_get_numscans"
                | "pg_database_size",
                _,
            ) => {
                return Err(EngineError::new(
                    "42883",
                    "metadata function signature is unsupported",
                ));
            }
            ("pg_get_constraintdef" | "pg_get_viewdef", 1 | 2) => {
                let parameter_type = if let Expr::Value(v) = a[0]
                    && let Value::Placeholder(n) = &v.value
                {
                    n.strip_prefix('$')
                        .and_then(|n| n.parse::<usize>().ok())
                        .and_then(|n| self.parameter_types.get(&n).copied())
                } else {
                    None
                };
                let text = parameter_type
                    .or_else(|| crate::result_types::infer(a[0], &vec![]).map(|c| c.data_type))
                    == Some(PgType::Text);
                let oid = if name == "pg_get_viewdef" && text {
                    regclass_expression(&p(0)?, self.session, self.scope, true, false)?
                } else {
                    p(0)?
                };
                format!(
                    "(SELECT d.definition FROM ({}) d WHERE d.oid=({oid}) AND d.kind={} LIMIT 1)",
                    scoped_definitions(self.scope),
                    quote_literal(if name == "pg_get_viewdef" {
                        "view"
                    } else {
                        "constraint"
                    })
                )
            }
            ("pg_get_indexdef", 1 | 3) => {
                let ordinal = if a.len() == 3 { p(1)? } else { "0".into() };
                format!(
                    "(SELECT CASE WHEN ({ordinal})=0 THEN d.definition ELSE list_extract(d.items,CAST({ordinal} AS BIGINT)) END FROM ({}) d WHERE d.oid={} AND d.kind='index' LIMIT 1)",
                    scoped_definitions(self.scope),
                    p(0)?
                )
            }
            ("format_type", 2) => {
                let oid = p(0)?;
                let modifier = p(1)?;
                format!(
                    "(SELECT CASE WHEN t.oid=1700 AND ({modifier})>=4 THEN 'numeric('||CAST((CAST({modifier} AS INTEGER)-4)>>16 AS VARCHAR)||','||CAST((CAST({modifier} AS INTEGER)-4)&65535 AS VARCHAR)||')' WHEN t.oid=1043 AND ({modifier})>=4 THEN 'character varying('||CAST(CAST({modifier} AS INTEGER)-4 AS VARCHAR)||')' ELSE CASE t.typname WHEN 'int2' THEN 'smallint' WHEN 'int4' THEN 'integer' WHEN 'int8' THEN 'bigint' WHEN 'bool' THEN 'boolean' WHEN 'varchar' THEN 'character varying' ELSE t.typname END END FROM ({}) t WHERE t.oid={oid} LIMIT 1)",
                    scoped_catalog("pg_type", self.scope)?
                )
            }
            ("obj_description", 1 | 2) | ("col_description", 2) => {
                let column = if name == "col_description" {
                    p(1)?
                } else {
                    "0".into()
                };
                let class = if name == "obj_description" && a.len() == 2 {
                    string(a[1])
                        .and_then(|n| catalog_schema().get(n))
                        .map(|f| f[0].oid)
                } else {
                    Some(catalog_schema()["pg_class"][0].oid)
                };
                format!(
                    "(SELECT d.description FROM ({}) d WHERE d.objoid={} AND d.objsubid={column} {} LIMIT 1)",
                    scoped_catalog("pg_description", self.scope)?,
                    p(0)?,
                    class
                        .map(|n| format!("AND d.classoid={n}"))
                        .unwrap_or_else(|| "AND false".into())
                )
            }
            ("pg_encoding_to_char", 1) => {
                format!("CASE WHEN {}=6 THEN 'UTF8' ELSE NULL::VARCHAR END", p(0)?)
            }
            ("pg_get_userbyid", 1) => format!(
                "CASE WHEN {}=10 THEN {} ELSE NULL::VARCHAR END",
                p(0)?,
                quote_literal(&self.scope.principal)
            ),
            ("_pg_expandarray", 1) => format!(
                "unnest(list_transform(range(1,array_length({})+1),__fed_n -> struct_pack(x := list_extract({},__fed_n), n := __fed_n)))",
                p(0)?,
                p(0)?
            ),
            ("array_upper" | "array_lower", 2) => {
                let vector = self
                    .bindings
                    .last()
                    .and_then(|b| crate::result_types::infer(a[0], b))
                    .is_some_and(|c| matches!(c.data_type, PgType::Catalog(22 | 30)));
                let bound = if name == "array_lower" {
                    if vector { "0".into() } else { "1".into() }
                } else {
                    format!("array_length({}){}", p(0)?, if vector { "-1" } else { "" })
                };
                format!(
                    "CASE WHEN {}=1 AND array_length({})>0 THEN {bound} ELSE NULL::INTEGER END",
                    p(1)?,
                    p(0)?
                )
            }
            ("pg_table_is_visible", 1) => {
                let c = scoped_catalog("pg_class", self.scope)?;
                let n = scoped_catalog("pg_namespace", self.scope)?;
                let paths = catalog_path(self.session, self.scope);
                let path = format!(
                    "[{}]",
                    paths
                        .iter()
                        .map(|p| quote_literal(p))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                format!(
                    "EXISTS(SELECT 1 FROM ({c}) c JOIN ({n}) n ON n.oid=c.relnamespace WHERE c.oid={} AND list_position({path},n.nspname)>0 AND NOT EXISTS(SELECT 1 FROM ({c}) c2 JOIN ({n}) n2 ON n2.oid=c2.relnamespace WHERE c2.relname=c.relname AND list_position({path},n2.nspname)<list_position({path},n.nspname)))",
                    p(0)?
                )
            }
            ("has_database_privilege", 2) => format!(
                "(upper({})='CONNECT' AND EXISTS(SELECT 1 FROM ({}) d WHERE d.datname=CAST({} AS VARCHAR) OR CAST(d.oid AS VARCHAR)=CAST({} AS VARCHAR)))",
                p(1)?,
                scoped_catalog("pg_database", self.scope)?,
                p(0)?,
                p(0)?
            ),
            ("has_schema_privilege", 2) => format!(
                "(contains(upper({}),'USAGE') AND EXISTS(SELECT 1 FROM ({}) n WHERE n.nspname=CAST({} AS VARCHAR) OR CAST(n.oid AS VARCHAR)=CAST({} AS VARCHAR)))",
                p(1)?,
                scoped_catalog("pg_namespace", self.scope)?,
                p(0)?,
                p(0)?
            ),
            ("has_table_privilege", 2) => format!(
                "(upper({})='SELECT' AND EXISTS(SELECT 1 FROM ({}) c WHERE c.oid={}))",
                p(1)?,
                scoped_catalog("pg_class", self.scope)?,
                p(0)?
            ),
            _ => {
                const SAFE: &[&str] = &[
                    "coalesce",
                    "nullif",
                    "lower",
                    "upper",
                    "replace",
                    "concat",
                    "length",
                    "char_length",
                    "substring",
                    "substr",
                    "trim",
                    "ltrim",
                    "rtrim",
                    "array_length",
                    "cardinality",
                    "array_to_string",
                    "row_number",
                    "count",
                    "min",
                    "max",
                    "sum",
                    "avg",
                    "abs",
                    "round",
                    "floor",
                    "ceil",
                    "string_agg",
                    "bool_and",
                    "bool_or",
                ];
                if !SAFE.contains(&name.as_str()) {
                    return Err(EngineError::unsupported(format!(
                        "metadata function {name} is outside the compatibility matrix"
                    )));
                }
                f.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]);
                return Ok(None);
            }
        };
        Ok(Some(sql))
    }
}

fn generator(name: &ObjectName, args: &[FunctionArg]) -> Result<()> {
    let names = parts(name)?;
    if !matches!(names.as_slice(),[n] if n=="generate_series")
        && !matches!(names.as_slice(),[s,n] if s=="pg_catalog"&&n=="generate_series")
    {
        return Err(EngineError::unsupported(
            "metadata table function is outside the compatibility matrix",
        ));
    }
    if args.len() != 2 {
        return Err(EngineError::unsupported(
            "metadata generator requires two bounded integer literals",
        ));
    }
    let values = args
        .iter()
        .map(|a| {
            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v))) = a
                && let Value::Number(n, _) = &v.value
            {
                n.parse::<i32>().ok()
            } else {
                None
            }
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            EngineError::unsupported("metadata generator requires bounded integer literals")
        })?;
    if values[0] < 0 || values[1] < 0 || i64::from(values[1]) - i64::from(values[0]) > 1024 {
        return Err(EngineError::new(
            "54000",
            "metadata generator exceeds 1024 rows",
        ));
    }
    Ok(())
}

pub fn catalog_path(session: &SessionContext, scope: &MetadataScope) -> Vec<String> {
    let mut paths = effective_path(session, scope);
    if !paths.iter().any(|p| p == "pg_catalog") {
        paths.insert(0, "pg_catalog".into());
    }
    paths
}
/// PG's quote_identifier policy, distinct from always-quoted SQL generation.
pub fn display_identifier(name: &str) -> String {
    static KEYWORDS: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    let keywords = KEYWORDS.get_or_init(|| {
        let words: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../catalog/pg16-keywords.json"))
                .expect("checked-in keywords");
        words
            .into_iter()
            .filter(|w| w["catcode"] != "U")
            .map(|w| w["word"].as_str().unwrap().to_owned())
            .collect()
    });
    let mut chars = name.chars();
    if chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_lowercase())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_lowercase() || c.is_ascii_digit())
        && !keywords.contains(name)
    {
        name.into()
    } else {
        quote_identifier(name)
    }
}
/// Parse only a relation identifier, never a SELECT or arbitrary SQL fragment.
pub fn relation_name_parts(name: &str) -> Result<Vec<String>> {
    let invalid = || EngineError::new("42602", "invalid relation identifier");
    let mut chars = name.chars().peekable();
    let mut parts = Vec::new();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                let c = chars.next().ok_or_else(invalid)?;
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        part.push('"');
                    } else {
                        break;
                    }
                } else if c == '\0' {
                    return Err(invalid());
                } else {
                    part.push(c);
                }
            }
        } else {
            while let Some(c) = chars.peek().copied() {
                if c == '.' || c.is_ascii_whitespace() {
                    break;
                }
                if c == '"' || c == '\0' {
                    return Err(invalid());
                }
                chars.next();
                part.push(c.to_ascii_lowercase());
            }
        }
        if part.is_empty() {
            return Err(invalid());
        }
        parts.push(part);
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => break,
            Some('.') => {}
            _ => return Err(invalid()),
        }
    }
    Ok(parts)
}
pub fn regclass_numeric_input(name: &str) -> Result<Option<u32>> {
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        name.parse::<u32>()
            .map(Some)
            .map_err(|_| EngineError::new("22003", "value is out of range for type oid"))
    } else {
        Ok(None)
    }
}
/// Runtime name parsing produces data only; soft failures implement to_regclass.
pub fn regclass_input(name: &str) -> Option<String> {
    match regclass_numeric_input(name) {
        Ok(Some(oid)) => return Some(serde_json::json!({"oid":oid}).to_string()),
        Err(_) => return Some(serde_json::json!({"error":"oid_range"}).to_string()),
        Ok(None) => {}
    }
    relation_name_parts(name)
        .ok()
        .map(|p| serde_json::json!({"parts":p}).to_string())
}
fn regclass_text_expression(
    oid: &str,
    session: &SessionContext,
    scope: &MetadataScope,
) -> Result<String> {
    let c = scoped_catalog("pg_class", scope)?;
    let n = scoped_catalog("pg_namespace", scope)?;
    let path = format!(
        "[{}]",
        catalog_path(session, scope)
            .iter()
            .map(|p| quote_literal(p))
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(format!(
        "CASE WHEN ({oid})=0 THEN '-' ELSE coalesce((SELECT CASE WHEN list_position({path},n.nspname)>0 AND NOT EXISTS(SELECT 1 FROM ({c}) c2 JOIN ({n}) n2 ON n2.oid=c2.relnamespace WHERE c2.relname=c.relname AND list_position({path},n2.nspname)>0 AND list_position({path},n2.nspname)<list_position({path},n.nspname)) THEN __fed_pg_quote_ident(c.relname) ELSE __fed_pg_quote_ident(n.nspname)||'.'||__fed_pg_quote_ident(c.relname) END FROM ({c}) c JOIN ({n}) n ON n.oid=c.relnamespace WHERE c.oid=({oid}) LIMIT 1),CAST(({oid}) AS VARCHAR)) END"
    ))
}
fn regclass_expression(
    expr: &str,
    session: &SessionContext,
    scope: &MetadataScope,
    required: bool,
    numeric_oid: bool,
) -> Result<String> {
    let parser = if numeric_oid {
        "__fed_pg_regclass_input"
    } else {
        "__fed_pg_relation_parts"
    };
    let parsed = format!("{parser}(CAST(({expr}) AS VARCHAR))");
    let part = |i: usize| format!("json_extract_string({parsed},'$.parts[{i}]')");
    let len = format!("json_array_length({parsed},'$.parts')");
    let path = format!(
        "[{}]",
        catalog_path(session, scope)
            .iter()
            .map(|p| quote_literal(p))
            .collect::<Vec<_>>()
            .join(",")
    );
    let lookup = format!(
        "(SELECT c.oid FROM ({}) c JOIN ({}) n ON n.oid=c.relnamespace WHERE c.relname=CASE WHEN {len}=1 THEN {} WHEN {len}=2 THEN {} ELSE {} END AND CASE WHEN {len}=1 THEN list_position({path},n.nspname)>0 WHEN {len}=2 THEN n.nspname={} ELSE n.nspname={} END ORDER BY list_position({path},n.nspname) LIMIT 1)",
        scoped_catalog("pg_class", scope)?,
        scoped_catalog("pg_namespace", scope)?,
        part(0),
        part(1),
        part(2),
        part(0),
        part(1)
    );
    let lookup = if required {
        format!("coalesce({lookup},CAST(error('__fed_pg_relation_missing') AS UINTEGER))")
    } else {
        lookup
    };
    let invalid = if required {
        "CAST(error('__fed_pg_relation_name') AS UINTEGER)"
    } else {
        "NULL::UINTEGER"
    };
    Ok(format!(
        "CASE WHEN ({expr}) IS NULL THEN NULL::UINTEGER WHEN {parsed} IS NULL THEN {invalid} WHEN json_extract_string({parsed},'$.oid') IS NOT NULL THEN CAST(json_extract_string({parsed},'$.oid') AS UINTEGER) WHEN {len}=3 AND {}<>{} THEN CAST(error('__fed_pg_cross_database') AS UINTEGER) WHEN {len}>3 THEN {invalid} ELSE {lookup} END",
        part(0),
        quote_literal(&session.database)
    ))
}
fn regclass(name: &str, session: &SessionContext, scope: &MetadataScope) -> Result<String> {
    if let Some(oid) = regclass_numeric_input(name)? {
        return Ok(format!("CAST({oid} AS UINTEGER)"));
    }
    let p = relation_name_parts(name)?;
    let (relation, schema) = match p.as_slice() {
        [n] => (n, None),
        [s, n] => (n, Some(s)),
        [db, s, n] if db == &session.database => (n, Some(s)),
        [_, _, _] => {
            return Err(EngineError::unsupported(
                "cross-database references are not implemented",
            ));
        }
        _ => {
            return Err(EngineError::unsupported(
                "regclass requires a relation name within the current database",
            ));
        }
    };
    let paths = catalog_path(session, scope);
    let path = format!(
        "[{}]",
        paths
            .iter()
            .map(|p| quote_literal(p))
            .collect::<Vec<_>>()
            .join(",")
    );
    let filter = schema
        .map(|s| format!("n.nspname={}", quote_literal(s)))
        .unwrap_or_else(|| format!("list_position({path},n.nspname)>0"));
    Ok(format!(
        "(SELECT c.oid FROM ({}) c JOIN ({}) n ON n.oid=c.relnamespace WHERE c.relname={} AND {filter} ORDER BY list_position({path},n.nspname) LIMIT 1)",
        scoped_catalog("pg_class", scope)?,
        scoped_catalog("pg_namespace", scope)?,
        quote_literal(relation)
    ))
}
fn required_regclass(
    name: &str,
    session: &SessionContext,
    scope: &MetadataScope,
) -> Result<String> {
    Ok(format!(
        "coalesce({},CAST(error('__fed_pg_relation_missing') AS UINTEGER))",
        regclass(name, session, scope)?
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> (SessionContext, MetadataScope) {
        let s = SessionContext {
            database: "fedengine".into(),
            ..Default::default()
        };
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: s.database.clone(),
            principal: "tester".into(),
            catalogs: vec![],
        };
        (s, scope)
    }
    #[test]
    fn session_values_never_use_duckdb_identity() {
        let (s, scope) = context();
        for sql in [
            "SELECT current_catalog",
            "SELECT current_database()",
            "SELECT current_user",
            "SELECT pg_catalog.current_schemas(true)",
        ] {
            let CompatibilityPlan::Metadata(p) = plan_compatibility(sql, &s, &scope, 321).unwrap()
            else {
                panic!("not metadata")
            };
            let lowered = parse_pg(&p.sql).unwrap();
            let flow = visit_expressions(&lowered, |expr| {
                let unreplaced = match expr {
                    Expr::Function(f) => function_name(f).is_ok_and(|n| n.starts_with("current_")),
                    Expr::Identifier(i) => i.value.starts_with("current_"),
                    _ => false,
                };
                if unreplaced {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            assert!(flow.is_continue(), "{}", p.sql);
        }
    }
    #[test]
    fn metadata_cannot_escape_its_read_scope() {
        let (s, scope) = context();
        for sql in [
            "SELECT * FROM _fedengine.meta_pg_class",
            "SELECT * FROM pg_class JOIN business ON true",
            "SELECT * INTO stolen FROM pg_class",
            "WITH x AS (DELETE FROM pg_class RETURNING *) SELECT * FROM x",
            "SELECT read_text('/etc/passwd') FROM pg_class",
            "SELECT * FROM pg_class,pg_catalog.read_csv('/etc/passwd')",
            "SELECT * FROM pg_class,generate_series(1,999999999)",
            "WITH RECURSIVE x AS (SELECT * FROM pg_class) SELECT * FROM x",
        ] {
            assert!(plan_compatibility(sql, &s, &scope, 1).is_err(), "{sql}");
        }
    }
    #[test]
    fn storage_function_parameter_types_retain_postgres_provenance() {
        let (session, scope) = context();
        for (sql, expected) in [
            ("SELECT pg_relation_size($1)", PgType::Catalog(2205)),
            ("SELECT pg_stat_get_numscans($1)", PgType::Catalog(26)),
            ("SELECT pg_relation_size($1::oid)", PgType::Catalog(26)),
            (
                "SELECT pg_relation_size($1::regclass)",
                PgType::Catalog(2205),
            ),
        ] {
            let CompatibilityPlan::Metadata(plan) =
                plan_compatibility(sql, &session, &scope, 1).unwrap()
            else {
                panic!("not metadata");
            };
            assert_eq!(plan.parameter_types.get(&1), Some(&expected));
        }
    }
}
