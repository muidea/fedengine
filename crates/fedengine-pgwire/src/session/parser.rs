use super::Backend;
use fedengine_contract::*;
use fedengine_core::EngineError;
use fedengine_ddl::Analyzer;
use fedengine_pg_compat::{fields, pg_error};
use modorch::prelude::async_trait;
use pgwire::{
    api::{portal::Format, results::FieldInfo, stmt::QueryParser, *},
    error::PgWireResult,
};
#[async_trait]
impl QueryParser for Backend {
    type Statement = PreparedDescriptor;
    async fn parse_sql<C>(
        &self,
        client: &C,
        sql: &str,
        types: &[Option<Type>],
    ) -> PgWireResult<Option<PreparedDescriptor>>
    where
        C: ClientInfo + Unpin + Send + std::marker::Sync,
    {
        if Analyzer::parse(sql).map_err(pg_error)?.is_empty() {
            return Ok(None);
        }
        let types = types
            .iter()
            .map(|t| match t {
                None => Ok(None),
                Some(t) => PgType::from_oid(t.oid()).map(Some).ok_or_else(|| {
                    pg_error(EngineError::unsupported("parameter OID is not supported"))
                }),
            })
            .collect::<PgWireResult<Vec<_>>>()?;
        let reply = self
            .call(
                client,
                sql.into(),
                QueryMode::Prepare {
                    parameter_types: types,
                },
            )
            .await?;
        match reply.as_ref() {
            Ok(QueryReply::Prepared(d)) => Ok(Some(d.clone())),
            _ => Err(pg_error(EngineError::new("XX000", "invalid prepare reply"))),
        }
    }
    fn get_parameter_types(&self, s: &PreparedDescriptor) -> PgWireResult<Vec<Type>> {
        Ok(s.parameters
            .iter()
            .map(|p| Type::from_oid(p.oid()).unwrap())
            .collect())
    }
    fn get_result_schema(
        &self,
        s: &PreparedDescriptor,
        format: Option<&Format>,
    ) -> PgWireResult<Vec<FieldInfo>> {
        Ok(fields(&s.columns, format.unwrap_or(&Format::UnifiedText)))
    }
}
