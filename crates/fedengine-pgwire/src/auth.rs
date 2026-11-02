use crate::initiator::fw;
use fedengine_core::{EngineError, PgwireConfig};
use fedengine_pg_compat::pg_error;
use futures::Sink;
use modorch::{FrameworkResult, prelude::async_trait};
use pgwire::{
    api::{
        auth::{
            sasl::{SASLAuthStartupHandler, scram::ScramAuth},
            *,
        },
        *,
    },
    error::{PgWireError, PgWireResult},
    messages::{PgWireBackendMessage, PgWireFrontendMessage},
};
use std::{collections::BTreeMap, fmt::Debug, sync::Arc};
use tokio::sync::Notify;
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};
fn server_parameters() -> DefaultServerParameterProvider {
    let mut p = DefaultServerParameterProvider::default();
    p.server_version = "16.0 (FedEngine 0.1; PostgreSQL protocol subset)".into();
    p.is_superuser = false;
    p.client_encoding = Some("UTF8".into());
    p.time_zone = "UTC".into();
    p
}
pub(super) fn load_tls(config: &PgwireConfig) -> FrameworkResult<Option<TlsAcceptor>> {
    let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) else {
        return Ok(None);
    };
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert).map_err(|_| fw("cannot open TLS certificate"))?,
    ))
    .collect::<std::io::Result<Vec<_>>>()
    .map_err(|_| fw("invalid TLS certificate"))?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key).map_err(|_| fw("cannot open TLS key"))?,
    ))
    .map_err(|_| fw("invalid TLS key"))?
    .ok_or_else(|| fw("TLS key is empty"))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| fw("invalid TLS certificate/key pair"))?;
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}
#[derive(Debug)]
struct Passwords(Arc<BTreeMap<String, Password>>);
#[async_trait]
impl AuthSource for Passwords {
    async fn get_password(&self, login: &LoginInfo) -> PgWireResult<Password> {
        self.0
            .get(login.user().unwrap_or(""))
            .cloned()
            .ok_or_else(|| pg_error(EngineError::new("28P01", "authentication failed")))
    }
}
pub(super) struct Authentication {
    inner: SASLAuthStartupHandler<DefaultServerParameterProvider>,
    tls_required: bool,
    ready: Arc<Notify>,
}
#[async_trait]
impl StartupHandler for Authentication {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + std::marker::Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if self.tls_required && !client.is_secure() {
            return Err(pg_error(EngineError::new("28000", "TLS is required")));
        }
        if let PgWireFrontendMessage::Startup(s) = &message
            && s.parameters
                .get("client_encoding")
                .is_some_and(|v| !["UTF8", "UTF-8"].contains(&v.to_uppercase().as_str()))
        {
            return Err(pg_error(EngineError::unsupported(
                "only UTF8 client encoding is supported",
            )));
        }
        self.inner.on_startup(client, message).await?;
        if matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
            self.ready.notify_one();
        }
        Ok(())
    }
}

impl Authentication {
    pub(super) fn new(
        passwords: Arc<BTreeMap<String, Password>>,
        tls_required: bool,
        ready: Arc<Notify>,
    ) -> Self {
        Self {
            inner: SASLAuthStartupHandler::new(Arc::new(server_parameters()))
                .with_scram(ScramAuth::new(Arc::new(Passwords(passwords)))),
            tls_required,
            ready,
        }
    }
}
