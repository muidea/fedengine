use crate::initiator::fw;
use fedengine_catalog::{CATALOG_TOPIC, CatalogReply, CatalogRequest};
use fedengine_core::{Config, EngineError, PgwireConfig, Result};
use fedengine_pg_compat::pg_error;
use futures::{Sink, SinkExt};
use modorch::{ComponentRef, Destination, ExternalEventHub};
use modorch::{FrameworkResult, prelude::async_trait};
use pgwire::{
    api::{
        auth::{
            driver::AuthOutcome,
            mechanism::AuthMechanismFactory,
            sasl::{SaslMechanisms, scram::ScramAuth},
            *,
        },
        *,
    },
    error::{PgWireError, PgWireResult},
    messages::{PgWireBackendMessage, PgWireFrontendMessage},
};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    sync::Arc,
};
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
struct Parameters {
    defaults: DefaultServerParameterProvider,
    search_path: std::sync::Mutex<String>,
}
impl ServerParameterProvider for Parameters {
    fn server_parameters<C: ClientInfo>(&self, client: &C) -> Option<HashMap<String, String>> {
        let mut values = self.defaults.server_parameters(client)?;
        values.insert(
            "search_path".into(),
            self.search_path.lock().unwrap().clone(),
        );
        values.insert("DateStyle".into(), "ISO, YMD".into());
        Some(values)
    }
}
pub(super) struct Authentication {
    driver: AuthDriver<Arc<Parameters>>,
    mechanisms: SaslMechanisms,
    parameters: Arc<Parameters>,
    config: Arc<Config>,
    hub: ExternalEventHub,
    tls_required: bool,
    ready: Arc<Notify>,
    failed: tokio_util::sync::CancellationToken,
}
#[async_trait]
impl StartupHandler for Authentication {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match self.startup(client, message).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // pgwire 0.42.1's generic error path emits ReadyForQuery even for FATAL.
                // Flush the terminal response ourselves, then let the listener drop the
                // entire socket/task. Never hand a startup failure to that query path.
                let mut info: pgwire::error::ErrorInfo = error.into();
                info.severity = "FATAL".into();
                info.severity_nonlocalized = Some("FATAL".into());
                let _ = client
                    .send(PgWireBackendMessage::ErrorResponse(info.into()))
                    .await;
                self.failed.cancel();
                // The listener selects `failed` and drops this future and its socket.
                // Pending here also prevents buffered frontend messages being admitted.
                std::future::pending().await
            }
        }
    }
}
impl Authentication {
    async fn startup<C>(&self, client: &mut C, message: PgWireFrontendMessage) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(mut startup) => {
                if self.tls_required && !client.is_secure() {
                    return Err(pg_error(EngineError::new("28000", "TLS is required")));
                }
                if startup
                    .parameters
                    .get("client_encoding")
                    .is_some_and(|v| !["UTF8", "UTF-8"].contains(&v.to_uppercase().as_str()))
                {
                    return Err(pg_error(EngineError::unsupported(
                        "only UTF8 client encoding is supported",
                    )));
                }
                if startup
                    .parameters
                    .get("options")
                    .is_some_and(|s| !s.is_empty())
                {
                    return Err(pg_error(EngineError::unsupported(
                        "startup options are unsupported; use SET after connection",
                    )));
                }
                let principal = startup
                    .parameters
                    .get("user")
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .ok_or(PgWireError::UserNameRequired)?;
                let database = startup
                    .parameters
                    .get("database")
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or(principal);
                startup.parameters.insert("database".into(), database);
                protocol_negotiation(client, &startup).await?;
                save_startup_parameters_to_metadata(client, &startup);
                self.driver
                    .begin_authentication(client, self.mechanisms.create())
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(message) => {
                if let AuthOutcome::Authenticated { final_message, .. } =
                    self.driver.advance_authentication(client, message).await?
                {
                    self.admit(client).await?;
                    self.driver
                        .complete_authentication(client, final_message)
                        .await?;
                    self.ready.notify_one();
                }
            }
            _ => {
                return Err(pg_error(EngineError::new(
                    "08P01",
                    "unexpected message during authentication",
                )));
            }
        }
        Ok(())
    }
    async fn admit<C: ClientInfo>(&self, client: &mut C) -> PgWireResult<()> {
        let principal = client.metadata().get("user").cloned().unwrap_or_default();
        let database = client
            .metadata()
            .get("database")
            .cloned()
            .unwrap_or_default();
        let timeout = std::time::Duration::from_millis(self.config.pgwire.startup_timeout_ms);
        let reply = self
            .hub
            .request::<_, Result<CatalogReply>>(
                CATALOG_TOPIC,
                Destination::Component(ComponentRef::module("catalog-manager")),
                CatalogRequest::Headers,
                Default::default(),
            )
            .map_err(|_| {
                pg_error(EngineError::new(
                    "53300",
                    "catalog startup admission failed",
                ))
            })?
            .wait(timeout)
            .await
            .map_err(|_| pg_error(EngineError::new("57014", "catalog startup timed out")))?;
        let headers = match reply.as_ref() {
            Ok(CatalogReply::Headers(h)) => h.clone(),
            Err(e) => return Err(pg_error(e.clone())),
            _ => {
                return Err(pg_error(EngineError::new(
                    "XX000",
                    "invalid startup catalog response",
                )));
            }
        };
        let definitions = headers
            .iter()
            .filter_map(|h| h.applied.as_ref().map(|a| a.effective_definition()))
            .collect::<Vec<_>>();
        let scope = fedengine_policy::metadata_scope(
            &self.config,
            &principal,
            &database,
            headers,
            &definitions,
        )
        .map_err(pg_error)?;
        let database_oid = scope
            .virtual_database
            .as_ref()
            .map(|d| d.oid)
            .or_else(|| {
                scope
                    .catalogs
                    .iter()
                    .find(|c| c.catalog == database)
                    .map(|c| c.database_oid)
            })
            .ok_or_else(|| pg_error(EngineError::new("3D000", "database identity unavailable")))?;
        client
            .metadata_mut()
            .insert("fedengine_database_oid".into(), database_oid.to_string());
        let session = fedengine_contract::SessionContext {
            database,
            ..Default::default()
        };
        *self.parameters.search_path.lock().unwrap() =
            fedengine_pg_compat::setting(&session, &scope, "search_path").map_err(pg_error)?;
        Ok(())
    }
    pub(super) fn new(
        passwords: Arc<BTreeMap<String, Password>>,
        config: Arc<Config>,
        hub: ExternalEventHub,
        ready: Arc<Notify>,
        failed: tokio_util::sync::CancellationToken,
    ) -> Self {
        let parameters = Arc::new(Parameters {
            defaults: server_parameters(),
            search_path: Default::default(),
        });
        Self {
            driver: AuthDriver::new(parameters.clone()),
            mechanisms: SaslMechanisms::new(vec![Arc::new(ScramAuth::new(Arc::new(Passwords(
                passwords,
            ))))]),
            parameters,
            tls_required: config.pgwire.tls_required,
            config,
            hub,
            ready,
            failed,
        }
    }
}
