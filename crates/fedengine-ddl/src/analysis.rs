use crate::{
    binding::{Binder, bind_constraints, object_name, validate_scalar_type},
    model::*,
};
use fedengine_core::*;
use sqlparser::{ast::*, dialect::DuckDbDialect, parser::Parser};
use std::{collections::BTreeSet, ops::ControlFlow};
#[derive(Debug, Clone)]
pub struct Analyzer {
    pub sources: Vec<DataSourceDefinition>,
    pub indexes: Vec<IndexOwner>,
}
impl Analyzer {
    pub fn parse(sql: &str) -> Result<Vec<Statement>> {
        if sql.len() > 1024 * 1024 {
            return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
        }
        Parser::parse_sql(&DuckDbDialect {}, sql)
            .map_err(|_| EngineError::new("42601", "SQL syntax is outside the supported grammar"))
    }
    pub fn split(sql: &str) -> Result<Vec<String>> {
        let statements = Self::parse(sql)?;
        if statements.len() > 64 {
            return Err(EngineError::new("54000", "too many statements"));
        }
        if statements.len() > 1 && statements.iter().any(|s| !matches!(s, Statement::Query(_))) {
            return Err(EngineError::unsupported(
                "multi-statement messages containing writes, DDL or transaction commands are disabled",
            ));
        }
        Ok(statements.iter().map(ToString::to_string).collect())
    }
    pub fn analyze(&self, sql: &str) -> Result<Analysis> {
        let mut parsed = Self::parse(sql)?;
        if parsed.len() != 1 {
            return Err(EngineError::unsupported(
                "exactly one statement is required",
            ));
        }
        let mut statement = parsed.remove(0);
        let (kind, action, command) = classify(&statement)?;
        let mut binder = Binder::new(self);
        let mut primary = None;
        let mut index = None;
        match &mut statement {
            Statement::CreateSchema {
                schema_name,
                with,
                options,
                default_collate_spec,
                clone,
                ..
            } => {
                if with.is_some()
                    || options.is_some()
                    || default_collate_spec.is_some()
                    || clone.is_some()
                {
                    return Err(EngineError::unsupported("schema options are disabled"));
                }
                let SchemaName::Simple(name) = schema_name else {
                    return Err(EngineError::unsupported(
                        "schema authorization syntax is disabled",
                    ));
                };
                let object = self.bind(name, true)?;
                *name = object_name(&[&object.catalog, &object.schema]);
                binder.objects.insert(object.clone());
                primary = Some(object);
            }
            Statement::Drop {
                object_type,
                names,
                cascade,
                purge,
                temporary,
                table,
                ..
            } => {
                if *cascade || *purge || *temporary || names.len() != 1 {
                    return Err(EngineError::unsupported(
                        "CASCADE, temporary and multiple-object DROP are disabled",
                    ));
                }
                let object = self.bind(&names[0], *object_type == ObjectType::Schema)?;
                if *object_type == ObjectType::Index {
                    let candidates: Vec<_> =
                        self.indexes.iter().filter(|i| i.index == object).collect();
                    if candidates.len() != 1 {
                        return Err(EngineError::new(
                            "42704",
                            "index table ownership is unknown or ambiguous",
                        ));
                    }
                    let owner = candidates[0];
                    if owner.protected {
                        return Err(EngineError::denied(
                            "constraint/protected indexes cannot be dropped",
                        ));
                    }
                    if let Some(t) = table
                        && self.bind(t, false)? != owner.table
                    {
                        return Err(EngineError::denied("index and supplied table disagree"));
                    }
                    binder.objects.insert(owner.table.clone());
                    index = Some(IndexIntent {
                        table: owner.table.clone(),
                        name: object.name.clone(),
                        columns: vec![],
                        unique: false,
                        create: false,
                        if_exists: false,
                    });
                    primary = Some(owner.table.clone());
                } else {
                    primary = Some(object.clone());
                }
                names[0] = if *object_type == ObjectType::Schema {
                    object_name(&[&object.catalog, &object.schema])
                } else {
                    object_name(&[&object.catalog, &object.schema, &object.name])
                };
                binder.objects.insert(object);
            }
            Statement::CreateIndex(i) => {
                if i.concurrently
                    || !i.include.is_empty()
                    || i.nulls_distinct.is_some()
                    || !i.with.is_empty()
                    || i.predicate.is_some()
                    || !i.index_options.is_empty()
                    || !i.alter_options.is_empty()
                    || i.using
                        .as_ref()
                        .is_some_and(|v| v.to_string().to_lowercase() != "btree")
                {
                    return Err(EngineError::unsupported(
                        "only ordinary B-tree column indexes are allowed",
                    ));
                }
                let table = self.bind(&i.table_name, false)?;
                let name = i
                    .name
                    .as_ref()
                    .ok_or_else(|| EngineError::unsupported("index name is required"))?;
                let p = Self::parts(name)?;
                let index_name = match p.as_slice() {
                    [n] => n.clone(),
                    _ => {
                        let idx = self.bind(name, false)?;
                        binder.objects.insert(idx.clone());
                        if idx.schema != table.schema {
                            return Err(EngineError::unsupported(
                                "index must share table namespace",
                            ));
                        }
                        idx.name
                    }
                };
                let mut columns = Vec::new();
                for c in &i.columns {
                    let Expr::Identifier(id) = &c.column.expr else {
                        return Err(EngineError::unsupported("expression indexes are disabled"));
                    };
                    if c.operator_class.is_some()
                        || c.column.options.asc.is_some()
                        || c.column.options.nulls_first.is_some()
                    {
                        return Err(EngineError::unsupported(
                            "index sort/operator options are disabled",
                        ));
                    }
                    columns.push(id.value.clone());
                }
                if columns.is_empty() {
                    return Err(EngineError::unsupported("index requires columns"));
                }
                index = Some(IndexIntent {
                    table: table.clone(),
                    name: index_name,
                    columns,
                    unique: i.unique,
                    create: true,
                    if_exists: i.if_not_exists,
                });
                primary = Some(table);
            }
            Statement::CreateTable(t) => {
                // Reconstruct the permitted form; comparing ASTs catches every non-whitelisted option.
                let base = if t.query.is_some() {
                    "CREATE TABLE x AS SELECT 1"
                } else {
                    "CREATE TABLE x (id INTEGER)"
                };
                let Statement::CreateTable(mut permitted) = Self::parse(base)?.remove(0) else {
                    unreachable!()
                };
                permitted.name = t.name.clone();
                permitted.columns = t.columns.clone();
                permitted.constraints = t.constraints.clone();
                permitted.query = t.query.clone();
                permitted.if_not_exists = t.if_not_exists;
                if *t != permitted {
                    return Err(EngineError::unsupported(
                        "CREATE TABLE options are outside the whitelist",
                    ));
                }
                primary = Some(self.bind(&t.name, false)?);
                bind_constraints(&mut binder, &mut t.columns, &mut t.constraints)?;
            }
            Statement::AlterTable(t) => {
                primary = Some(self.bind(&t.name, false)?);
                if t.operations.len() != 1 || t.table_type.is_some() || t.location.is_some() {
                    return Err(EngineError::unsupported(
                        "one ordinary ALTER TABLE operation is required",
                    ));
                }
                match &mut t.operations[0] {
                    AlterTableOperation::AddColumn {
                        column_def,
                        column_position,
                        ..
                    } => {
                        if column_position.is_some() {
                            return Err(EngineError::unsupported("column position is disabled"));
                        }
                        bind_constraints(&mut binder, std::slice::from_mut(column_def), &mut [])?;
                    }
                    AlterTableOperation::DropColumn {
                        drop_behavior: None | Some(DropBehavior::Restrict),
                        ..
                    }
                    | AlterTableOperation::RenameColumn { .. } => {}
                    AlterTableOperation::AlterColumn { op, .. } => match op {
                        AlterColumnOperation::SetNotNull
                        | AlterColumnOperation::DropNotNull
                        | AlterColumnOperation::SetDefault { .. }
                        | AlterColumnOperation::DropDefault => {}
                        AlterColumnOperation::SetDataType {
                            data_type,
                            using: None,
                            ..
                        } => validate_scalar_type(data_type)?,
                        _ => {
                            return Err(EngineError::unsupported(
                                "ALTER COLUMN operation is not enabled",
                            ));
                        }
                    },
                    // Renames and schema migrations need explicit destination binding, currently fail closed.
                    _ => return Err(EngineError::unsupported("ALTER operation is not enabled")),
                }
            }
            Statement::Insert(i) => {
                if i.returning.is_some()
                    || i.overwrite
                    || i.replace_into
                    || i.or.is_some()
                    || i.ignore
                    || i.partitioned.is_some()
                    || i.on.is_some()
                    || i.output.is_some()
                    || i.settings.is_some()
                    || i.format_clause.is_some()
                    || i.multi_table_insert_type.is_some()
                    || !i.multi_table_into_clauses.is_empty()
                {
                    return Err(EngineError::unsupported("INSERT variant is not enabled"));
                }
            }
            Statement::Update(u) => {
                if u.returning.is_some()
                    || !u.table.joins.is_empty()
                    || u.output.is_some()
                    || u.or.is_some()
                {
                    return Err(EngineError::unsupported("UPDATE variant is not enabled"));
                }
            }
            Statement::Delete(d) => {
                if d.returning.is_some() || !d.tables.is_empty() || d.output.is_some() {
                    return Err(EngineError::unsupported("DELETE variant is not enabled"));
                }
            }
            _ => {}
        }
        if let ControlFlow::Break(e) = VisitMut::visit(&mut statement, &mut binder) {
            return Err(e);
        }
        let sources: BTreeSet<_> = binder.objects.iter().map(|o| o.source.clone()).collect();
        if kind != Kind::Query && sources.len() != 1 {
            return Err(EngineError::denied(
                "write/DDL references must resolve to exactly one registered DataSourceId",
            ));
        }
        let capability = match &statement {
            Statement::AlterTable(t) => match &t.operations[0] {
                AlterTableOperation::AddColumn { .. } => "ALTER_TABLE_ADD_COLUMN",
                AlterTableOperation::DropColumn { .. } => "ALTER_TABLE_DROP_COLUMN",
                AlterTableOperation::RenameColumn { .. } => "ALTER_TABLE_RENAME_COLUMN",
                AlterTableOperation::AlterColumn { op, .. } => match op {
                    AlterColumnOperation::SetDataType { .. } => "ALTER_TABLE_COLUMN_TYPE",
                    AlterColumnOperation::SetDefault { .. } | AlterColumnOperation::DropDefault => {
                        "ALTER_TABLE_COLUMN_DEFAULT"
                    }
                    _ => "ALTER_TABLE_COLUMN_NULLABILITY",
                },
                _ => unreachable!(),
            },
            Statement::CreateIndex(i) if i.unique => "CREATE_INDEX_UNIQUE_BTREE",
            Statement::CreateIndex(_) => "CREATE_INDEX_BTREE",
            _ => action.as_str(),
        }
        .to_string();
        Ok(Analysis {
            capability,
            sql: statement.to_string(),
            kind,
            action,
            command,
            objects: binder.objects,
            sources,
            primary,
            index,
        })
    }
}
pub(super) fn classify(s: &Statement) -> Result<(Kind, String, String)> {
    let (k, a, c) = match s {
        Statement::Query(_) => (Kind::Query, "SELECT", "SELECT"),
        Statement::Insert(_) => (Kind::Insert, "INSERT", "INSERT"),
        Statement::Update(_) => (Kind::Update, "UPDATE", "UPDATE"),
        Statement::Delete(_) => (Kind::Delete, "DELETE", "DELETE"),
        Statement::CreateTable(t) => (
            Kind::Ddl,
            if t.query.is_some() {
                "CREATE_TABLE_AS"
            } else {
                "CREATE_TABLE"
            },
            "CREATE TABLE",
        ),
        Statement::CreateSchema { .. } => (Kind::Ddl, "CREATE_SCHEMA", "CREATE SCHEMA"),
        Statement::CreateIndex(_) => (Kind::Ddl, "CREATE_INDEX", "CREATE INDEX"),
        Statement::AlterTable(_) => (Kind::Ddl, "ALTER_TABLE", "ALTER TABLE"),
        Statement::Drop { object_type, .. } => match object_type {
            ObjectType::Schema => (Kind::Ddl, "DROP_SCHEMA", "DROP SCHEMA"),
            ObjectType::Table => (Kind::Ddl, "DROP_TABLE", "DROP TABLE"),
            ObjectType::Index => (Kind::Ddl, "DROP_INDEX", "DROP INDEX"),
            _ => return Err(EngineError::unsupported("object type is not enabled")),
        },
        _ => {
            return Err(EngineError::unsupported(
                "statement class is not enabled; explicit transactions and engine administration are disabled",
            ));
        }
    };
    Ok((k, a.into(), c.into()))
}
