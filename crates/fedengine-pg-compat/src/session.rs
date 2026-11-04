//! One session setting registry for PostgreSQL probes and state-changing commands.
use fedengine_contract::*;
use fedengine_core::*;
use sqlparser::{ast::*, dialect::PostgreSqlDialect, parser::Parser};

pub const SERVER_VERSION: &str = "16.0 (FedEngine 0.1; PostgreSQL protocol subset)";
pub fn parse_pg(sql: &str) -> Result<Vec<Statement>> {
    if sql.len() > 1024 * 1024 {
        return Err(EngineError::new("54000", "SQL exceeds 1 MiB limit"));
    }
    Parser::parse_sql(&PostgreSqlDialect {}, sql).map_err(|_| {
        EngineError::new(
            "42601",
            "SQL syntax is outside the supported PostgreSQL grammar",
        )
    })
}
fn default_path(session: &SessionContext, scope: &MetadataScope) -> Vec<SessionIdentifier> {
    if scope.virtual_database.is_some() {
        return vec![
            SessionIdentifier {
                name: "$user".into(),
                quoted: true,
            },
            SessionIdentifier {
                name: "public".into(),
                quoted: false,
            },
        ];
    }
    scope
        .catalogs
        .iter()
        .find(|c| c.catalog == session.database)
        .filter(|c| !c.default_schema.is_empty())
        .map(|c| {
            vec![SessionIdentifier {
                name: c.default_schema.clone(),
                quoted: true,
            }]
        })
        .unwrap_or_default()
}
pub fn effective_path(session: &SessionContext, scope: &MetadataScope) -> Vec<String> {
    let entries = session
        .search_path
        .clone()
        .unwrap_or_else(|| default_path(session, scope));
    let mut schemas = scope
        .catalogs
        .iter()
        .filter(|c| c.catalog == session.database)
        .flat_map(|c| c.schemas.clone())
        .collect::<Vec<_>>();
    schemas.extend(["pg_catalog".into(), "information_schema".into()]);
    if scope.virtual_database.is_some() {
        schemas.push("public".into());
    }
    let mut result = vec![];
    for entry in entries {
        let name = if entry.name == "$user" {
            scope.principal.clone()
        } else if entry.quoted {
            entry.name
        } else {
            entry.name.to_lowercase()
        };
        if schemas.contains(&name) && !result.contains(&name) {
            result.push(name);
        }
    }
    result
}
pub fn setting(session: &SessionContext, scope: &MetadataScope, name: &str) -> Result<String> {
    let name = name.to_ascii_lowercase();
    if name == "search_path" {
        let names = session
            .search_path
            .clone()
            .unwrap_or_else(|| default_path(session, scope));
        return Ok(names
            .into_iter()
            .map(|n| {
                if n.quoted {
                    quote_identifier(&n.name)
                } else {
                    n.name
                }
            })
            .collect::<Vec<_>>()
            .join(", "));
    }
    if let Some(v) = session.settings.get(&name) {
        return Ok(v.clone());
    }
    Ok(match name.as_str() {
        "server_version" => SERVER_VERSION,
        "server_version_num" => "160000",
        "server_encoding" | "client_encoding" => "UTF8",
        "standard_conforming_strings" | "integer_datetimes" => "on",
        "timezone" => "UTC",
        "datestyle" => "ISO, YMD",
        "application_name" => "",
        "extra_float_digits" => "3",
        "client_min_messages" => "notice",
        _ => {
            return Err(EngineError::new(
                "42704",
                "unrecognized or unsupported configuration parameter",
            ));
        }
    }
    .into())
}
fn expr_string(expr: &Expr) -> Result<String> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) | Value::Number(s, _) => {
                Ok(s.clone())
            }
            Value::Boolean(b) => Ok(if *b { "on" } else { "off" }.into()),
            _ => Err(EngineError::unsupported("setting requires a literal value")),
        },
        Expr::Identifier(i) => Ok(i.value.clone()),
        Expr::UnaryOp { op, expr } if *op == UnaryOperator::Minus => {
            Ok(format!("-{}", expr_string(expr)?))
        }
        _ => Err(EngineError::unsupported("setting requires a literal value")),
    }
}
fn set(session: &mut SessionContext, name: &str, values: &[Expr]) -> Result<()> {
    let name = name.to_ascii_lowercase();
    if name == "search_path" {
        let entries = values
            .iter()
            .map(|v| match v {
                Expr::Identifier(i) => Ok(SessionIdentifier {
                    name: i.value.clone(),
                    quoted: i.quote_style.is_some(),
                }),
                _ => Ok(SessionIdentifier {
                    name: expr_string(v)?,
                    quoted: false,
                }),
            })
            .collect::<Result<Vec<_>>>()?;
        if entries.len() > 64 {
            return Err(EngineError::new("54000", "search_path exceeds 64 entries"));
        }
        // Quoted setting strings may contain an SQL identifier list; parse it as expressions.
        let entries = if values.len() == 1 && matches!(&values[0], Expr::Value(_)) {
            let text = expr_string(&values[0])?;
            if text.is_empty() {
                vec![]
            } else {
                let parsed = parse_pg(&format!("SELECT {text}"))?;
                let Statement::Query(q) = &parsed[0] else {
                    return Err(EngineError::new("42601", "invalid search_path"));
                };
                let SetExpr::Select(select) = q.body.as_ref() else {
                    return Err(EngineError::new("42601", "invalid search_path"));
                };
                select
                    .projection
                    .iter()
                    .map(|p| {
                        if let SelectItem::UnnamedExpr(Expr::Identifier(i)) = p {
                            Ok(SessionIdentifier {
                                name: i.value.clone(),
                                quoted: i.quote_style.is_some(),
                            })
                        } else {
                            Err(EngineError::new("42601", "invalid search_path identifier"))
                        }
                    })
                    .collect::<Result<_>>()?
            }
        } else {
            entries
        };
        session.search_path = Some(entries);
        session.namespace_version = session
            .namespace_version
            .checked_add(1)
            .ok_or_else(|| EngineError::new("54000", "session namespace version exhausted"))?;
        return Ok(());
    }
    if values.len() != 1 {
        return Err(EngineError::unsupported("setting requires one value"));
    }
    let value = expr_string(&values[0])?;
    let normalized = value.to_ascii_lowercase();
    let valid = match name.as_str() {
        "application_name" => value.len() <= 256,
        "client_encoding" => matches!(normalized.as_str(), "utf8" | "utf-8"),
        "timezone" => normalized == "utc",
        "datestyle" => matches!(normalized.as_str(), "iso" | "iso, ymd" | "iso, mdy"),
        "standard_conforming_strings" => matches!(normalized.as_str(), "on" | "true"),
        "extra_float_digits" => value == "3",
        "client_min_messages" => normalized == "notice",
        _ => {
            return Err(EngineError::new(
                "42704",
                "unrecognized or unsupported configuration parameter",
            ));
        }
    };
    if !valid {
        return Err(EngineError::unsupported(
            "setting value does not match implemented behavior",
        ));
    }
    let value = match name.as_str() {
        "client_encoding" => "UTF8".into(),
        "timezone" => "UTC".into(),
        "standard_conforming_strings" => "on".into(),
        _ => value,
    };
    session.settings.insert(name, value);
    Ok(())
}
/// Parse validates against a copy; only Execute commits the returned session state.
pub fn session_command(
    sql: &str,
    session: &SessionContext,
) -> Result<Option<(SessionContext, String)>> {
    let mut statements = parse_pg(sql)?;
    if statements.len() != 1 {
        return Err(EngineError::unsupported("one statement is required"));
    }
    let mut next = session.clone();
    match statements.remove(0) {
        Statement::Set(Set::SingleAssignment {
            scope,
            hivevar,
            variable,
            values,
        }) => {
            if hivevar || scope == Some(ContextModifier::Local) {
                return Err(EngineError::unsupported(
                    "SET LOCAL and dialect-specific settings are unsupported",
                ));
            }
            let name = variable.to_string();
            if values.len() == 1
                && matches!(&values[0],Expr::Identifier(i) if i.value.eq_ignore_ascii_case("default"))
            {
                reset(&mut next, &name)?;
            } else {
                set(&mut next, &name, &values)?;
            }
            Ok(Some((next, "SET".into())))
        }
        Statement::Set(Set::SetTimeZone {
            local: false,
            value,
        }) => {
            set(&mut next, "timezone", &[value])?;
            Ok(Some((next, "SET".into())))
        }
        Statement::Reset(r) => {
            match r.reset {
                Reset::ALL => {
                    next.settings.clear();
                    next.search_path = None;
                    next.namespace_version += 1;
                }
                Reset::ConfigurationParameter(n) => reset(&mut next, &n.to_string())?,
            }
            Ok(Some((next, "RESET".into())))
        }
        Statement::Set(_) => Err(EngineError::unsupported(
            "session command is outside the supported registry",
        )),
        _ => Ok(None),
    }
}
fn reset(session: &mut SessionContext, name: &str) -> Result<()> {
    let name = name.to_ascii_lowercase();
    if name == "search_path" {
        session.search_path = None;
        session.namespace_version += 1;
    } else {
        let scope = MetadataScope {
            virtual_database: None,
            databases: vec![],
            database: session.database.clone(),
            principal: String::new(),
            catalogs: vec![],
        };
        setting(&SessionContext::default(), &scope, &name)?;
        session.settings.remove(&name);
    }
    Ok(())
}
