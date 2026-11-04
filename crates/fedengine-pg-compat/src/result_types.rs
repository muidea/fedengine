//! Result type provenance for catalog fields, aliases, derived relations and casts.
use crate::catalog_schema;
use fedengine_contract::*;
use fedengine_core::*;
use sqlparser::ast::*;
use std::collections::BTreeMap;

pub(crate) type Bindings = Vec<(String, Vec<Option<ColumnMeta>>)>;
fn ident(i: &Ident) -> String {
    if i.quote_style.is_some() {
        i.value.clone()
    } else {
        i.value.to_lowercase()
    }
}
pub(crate) fn type_from_name(name: &str) -> Option<PgType> {
    Some(
        match name.to_ascii_lowercase().trim_start_matches("pg_catalog.") {
            "oid" => PgType::Catalog(26),
            "name" => PgType::Catalog(19),
            "\"char\"" => PgType::Catalog(18),
            "regproc" => PgType::Catalog(24),
            "regclass" => PgType::Catalog(2205),
            "regtype" => PgType::Catalog(2206),
            "int2vector" => PgType::Catalog(22),
            "oidvector" => PgType::Catalog(30),
            "smallint[]" | "int2[]" => PgType::Catalog(1005),
            "oid[]" => PgType::Catalog(1028),
            "text[]" | "varchar[]" | "name[]" => PgType::Catalog(1009),
            "smallint" | "int2" => PgType::Int2,
            "int" | "integer" | "int4" => PgType::Int4,
            "bigint" | "int8" => PgType::Int8,
            "boolean" | "bool" => PgType::Bool,
            "text" | "varchar" | "character varying" => PgType::Text,
            "real" | "float4" => PgType::Float4,
            "double precision" | "float8" => PgType::Float8,
            "bytea" => PgType::Bytea,
            n if n.starts_with("numeric") || n.starts_with("decimal") => PgType::Numeric,
            _ => return None,
        },
    )
}
fn sys_fields(name: &ObjectName) -> Vec<Option<ColumnMeta>> {
    let p = name
        .0
        .iter()
        .filter_map(|p| {
            if let ObjectNamePart::Identifier(i) = p {
                Some(ident(i))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let key = match p.as_slice() {
        [n] => n.clone(),
        [s, n] if s == "pg_catalog" => n.clone(),
        [s, n] if s == "information_schema" => format!("{s}.{n}"),
        _ => return vec![],
    };
    catalog_schema()
        .get(&key)
        .map(|fields| {
            fields
                .iter()
                .map(|f| {
                    let mut column = ColumnMeta::new(
                        &f.attname,
                        PgType::from_oid(f.atttypid).unwrap_or(PgType::Catalog(f.atttypid)),
                    );
                    column.type_modifier = f.atttypmod;
                    column.table_oid = Some(f.oid);
                    column.column_attnum = Some(f.attnum);
                    Some(column)
                })
                .collect()
        })
        .unwrap_or_default()
}
fn factor(
    t: &TableFactor,
    ctes: &BTreeMap<String, Vec<Option<ColumnMeta>>>,
) -> Result<(String, Vec<Option<ColumnMeta>>)> {
    let (name, mut columns, alias) = match t {
        TableFactor::Table {
            name, alias, args, ..
        } => {
            let n = name
                .0
                .last()
                .map(ToString::to_string)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let fields = if args.is_some() && n == "pg_get_keywords" {
                [
                    ("word", PgType::Text),
                    ("catcode", PgType::Catalog(18)),
                    ("barelabel", PgType::Bool),
                    ("catdesc", PgType::Text),
                    ("baredesc", PgType::Text),
                ]
                .into_iter()
                .map(|(n, t)| Some(ColumnMeta::new(n, t)))
                .collect()
            } else if args.is_some() {
                vec![Some(ColumnMeta::new("generate_series", PgType::Int4))]
            } else {
                ctes.get(&n).cloned().unwrap_or_else(|| sys_fields(name))
            };
            (n, fields, alias)
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => (String::new(), query_columns(subquery, ctes)?, alias),
        _ => return Ok((String::new(), vec![])),
    };
    if let Some(alias) = alias {
        for (column, name) in columns.iter_mut().zip(&alias.columns) {
            if let Some(c) = column {
                c.name = ident(&name.name);
            }
        }
        Ok((ident(&alias.name), columns))
    } else {
        Ok((name, columns))
    }
}
pub(crate) fn bindings(q: &Query) -> Result<Bindings> {
    bindings_inner(q, &BTreeMap::new())
}
fn bindings_inner(
    q: &Query,
    parent: &BTreeMap<String, Vec<Option<ColumnMeta>>>,
) -> Result<Bindings> {
    let mut ctes = parent.clone();
    if let Some(w) = &q.with {
        for cte in &w.cte_tables {
            let mut columns = query_columns(&cte.query, &ctes)?;
            for (column, name) in columns.iter_mut().zip(&cte.alias.columns) {
                if let Some(c) = column {
                    c.name = ident(&name.name);
                }
            }
            ctes.insert(ident(&cte.alias.name), columns);
        }
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    for from in &s.from {
        result.push(factor(&from.relation, &ctes)?);
        for join in &from.joins {
            result.push(factor(&join.relation, &ctes)?);
        }
    }
    Ok(result)
}
pub(crate) fn access_source(root: &Expr, chain: &[AccessExpr], b: &Bindings) -> Option<ColumnMeta> {
    let mut source = root.clone();
    for access in chain {
        if let AccessExpr::Dot(Expr::Identifier(field)) = access {
            source = match source {
                Expr::Identifier(i) => Expr::CompoundIdentifier(vec![i, field.clone()]),
                _ => break,
            };
        } else {
            break;
        }
    }
    infer(&source, b)
}
pub(crate) fn infer(expr: &Expr, b: &Bindings) -> Option<ColumnMeta> {
    let new = |ty| Some(ColumnMeta::new(label(expr), ty));
    match expr {
        Expr::Identifier(i) => {
            let n = ident(i);
            let cols: Vec<_> = b
                .iter()
                .flat_map(|(_, c)| c.iter().flatten())
                .filter(|c| c.name == n)
                .collect();
            if cols.len() == 1 {
                Some(cols[0].clone())
            } else {
                None
            }
        }
        Expr::CompoundIdentifier(p) if p.len() == 2 => b
            .iter()
            .find(|(a, _)| a == &ident(&p[0]))
            .and_then(|(_, c)| c.iter().flatten().find(|c| c.name == ident(&p[1])))
            .cloned(),
        Expr::Nested(e) => infer(e, b),
        Expr::Cast { data_type, .. } => type_from_name(&data_type.to_string()).and_then(new),
        Expr::Value(v) => match &v.value {
            Value::Boolean(_) => new(PgType::Bool),
            Value::SingleQuotedString(_) => new(PgType::Text),
            Value::Number(n, _) if !n.contains('.') => new(if n.parse::<i32>().is_ok() {
                PgType::Int4
            } else {
                PgType::Int8
            }),
            _ => None,
        },
        Expr::Function(f) => {
            let n = f
                .name
                .to_string()
                .split('.')
                .next_back()
                .unwrap_or("")
                .to_ascii_lowercase();
            match n.as_str() {
                "current_database"
                | "current_catalog"
                | "current_schema"
                | "current_user"
                | "session_user"
                | "version"
                | "format_type"
                | "current_setting"
                | "pg_get_expr"
                | "pg_get_constraintdef"
                | "pg_get_indexdef"
                | "pg_get_viewdef"
                | "pg_get_ruledef"
                | "obj_description"
                | "col_description"
                | "pg_get_userbyid"
                | "pg_encoding_to_char" => new(PgType::Text),
                "to_regclass" => new(PgType::Catalog(2205)),
                "current_schemas" => new(PgType::Catalog(1009)),
                "pg_backend_pid" | "array_upper" | "array_lower" | "array_length"
                | "cardinality" => new(PgType::Int4),
                "count"
                | "row_number"
                | "pg_relation_size"
                | "pg_table_size"
                | "pg_indexes_size"
                | "pg_total_relation_size"
                | "pg_database_size"
                | "pg_stat_get_numscans" => new(PgType::Int8),
                "pg_table_is_visible"
                | "has_table_privilege"
                | "has_schema_privilege"
                | "has_database_privilege" => new(PgType::Bool),
                "coalesce" | "nullif" => {
                    if let FunctionArguments::List(args) = &f.args {
                        args.args.iter().find_map(|a| {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                                infer(e, b)
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        Expr::CompoundFieldAccess { root, access_chain } => {
            if access_chain
                .iter()
                .any(|a| matches!(a,AccessExpr::Dot(Expr::Identifier(i)) if i.value=="n"))
            {
                return new(PgType::Int4);
            }
            let t = access_source(root, access_chain, b)?.data_type;
            new(match t {
                PgType::Catalog(22 | 1005) => PgType::Int2,
                PgType::Catalog(30 | 1028) => PgType::Catalog(26),
                PgType::Catalog(1009 | 1003 | 1015) => PgType::Text,
                _ => return None,
            })
        }
        Expr::BinaryOp {
            op:
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::Gt
                | BinaryOperator::LtEq
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
                | BinaryOperator::PGRegexMatch
                | BinaryOperator::PGRegexNotMatch,
            ..
        } => new(PgType::Bool),
        _ => None,
    }
}
fn label(e: &Expr) -> String {
    match e {
        Expr::Identifier(i) => ident(i),
        Expr::CompoundIdentifier(p) => p.last().map(ident).unwrap_or_default(),
        Expr::Function(f) => f
            .name
            .to_string()
            .split('.')
            .next_back()
            .unwrap_or("?column?")
            .into(),
        Expr::Nested(expr) => label(expr),
        Expr::Cast {
            expr, data_type, ..
        } => {
            let source = label(expr);
            if source != "?column?" {
                return source;
            }
            type_from_name(&data_type.to_string())
                .and_then(|typ| {
                    crate::builtin_types()
                        .iter()
                        .find(|t| t["oid"].as_u64() == Some(u64::from(typ.oid())))
                })
                .and_then(|t| t["typname"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| data_type.to_string().to_ascii_lowercase())
        }
        _ => "?column?".into(),
    }
}
/// Preserve client-visible names before parameter typing introduces synthetic casts.
pub(crate) fn preserve_names(statement: &mut Statement) {
    struct Names;
    impl VisitorMut for Names {
        type Break = ();
        fn pre_visit_query(&mut self, q: &mut Query) -> std::ops::ControlFlow<()> {
            if let SetExpr::Select(select) = q.body.as_mut() {
                for item in &mut select.projection {
                    if let SelectItem::UnnamedExpr(expr) = item {
                        *item = SelectItem::ExprWithAlias {
                            alias: Ident::with_quote('"', label(expr)),
                            expr: expr.clone(),
                        };
                    }
                }
            }
            std::ops::ControlFlow::Continue(())
        }
    }
    let _ = VisitMut::visit(statement, &mut Names);
}
fn query_columns(
    q: &Query,
    ctes: &BTreeMap<String, Vec<Option<ColumnMeta>>>,
) -> Result<Vec<Option<ColumnMeta>>> {
    if let SetExpr::SetOperation { left, right, .. } = q.body.as_ref() {
        let mut l = q.clone();
        l.body = left.clone();
        let mut r = q.clone();
        r.body = right.clone();
        let a = query_columns(&l, ctes)?;
        let b = query_columns(&r, ctes)?;
        if a.len() != b.len() {
            return Err(EngineError::unsupported(
                "metadata set operation has incompatible projections",
            ));
        }
        return a
            .into_iter()
            .zip(b)
            .map(|(a, b)| match (a, b) {
                (Some(mut a), Some(b)) if a.data_type == b.data_type => {
                    a.table_oid = None;
                    a.column_attnum = None;
                    Ok(Some(a))
                }
                (None, None) => Ok(None),
                _ => Err(EngineError::unsupported(
                    "metadata set operation requires explicit matching result casts",
                )),
            })
            .collect();
    }
    if let SetExpr::Query(inner) = q.body.as_ref() {
        return query_columns(inner, ctes);
    }
    let b = bindings_inner(q, ctes)?;
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) => result.push(infer(e, &b)),
            SelectItem::ExprWithAlias { expr, alias } => {
                result.push(infer(expr, &b).map(|mut c| {
                    c.name = ident(alias);
                    c
                }))
            }
            SelectItem::Wildcard(options) if *options == WildcardAdditionalOptions::default() => {
                result.extend(b.iter().flat_map(|(_, c)| c.iter().cloned()))
            }
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(name),
                options,
            ) if *options == WildcardAdditionalOptions::default() => {
                let alias = name.to_string().trim_matches('"').to_string();
                if let Some((_, columns)) = b.iter().find(|(a, _)| a == &alias) {
                    result.extend(columns.clone());
                }
            }
            _ => {
                return Err(EngineError::unsupported(
                    "metadata wildcard modifier is unsupported",
                ));
            }
        }
    }
    Ok(result)
}
pub(crate) fn result_columns(s: &Statement) -> Result<Vec<Option<ColumnMeta>>> {
    if let Statement::Query(q) = s {
        query_columns(q, &BTreeMap::new())
    } else {
        Ok(vec![])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derived_vector_subscript_provenance() {
        let stmt=crate::parse_pg("SELECT tmp.I_INDOPTION[tmp.ORDINAL_POSITION-1] FROM (SELECT i.indoption AS I_INDOPTION,1 AS ORDINAL_POSITION FROM pg_index i) tmp").unwrap().remove(0);
        let columns = result_columns(&stmt).unwrap();
        assert_eq!(columns[0].as_ref().unwrap().data_type, PgType::Int2);
        let session = SessionContext {
            database: "fedengine".into(),
            ..Default::default()
        };
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: session.database.clone(),
            principal: "test".into(),
            catalogs: vec![],
        };
        let crate::CompatibilityPlan::Metadata(plan) =
            crate::plan_compatibility(&stmt.to_string(), &session, &scope, 1).unwrap()
        else {
            panic!("not metadata")
        };
        assert!(plan.sql.contains("+ 1]"), "{}", plan.sql);
    }
}
