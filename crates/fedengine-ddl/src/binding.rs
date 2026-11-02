use crate::{Analyzer, ObjectRef, analysis::classify};
use fedengine_core::*;
use sqlparser::ast::*;
use std::{collections::BTreeSet, ops::ControlFlow};
impl Analyzer {
    pub(super) fn parts(name: &ObjectName) -> Result<Vec<String>> {
        name.0
            .iter()
            .map(|p| match p {
                ObjectNamePart::Identifier(i) => Ok(if i.quote_style.is_some() {
                    i.value.clone()
                } else {
                    i.value.to_lowercase()
                }),
                _ => Err(EngineError::unsupported(
                    "computed object names are disabled",
                )),
            })
            .collect()
    }
    pub fn bind(&self, name: &ObjectName, schema_object: bool) -> Result<ObjectRef> {
        let p = Self::parts(name)?;
        let enabled: Vec<_> = self.sources.iter().filter(|s| s.enabled).collect();
        let default = || {
            if enabled.len() == 1 {
                Ok(enabled[0])
            } else {
                Err(EngineError::new(
                    "3D000",
                    "object requires an unambiguous registered catalog",
                ))
            }
        };
        let (source, schema, object) = if schema_object {
            match p.as_slice() {
                [catalog, schema] => (
                    enabled
                        .iter()
                        .find(|s| s.catalog_alias == *catalog)
                        .copied()
                        .ok_or_else(|| EngineError::new("3D000", "unknown catalog"))?,
                    schema.clone(),
                    schema.clone(),
                ),
                [schema] => (default()?, schema.clone(), schema.clone()),
                _ => return Err(EngineError::unsupported("schema must be catalog.schema")),
            }
        } else {
            match p.as_slice() {
                [catalog, schema, object] => (
                    enabled
                        .iter()
                        .find(|s| s.catalog_alias == *catalog)
                        .copied()
                        .ok_or_else(|| EngineError::new("3D000", "unknown catalog"))?,
                    schema.clone(),
                    object.clone(),
                ),
                [first, object] => {
                    if let Some(s) = enabled.iter().find(|s| s.catalog_alias == *first) {
                        (*s, s.default_schema.clone(), object.clone())
                    } else {
                        (default()?, first.clone(), object.clone())
                    }
                }
                [object] => {
                    let s = default()?;
                    (s, s.default_schema.clone(), object.clone())
                }
                _ => return Err(EngineError::unsupported("invalid object qualification")),
            }
        };
        if !source.allowed_schemas.contains(&schema)
            || ["pg_catalog", "information_schema", "sys", "mysql"]
                .contains(&schema.to_ascii_lowercase().as_str())
        {
            return Err(EngineError::denied(
                "namespace is outside the data source allowlist",
            ));
        }
        Ok(ObjectRef {
            source: source.id.clone(),
            catalog: source.catalog_alias.clone(),
            schema,
            name: object,
        })
    }
}
pub(super) fn object_name(parts: &[&str]) -> ObjectName {
    ObjectName(
        parts
            .iter()
            .map(|p| ObjectNamePart::Identifier(Ident::with_quote('"', *p)))
            .collect(),
    )
}
pub(super) struct Binder<'a> {
    analyzer: &'a Analyzer,
    pub(super) objects: BTreeSet<ObjectRef>,
    ctes: Vec<BTreeSet<String>>,
    builtin_relation: Option<ObjectName>,
    depth: usize,
}
impl<'a> Binder<'a> {
    pub(super) fn new(analyzer: &'a Analyzer) -> Self {
        Self {
            analyzer,
            objects: BTreeSet::new(),
            ctes: Vec::new(),
            builtin_relation: None,
            depth: 0,
        }
    }

