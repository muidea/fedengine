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
            cursors: tokio::sync::Mutex::new(BTreeMap::new()),
        }
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
    async fn cursor<C: ClientInfo + Send + std::marker::Sync>(
        &self,
        client: &C,
        sql: String,
        parameters: Vec<Cell>,
        parameter_types: Vec<PgType>,
        expected: std::collections::BTreeMap<DataSourceId, u64>,
    ) -> PgWireResult<Cursor> {
        let reply = self
            .call(
                client,
                sql,
                QueryMode::Execute {
                    parameters,
                    parameter_types,
                    expected,
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
