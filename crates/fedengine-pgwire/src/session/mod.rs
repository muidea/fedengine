//! Per-connection state and shared query dispatch.
mod cursor;
mod extended;
mod parser;
mod simple;

use crate::cancel::{CancelKey, Cancellations};
use cursor::Cursor;
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_pg_compat::pg_error;
use futures::{Sink, SinkExt};
use modorch::*;
use pgwire::{
    api::*,
    error::{PgWireError, PgWireResult},
    messages::PgWireBackendMessage,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt::Debug,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct Names {
    statements: HashSet<String>,
    portals: HashSet<String>,
}
pub(super) struct Backend {
    hub: ExternalEventHub,
    config: Arc<Config>,
    cancellations: Arc<Cancellations>,
    key: Mutex<Option<CancelKey>>,
    names: Mutex<Names>,
    session_state: Mutex<Option<SessionContext>>,
    seen_notices: Mutex<VecDeque<String>>,
    cursors: tokio::sync::Mutex<BTreeMap<String, Cursor>>,
}
impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(key) = self.key.lock().unwrap().take()
            && let Some(token) = self.cancellations.0.lock().unwrap().remove(&key)
        {
            token.cancel();
        }
    }
}
struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}
impl Backend {
    pub(super) fn new(
        hub: ExternalEventHub,
        config: Arc<Config>,
        cancellations: Arc<Cancellations>,
    ) -> Self {
        Self {
            hub,
            config,
            cancellations,
            key: Mutex::new(None),
            names: Mutex::new(Names::default()),
            session_state: Mutex::new(None),
            seen_notices: Mutex::new(VecDeque::new()),
            cursors: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn session<C: ClientInfo>(&self, client: &C) -> SessionContext {
        let mut state = self.session_state.lock().unwrap();
        state
            .get_or_insert_with(|| SessionContext {
                database_oid: client
                    .metadata()
                    .get("fedengine_database_oid")
                    .and_then(|v| v.parse().ok()),
                database: client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_else(|| {
                        client
                            .metadata()
                            .get(METADATA_USER)
                            .cloned()
                            .unwrap_or_default()
                    }),
                settings: client
                    .metadata()
                    .get(METADATA_APPLICATION_NAME)
                    .map(|v| {
                        [("application_name".into(), v.clone())]
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default(),
                ..Default::default()
            })
            .clone()
    }

    async fn database_version<C: ClientInfo>(
        &self,
        client: &C,
    ) -> PgWireResult<Option<DatabaseVersion>> {
        use fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
        let timeout = Duration::from_millis(self.config.pgwire.query_timeout_ms);
        let reply = self
            .hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                CatalogRequest::Headers,
                Default::default(),
            )
            .map_err(|_| pg_error(EngineError::new("53300", "catalog admission unavailable")))?
            .wait(timeout)
            .await
            .map_err(|_| {
                pg_error(EngineError::new(
                    "57014",
                    "catalog identity check timed out",
                ))
            })?;
        let headers = match reply.as_ref() {
            Ok(CatalogReply::Headers(h)) => h,
            Err(e) => return Err(pg_error(e.clone())),
            _ => {
                return Err(pg_error(EngineError::new(
                    "XX000",
                    "invalid catalog identity reply",
                )));
            }
        };
        let session = self.session(client);
        let db = headers
            .databases
            .iter()
            .find(|d| d.config.name == session.database);
        let oid = db.map(|d| d.oid).or_else(|| {
            headers
                .sources
                .iter()
                .find(|h| !h.source.deleted && h.source.catalog_alias == session.database)
                .and_then(|h| h.snapshot.as_ref())
                .map(|s| s.database_oid)
        });
        if session.database_oid.is_some() && session.database_oid != oid {
            return Err(pg_error(EngineError::new(
                "3D000",
                "connected database was removed or replaced; reconnect",
            )));
        }
        Ok(db.map(|d| DatabaseVersion {
            oid: d.oid,
            revision: d.revision,
        }))
    }
    async fn check_prepared<C: ClientInfo>(
        &self,
        client: &C,
        descriptor: &PreparedDescriptor,
    ) -> PgWireResult<()> {
        let version = self.database_version(client).await?;
        if descriptor.namespace_version != self.session(client).namespace_version
            || descriptor.database_version != version
        {
            return Err(pg_error(EngineError::new(
                "0A000",
                "session namespace or virtual database definition changed; Parse/Bind the statement again",
            )));
        }
        Ok(())
    }
    async fn call<C: ClientInfo + Send + std::marker::Sync>(
        &self,
        client: &C,
        sql: String,
        mode: QueryMode,
    ) -> PgWireResult<Arc<Result<QueryReply>>> {
        let principal = client
            .metadata()
            .get(METADATA_USER)
            .cloned()
            .unwrap_or_default();
        let session = self.session(client);
        if let Some((next, command)) =
            fedengine_pg_compat::session_command(&sql, &session).map_err(pg_error)?
        {
            let database_version = self.database_version(client).await?;
            if let QueryMode::Execute {
                expected_database: Some(expected),
                ..
            } = &mode
                && Some(*expected) != database_version
            {
                return Err(pg_error(EngineError::new(
                    "0A000",
                    "virtual database definition changed; Parse/Bind the statement again",
                )));
            }
            let reply = match mode {
                QueryMode::Prepare {
                    ref parameter_types,
                } => {
                    if !parameter_types.is_empty() {
                        return Err(pg_error(EngineError::new(
                            "08P01",
                            "session command has no parameters",
                        )));
                    }
                    QueryReply::Prepared(PreparedDescriptor {
                        namespace_version: session.namespace_version,
                        database_version,
                        sql,
                        parameters: vec![],
                        columns: vec![],
                        generations: Default::default(),
                        kind: StatementKind::Session,
                    })
                }
                QueryMode::Execute { ref parameters, .. } => {
                    if !parameters.is_empty() {
                        return Err(pg_error(EngineError::new(
                            "08P01",
                            "session command has no parameters",
                        )));
                    }
                    *self.session_state.lock().unwrap() = Some(next);
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    tx.try_send(ResultChunk::Complete(Completion {
                        notices: vec![],
                        command,
                        affected_rows: None,
                        outcome: ExecutionOutcome::Completed,
                    }))
                    .map_err(|_| {
                        pg_error(EngineError::new("XX000", "local session response failed"))
                    })?;
                    QueryReply::Accepted(AcceptedExecution::new(
                        vec![],
                        StatementKind::Session,
                        ExecutionStream {
                            receiver: rx,
                            cancel: CancellationToken::new(),
                        },
                    ))
                }
            };
            return Ok(Arc::new(Ok(reply)));
        }
        let token = CancellationToken::new();
        let (pid, secret) = client.pid_and_secret_key();
        let key = (pid, secret.to_bytes().to_vec());
        *self.key.lock().unwrap() = Some(key.clone());
        self.cancellations
            .0
            .lock()
            .unwrap()
            .insert(key, token.clone());
        let mut guard = CancelOnDrop {
            token: token.clone(),
            armed: true,
        };
        let timeout = Duration::from_millis(self.config.pgwire.query_timeout_ms);
        let request = QueryRequest {
            session: self.session(client),
            principal,
            session_id: pid,
            sql,
            mode,
            deadline: Instant::now() + timeout,
            cancel: token.clone(),
        };
        let handle = self
            .hub
            .request::<_, Result<QueryReply>>(
                QUERY_TOPIC,
                Destination::Component(ComponentRef::module("query-coordinator")),
                request,
                Default::default(),
            )
            .map_err(|e| {
                pg_error(EngineError::new(
                    "53300",
                    format!("query admission failed: {e}"),
                ))
            })?;
        let reply = tokio::select! {_=token.cancelled()=>return Err(pg_error(EngineError::new("57014","query canceled before execution receipt"))),r=handle.wait(timeout)=>r.map_err(|_|pg_error(EngineError::new("57014","query control deadline exceeded; write outcome may be unknown").unknown()))?};
        if let Err(e) = reply.as_ref() {
            return Err(pg_error(e.clone()));
        }
        guard.armed = false;
        Ok(reply)
    }
    async fn send_notices<C>(&self, client: &mut C, cursor: &mut Cursor) -> PgWireResult<()>
    where
        C: Sink<PgWireBackendMessage> + Unpin,
        C::Error: Debug,
        PgWireError: From<C::Error>,
    {
        for (key, message) in std::mem::take(&mut cursor.notices) {
            let fresh = {
                let mut seen = self.seen_notices.lock().unwrap();
                if seen.contains(&key) {
                    false
                } else {
                    if seen.len() == 64 {
                        seen.pop_front();
                    }
                    seen.push_back(key);
                    true
                }
            };
            if fresh {
                send(
                    client,
                    PgWireBackendMessage::NoticeResponse(
                        pgwire::error::ErrorInfo::new("NOTICE".into(), "01000".into(), message)
                            .into(),
                    ),
                    self.config.pgwire.output_timeout_ms,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn cursor<C: ClientInfo + Send + std::marker::Sync>(
        &self,
        client: &C,
        sql: String,
        parameters: Vec<Cell>,
        parameter_types: Vec<PgType>,
        expected: std::collections::BTreeMap<DataSourceId, fedengine_core::SourceVersion>,
        expected_database: Option<DatabaseVersion>,
    ) -> PgWireResult<Cursor> {
        let reply = self
            .call(
                client,
                sql,
                QueryMode::Execute {
                    parameters,
                    parameter_types,
                    expected,
                    expected_database,
                },
            )
            .await?;
        let Ok(QueryReply::Accepted(a)) = reply.as_ref() else {
            return Err(pg_error(EngineError::new(
                "XX000",
                "invalid execution reply",
            )));
        };
        Ok(Cursor {
            notices: a.notices.clone(),
            columns: a.columns.clone(),
            stream: Some(a.take_stream().map_err(pg_error)?),
            pending: VecDeque::new(),
            completion: None,
            total: 0,
            kind: a.kind,
        })
    }
}
async fn send<C>(client: &mut C, message: PgWireBackendMessage, timeout: u64) -> PgWireResult<()>
where
    C: Sink<PgWireBackendMessage> + Unpin,
    C::Error: Debug,
    PgWireError: From<C::Error>,
{
    tokio::time::timeout(Duration::from_millis(timeout), client.send(message))
        .await
        .map_err(|_| pg_error(EngineError::new("57014", "client output timed out")))?
        .map_err(Into::into)
}
