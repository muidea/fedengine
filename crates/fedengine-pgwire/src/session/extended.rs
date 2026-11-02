use super::{Backend, send};
use fedengine_contract::*;
use fedengine_core::EngineError;
use fedengine_pg_compat::{decode_parameter, encode_row, fields, pg_error};
use futures::{Sink, SinkExt};
use modorch::prelude::async_trait;
use pgwire::{
    api::{
        portal::{Format, Portal},
        query::ExtendedQueryHandler,
        results::*,
        stmt::{QueryParser, StoredStatement},
        store::{Entry, PortalStore},
        *,
    },
    error::{PgWireError, PgWireResult},
    messages::{
        extendedquery::*,
        response::{EmptyQueryResponse, ReadyForQuery, TransactionStatus},
        *,
    },
};
use std::{fmt::Debug, sync::Arc};
#[async_trait]
impl ExtendedQueryHandler for Backend {
    type Statement = PreparedDescriptor;
    type QueryParser = Backend;
    fn query_parser(&self) -> Arc<Self::QueryParser> {
        unreachable!("custom Parse and Describe handlers own this boundary")
    }
    async fn on_parse<C>(&self, client: &mut C, message: Parse) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name.as_deref().unwrap_or(DEFAULT_NAME).to_string();
        {
            let names = self.names.lock().unwrap();
            if name != DEFAULT_NAME && names.statements.contains(&name) {
                return Err(pg_error(EngineError::new(
                    "42P05",
                    "prepared statement already exists",
                )));
            }
            if !names.statements.contains(&name)
                && names.statements.len() >= self.config.pgwire.max_statements
            {
                return Err(pg_error(EngineError::new(
                    "54000",
                    "prepared statement limit reached",
                )));
            }
        }
        let types = message
            .type_oids
            .iter()
            .map(|oid| {
                if *oid == 0 {
                    Ok(None)
                } else {
                    Type::from_oid(*oid)
                        .map(Some)
                        .ok_or_else(|| pg_error(EngineError::unsupported("unknown parameter OID")))
                }
            })
            .collect::<PgWireResult<Vec<_>>>()?;
        if let Some(stmt) = self.parse_sql(client, &message.query, &types).await? {
            client
                .portal_store()
                .put_statement(Arc::new(StoredStatement::new(name.clone(), stmt, types)));
        } else {
            client.portal_store().put_empty_statement(&name);
        }
        self.names.lock().unwrap().statements.insert(name);
        client
            .send(PgWireBackendMessage::ParseComplete(ParseComplete::new()))
            .await?;
        Ok(())
    }
    async fn on_bind<C>(&self, client: &mut C, message: Bind) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message
            .portal_name
            .as_deref()
            .unwrap_or(DEFAULT_NAME)
            .to_string();
        let statement = message.statement_name.as_deref().unwrap_or(DEFAULT_NAME);
        {
            let names = self.names.lock().unwrap();
            if name != DEFAULT_NAME && names.portals.contains(&name) {
                return Err(pg_error(EngineError::new("42P03", "portal already exists")));
            }
            if !names.portals.contains(&name)
                && names.portals.len() >= self.config.pgwire.max_portals
            {
                return Err(pg_error(EngineError::new("54000", "portal limit reached")));
            }
        }
        match client.portal_store().get_statement(statement) {
            Some(Entry::Value(stmt)) => {
                let d = &stmt.statement;
                if message.parameters.len() != d.parameters.len() {
                    return Err(pg_error(EngineError::new(
                        "08P01",
                        "Bind parameter count mismatch",
                    )));
                }
                validate_formats(&message.parameter_format_codes, d.parameters.len())?;
                validate_formats(&message.result_column_format_codes, d.columns.len())?;
                let portal = Portal::try_new(&message, stmt)?;
                for (i, t) in portal.statement.statement.parameters.iter().enumerate() {
                    decode_parameter(
                        portal.parameters[i].as_deref(),
                        *t,
                        portal.parameter_format.is_binary(i),
                    )
                    .map_err(pg_error)?;
                }
                client.portal_store().put_portal(Arc::new(portal));
            }
            Some(Entry::Empty) => {
                if !message.parameters.is_empty() {
                    return Err(pg_error(EngineError::new(
                        "08P01",
                        "empty statement cannot bind parameters",
                    )));
                }
                client.portal_store().put_empty_portal(&name);
            }
            None => return Err(PgWireError::StatementNotFound(statement.into())),
        }
        self.cursors.lock().await.remove(&name);
        self.names.lock().unwrap().portals.insert(name);
        client
            .send(PgWireBackendMessage::BindComplete(BindComplete::new()))
            .await?;
        Ok(())
    }
    async fn on_execute<C>(&self, client: &mut C, message: Execute) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if message.max_rows < 0 {
            return Err(pg_error(EngineError::new(
                "08P01",
                "negative Execute maxRows",
            )));
        }
        let name = message.name.as_deref().unwrap_or(DEFAULT_NAME).to_string();
        let portal = match client.portal_store().get_portal(&name) {
            Some(Entry::Value(p)) => p,
            Some(Entry::Empty) => {
                client
                    .send(PgWireBackendMessage::EmptyQueryResponse(
                        EmptyQueryResponse::new(),
                    ))
                    .await?;
                return Ok(());
            }
            None => return Err(PgWireError::PortalNotFound(name)),
        };
        let mut cursor = self.cursors.lock().await.remove(&name);
        if cursor.is_none() {
            let s = &portal.statement.statement;
            let parameters = s
                .parameters
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    decode_parameter(
                        portal.parameters[i].as_deref(),
                        *t,
                        portal.parameter_format.is_binary(i),
                    )
                    .map_err(pg_error)
                })
                .collect::<PgWireResult<Vec<_>>>()?;
            cursor = Some(
                self.cursor(
                    client,
                    s.sql.clone(),
                    parameters,
                    s.parameters.clone(),
                    s.generations.clone(),
                )
                .await?,
            );
        }
        let mut cursor = cursor.unwrap();
        let mut count = 0usize;
        client.set_state(PgWireConnectionState::QueryInProgress);
        loop {
            if cursor.completion.is_some() {
                send(
                    client,
                    PgWireBackendMessage::CommandComplete(cursor.tag().into()),
                    self.config.pgwire.output_timeout_ms,
                )
                .await?;
                break;
            }
            if message.max_rows > 0 && count >= message.max_rows as usize {
                send(
                    client,
                    PgWireBackendMessage::PortalSuspended(PortalSuspended::new()),
                    self.config.pgwire.output_timeout_ms,
                )
                .await?;
                break;
            }
            if let Some(row) = cursor.next().await? {
                send(
                    client,
                    PgWireBackendMessage::DataRow(
                        encode_row(&row, &cursor.columns, &portal.result_column_format)
                            .map_err(pg_error)?,
                    ),
                    self.config.pgwire.output_timeout_ms,
                )
                .await?;
                count += 1;
            }
        }
        self.cursors.lock().await.insert(name, cursor);
        client.set_state(PgWireConnectionState::ReadyForQuery);
        Ok(())
    }
    async fn do_query<C>(
        &self,
        _: &mut C,
        _: &Portal<PreparedDescriptor>,
        _: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Err(pg_error(EngineError::unsupported(
            "use streaming on_execute",
        )))
    }
    async fn do_describe_statement<C>(
        &self,
        _: &mut C,
        target: &StoredStatement<PreparedDescriptor>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribeStatementResponse::new(
            self.get_parameter_types(&target.statement)?,
            fields(&target.statement.columns, &Format::UnifiedText),
        ))
    }
    async fn do_describe_portal<C>(
        &self,
        _: &mut C,
        target: &Portal<PreparedDescriptor>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribePortalResponse::new(fields(
            &target.statement.statement.columns,
            &target.result_column_format,
        )))
    }
    async fn on_close<C>(&self, client: &mut C, message: Close) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name.as_deref().unwrap_or(DEFAULT_NAME);
        match message.target_type {
            b'S' => {
                client.portal_store().rm_statement(name);
                self.names.lock().unwrap().statements.remove(name);
            }
            b'P' => {
                client.portal_store().rm_portal(name);
                self.cursors.lock().await.remove(name);
                self.names.lock().unwrap().portals.remove(name);
            }
            _ => return Err(PgWireError::InvalidTargetType(message.target_type)),
        };
        client
            .send(PgWireBackendMessage::CloseComplete(CloseComplete::new()))
            .await?;
        Ok(())
    }
    async fn on_sync<C>(&self, client: &mut C, _: Sync) -> PgWireResult<()>
    where
        C: ClientInfo
            + ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + std::marker::Sync,
        C::PortalStore: PortalStore<Statement = PreparedDescriptor>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        // With no explicit transaction support, Sync ends this implicit transaction.
        let portals = std::mem::take(&mut self.names.lock().unwrap().portals);
        for name in portals {
            client.portal_store().rm_portal(&name);
        }
        self.cursors.lock().await.clear();
        client.set_transaction_status(TransactionStatus::Idle);
        client
            .send(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                TransactionStatus::Idle,
            )))
            .await?;
        Ok(())
    }
}
fn validate_formats(codes: &[i16], count: usize) -> PgWireResult<()> {
    if (codes.len() > 1 && codes.len() != count) || codes.iter().any(|c| !matches!(c, 0 | 1)) {
        Err(pg_error(EngineError::new(
            "08P01",
            "invalid format code/count",
        )))
    } else {
        Ok(())
    }
}
