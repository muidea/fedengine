use crate::Analyzer;
use fedengine_core::*;
use sqlparser::ast::*;
use std::ops::ControlFlow;

/// Lower the parameters actually referenced by an execution plan to dense slots.
/// The returned zero-based indices address the original client parameter vector.
/// Client parameter discovery and validation must happen before this lowering.
pub fn compact_parameters(sql: &str) -> Result<(String, Vec<usize>)> {
    let mut statements = Analyzer::parse(sql)?;
    if statements.len() != 1 {
        return Err(EngineError::unsupported("one SQL statement is required"));
    }
    // Validate the placeholder grammar and bound even when a caller has no types.
    parameter_hints(sql)?;
    let mut slots = std::collections::BTreeSet::new();
    let _ = visit_expressions(&statements, |e| {
        if let Expr::Value(v) = e
            && let Value::Placeholder(p) = &v.value
        {
            slots.insert(p[1..].parse::<usize>().expect("validated parameter") - 1);
        }
        ControlFlow::<()>::Continue(())
    });
    let slots = slots.into_iter().collect::<Vec<_>>();
    let _ = visit_expressions_mut(&mut statements, |e| {
        if let Expr::Value(v) = e
            && let Value::Placeholder(p) = &mut v.value
        {
            let original = p[1..].parse::<usize>().expect("validated parameter") - 1;
            *p = format!("${}", slots.binary_search(&original).unwrap() + 1);
        }
        ControlFlow::<()>::Continue(())
    });
    Ok((statements.remove(0).to_string(), slots))
}
/// Infer only explicit casts. Context-sensitive inference is refused rather than guessing an OID.
pub fn parameter_hints(sql: &str) -> Result<Vec<Option<String>>> {
    struct Hints {
        types: Vec<Option<String>>,
    }
    impl Visitor for Hints {
        type Break = EngineError;
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Value(v) = e
                && let Value::Placeholder(p) = &v.value
            {
                let Some(n) = p
                    .strip_prefix('$')
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n > 0 && *n <= 1024)
                else {
                    return ControlFlow::Break(EngineError::unsupported(
                        "only numeric $1..$1024 parameters are supported",
                    ));
                };
                self.types.resize(self.types.len().max(n), None);
            }
            if let Expr::Cast {
                expr, data_type, ..
            } = e
                && let Expr::Value(v) = expr.as_ref()
                && let Value::Placeholder(p) = &v.value
                && let Some(n) = p
                    .strip_prefix('$')
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n > 0 && *n <= 1024)
            {
                self.types.resize(self.types.len().max(n), None);
                self.types[n - 1].get_or_insert_with(|| data_type.to_string());
            }
            ControlFlow::Continue(())
        }
    }
    let mut hints = Hints { types: Vec::new() };
    for s in Analyzer::parse(sql)? {
        if let ControlFlow::Break(e) = Visit::visit(&s, &mut hints) {
            return Err(e);
        }
    }
    Ok(hints.types)
}

/// Add parameter casts with the SQL AST; strings/comments are never rewritten.
pub fn cast_parameters(sql: &str, types: &[String]) -> Result<String> {
    struct Casts {
        types: Vec<Option<DataType>>,
    }
    impl VisitorMut for Casts {
        type Break = EngineError;
        fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<Self::Break> {
            if let Expr::Value(v) = e
                && let Value::Placeholder(p) = &v.value
                && let Some(n) = p.strip_prefix('$').and_then(|v| v.parse::<usize>().ok())
                && let Some(Some(t)) = n.checked_sub(1).and_then(|i| self.types.get(i))
            {
                let template = format!("SELECT CAST({p} AS {t})");
                if let Ok(mut statements) = Analyzer::parse(&template)
                    && let Statement::Query(q) = statements.remove(0)
                    && let SetExpr::Select(s) = *q.body
                    && let SelectItem::UnnamedExpr(cast) = s.projection.into_iter().next().unwrap()
                {
                    *e = cast;
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut parsed = Analyzer::parse(sql)?;
    let mut casts = Casts { types: Vec::new() };
    for t in types {
        if t == "SKIP" {
            casts.types.push(None);
            continue;
        }
        let mut p = Analyzer::parse(&format!("SELECT CAST(1 AS {t})"))?;
        let Statement::Query(q) = p.remove(0) else {
            unreachable!()
        };
        let SetExpr::Select(s) = *q.body else {
            unreachable!()
        };
        let SelectItem::UnnamedExpr(Expr::Cast { data_type, .. }) =
            s.projection.into_iter().next().unwrap()
        else {
            unreachable!()
        };
        casts.types.push(Some(data_type));
    }
    for s in &mut parsed {
        if let ControlFlow::Break(e) = VisitMut::visit(s, &mut casts) {
            return Err(e);
        }
    }
    Ok(parsed[0].to_string())
}
