//! Execution-side PG arithmetic. Applied only after user SQL has passed policy/binding.
use fedengine_core::{EngineError, Result};
use sqlparser::{ast::*, dialect::DuckDbDialect, parser::Parser};
use std::ops::ControlFlow;

/// Connection settings are not inherited by DuckDB clones. Keep all admitted sessions in UTC.
/// The private macro uses bound operand types: integer // truncates, floating // divides.
/// DuckDB decimal division loses precision, so reject it rather than return an inexact numeric.
pub const EXECUTION_SETTINGS: &str = "SET TimeZone='UTC';
CREATE OR REPLACE TEMP MACRO __fed_pg_divide(a,b) AS
CASE WHEN b=0 AND a IS NOT NULL THEN error('__fed_pg_division_by_zero')
WHEN (typeof(a) LIKE 'DECIMAL%' OR typeof(b) LIKE 'DECIMAL%'
OR typeof(a) IN ('HUGEINT','UHUGEINT','UBIGINT') OR typeof(b) IN ('HUGEINT','UHUGEINT','UBIGINT'))
AND typeof(a) NOT IN ('FLOAT','DOUBLE') AND typeof(b) NOT IN ('FLOAT','DOUBLE')
THEN error('__fed_pg_numeric_division') ELSE a // b END";

/// Rewrite division nodes, including nested queries, without touching literals or comments.
/// The private function is injected after authorization; clients cannot invoke it directly.
pub fn execution_sql(sql: &str) -> Result<String> {
    let mut statements = Parser::parse_sql(&DuckDbDialect {}, sql)
        .map_err(|_| EngineError::new("42601", "execution SQL syntax is unsupported"))?;
    if statements.len() != 1 {
        return Err(EngineError::unsupported(
            "one execution statement is required",
        ));
    }
    struct Arithmetic;
    impl VisitorMut for Arithmetic {
        type Break = EngineError;
        fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<Self::Break> {
            for item in &mut select.projection {
                if let SelectItem::UnnamedExpr(Expr::Function(f)) = item
                    && f.name.to_string().eq_ignore_ascii_case("decode")
                {
                    *item = SelectItem::ExprWithAlias {
                        expr: Expr::Function(f.clone()),
                        alias: Ident::with_quote('"', "decode"),
                    };
                }
                if let SelectItem::UnnamedExpr(expr) = item
                    && matches!(
                        expr,
                        Expr::BinaryOp { .. } | Expr::Nested(_) | Expr::Cast { .. }
                    )
                {
                    let division = visit_expressions(expr, |e| {
                        if matches!(
                            e,
                            Expr::BinaryOp {
                                op: BinaryOperator::Divide,
                                ..
                            }
                        ) {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    });
                    if division.is_break() {
                        *item = SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::with_quote('"', "?column?"),
                        };
                    }
                }
            }
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
            if let Expr::Function(f) = expr
                && f.name.to_string().eq_ignore_ascii_case("decode")
            {
                f.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(
                    "__fed_pg_decode",
                ))]);
            }
            if let Expr::BinaryOp {
                left,
                op: BinaryOperator::Divide,
                right,
            } = expr
            {
                // Parse a fixed function shape, then move in the original AST operands.
                let mut template =
                    Parser::parse_sql(&DuckDbDialect {}, "SELECT __fed_pg_divide(0,0)")
                        .expect("fixed arithmetic template");
                let Statement::Query(q) = template.remove(0) else {
                    unreachable!()
                };
                let SetExpr::Select(s) = *q.body else {
                    unreachable!()
                };
                let SelectItem::UnnamedExpr(Expr::Function(mut f)) =
                    s.projection.into_iter().next().unwrap()
                else {
                    unreachable!()
                };
                let FunctionArguments::List(args) = &mut f.args else {
                    unreachable!()
                };
                // DuckDB's positional macro overload binding rejects explicit casts
                // after remote catalogs are attached. Named arguments retain the
                // operand types without that overload-inference path.
                args.args = [("a", left), ("b", right)]
                    .into_iter()
                    .map(|(name, operand)| FunctionArg::Named {
                        name: Ident::new(name),
                        arg: FunctionArgExpr::Expr(*operand.clone()),
                        operator: FunctionArgOperator::Assignment,
                    })
                    .collect();
                *expr = Expr::Function(f);
            }
            ControlFlow::Continue(())
        }
    }
    if let ControlFlow::Break(error) = VisitMut::visit(&mut statements[0], &mut Arithmetic) {
        return Err(error);
    }
    Ok(statements.remove(0).to_string())
}
