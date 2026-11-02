use crate::{Analyzer, IndexIntent};
use fedengine_core::*;
use sqlparser::ast::{ObjectName, ObjectType, Statement};
impl IndexIntent {
    /// Generate only the limited remote DDL template; never concatenate original client SQL.
    pub fn duckdb_adapter_sql(&self, kind: DataSourceKind) -> Result<String> {
        let q = |s: &str| {
            if kind == DataSourceKind::Mysql {
                format!("`{}`", s.replace('`', "``"))
            } else {
                quote_identifier(s)
            }
        };
        let table = format!("{}.{}", q(&self.table.schema), q(&self.table.name));
        let remote = if self.create {
            if self.if_exists && kind == DataSourceKind::Mysql {
                return Err(EngineError::unsupported(
                    "MySQL index IF NOT EXISTS is not verified",
                ));
            }
            format!(
                "CREATE {}INDEX {}{} ON {} ({})",
                if self.unique { "UNIQUE " } else { "" },
                if self.if_exists { "IF NOT EXISTS " } else { "" },
                q(&self.name),
                table,
                self.columns
                    .iter()
                    .map(|c| q(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if kind == DataSourceKind::Mysql {
            format!("DROP INDEX {} ON {}", q(&self.name), table)
        } else {
            format!("DROP INDEX {}.{}", q(&self.table.schema), q(&self.name))
        };
        Ok(format!(
            "CALL {}_execute({}, {})",
            kind.extension(),
            quote_literal(&self.table.catalog),
            quote_literal(&remote)
        ))
    }
}

pub fn drop_index_name(sql: &str) -> Result<Option<ObjectName>> {
    Ok(match Analyzer::parse(sql)?.first() {
        Some(Statement::Drop {
            object_type: ObjectType::Index,
            names,
            ..
        }) if names.len() == 1 => Some(names[0].clone()),
        _ => None,
    })
}