    fn relation(&mut self, n: &mut ObjectName) -> Result<()> {
        let p = Analyzer::parts(n)?;
        if p.len() == 1 && self.ctes.iter().rev().any(|s| s.contains(&p[0])) {
            return Ok(());
        }
        // Built-in bounded generators are table functions, handled separately by pre_visit_table_factor.
        if self.builtin_relation.as_ref() == Some(n) {
            self.builtin_relation.take();
            return Ok(());
        }
        let o = self.analyzer.bind(n, false)?;
        *n = object_name(&[&o.catalog, &o.schema, &o.name]);
        self.objects.insert(o);
        Ok(())
    }
}
impl VisitorMut for Binder<'_> {
    type Break = EngineError;
    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<EngineError> {
        if !q.locks.is_empty() {
            return ControlFlow::Break(EngineError::unsupported("locking reads are disabled"));
        }
        let mut scope = BTreeSet::new();
        if let Some(w) = &q.with {
            for c in &w.cte_tables {
                scope.insert(c.alias.name.value.to_lowercase());
            }
        }
        self.ctes.push(scope);
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<EngineError> {
        self.ctes.pop();
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, s: &mut Select) -> ControlFlow<EngineError> {
        if s.into.is_some() {
            ControlFlow::Break(EngineError::unsupported("SELECT INTO is disabled"))
        } else {
            ControlFlow::Continue(())
        }
    }
    fn pre_visit_statement(&mut self, s: &mut Statement) -> ControlFlow<EngineError> {
        if self.depth > 0 && !matches!(s, Statement::Query(_)) {
            return ControlFlow::Break(EngineError::unsupported(
                "nested write statements are disabled",
            ));
        }
        self.depth += 1;
        match classify(s) {
            Ok(_) => ControlFlow::Continue(()),
            Err(e) => ControlFlow::Break(e),
        }
    }
    fn post_visit_statement(&mut self, _: &mut Statement) -> ControlFlow<EngineError> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, n: &mut ObjectName) -> ControlFlow<EngineError> {
        match self.relation(n) {
            Ok(()) => ControlFlow::Continue(()),
            Err(e) => ControlFlow::Break(e),
        }
    }
    fn pre_visit_table_factor(&mut self, t: &mut TableFactor) -> ControlFlow<EngineError> {
        match t {
            TableFactor::Table {
                name,
                args: Some(_),
                ..
            } => {
                self.builtin_relation = Some(name.clone());
                let name = name.to_string().to_lowercase();
                if !["range", "generate_series"].contains(&name.as_str()) {
                    return ControlFlow::Break(EngineError::denied(
                        "table function is not allowed",
                    ));
                }
            }
            TableFactor::Table { .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. } => {}
            _ => {
                return ControlFlow::Break(EngineError::unsupported(
                    "table factor cannot be proven safe",
                ));
            }
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<EngineError> {
        if let Expr::Function(f) = e {
            let name = f.name.to_string().to_lowercase();
            const ALLOWED: &[&str] = &[
                "count",
                "sum",
                "avg",
                "min",
                "max",
                "abs",
                "round",
                "floor",
                "ceil",
                "ceiling",
                "lower",
                "upper",
                "length",
                "char_length",
                "substring",
                "substr",
                "trim",
                "ltrim",
                "rtrim",
                "concat",
                "concat_ws",
                "coalesce",
                "nullif",
                "greatest",
                "least",
                "date_trunc",
                "date_part",
                "extract",
                "now",
                "current_timestamp",
                "current_date",
                "row_number",
                "rank",
                "dense_rank",
                "lag",
                "lead",
                "first_value",
                "last_value",
                "string_agg",
                "bool_and",
                "bool_or",
                "stddev",
                "variance",
                "sqrt",
                "power",
                "mod",
                "replace",
                "regexp_matches",
                "regexp_replace",
                "version",
                "current_database",
                "current_schema",
            ];
            if !ALLOWED.contains(&name.as_str()) {
                return ControlFlow::Break(EngineError::denied(
                    "function is not in the safe function whitelist",
                ));
            }
        }
        ControlFlow::Continue(())
    }
}
pub(super) fn bind_constraints(
    b: &mut Binder<'_>,
    columns: &mut [ColumnDef],
    constraints: &mut [TableConstraint],
) -> Result<()> {
    for c in columns {
        validate_scalar_type(&c.data_type)?;
        for option in &mut c.options {
            match &mut option.option {
                ColumnOption::ForeignKey(f) => b.relation(&mut f.foreign_table)?,
                ColumnOption::Null
                | ColumnOption::NotNull
                | ColumnOption::Default(_)
                | ColumnOption::PrimaryKey(_)
                | ColumnOption::Unique(_)
                | ColumnOption::Check(_) => {}
                _ => return Err(EngineError::unsupported("column option is not enabled")),
            }
        }
    }
    for c in constraints {
        match c {
            TableConstraint::ForeignKey(f) => b.relation(&mut f.foreign_table)?,
            TableConstraint::PrimaryKey(_)
            | TableConstraint::Unique(_)
            | TableConstraint::Check(_) => {}
            _ => return Err(EngineError::unsupported("table constraint is not enabled")),
        }
    }
    Ok(())
}
pub(super) fn validate_scalar_type(ty: &DataType) -> Result<()> {
    let name = ty.to_string().to_uppercase();
    let base = name.split('(').next().unwrap().trim();
    if ![
        "BOOLEAN",
        "BOOL",
        "SMALLINT",
        "INT2",
        "INTEGER",
        "INT",
        "INT4",
        "BIGINT",
        "INT8",
        "REAL",
        "FLOAT",
        "DOUBLE",
        "DOUBLE PRECISION",
        "DECIMAL",
        "NUMERIC",
        "VARCHAR",
        "CHAR",
        "CHARACTER",
        "CHARACTER VARYING",
        "TEXT",
        "BLOB",
        "BYTEA",
        "DATE",
        "TIME",
        "TIMESTAMP",
        "TIMESTAMPTZ",
        "UUID",
    ]
    .contains(&base)
    {
        return Err(EngineError::unsupported(
            "only built-in scalar column types are enabled",
        ));
    }
    Ok(())
}
