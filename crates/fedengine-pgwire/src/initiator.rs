use crate::{
    auth::{Authentication, load_tls},
    cancel::Cancellations,
    session::Backend,
};
use fedengine_contract::{EngineMetrics, GaugeGuard};
use fedengine_core::Config;
use modorch::{prelude::*, *};
use pgwire::api::{
    PgWireServerHandlers,
    auth::StartupHandler,
    auth::{Password, sasl::scram::gen_salted_password},
    cancel::CancelHandler,
    query::{ExtendedQueryHandler, SimpleQueryHandler},
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, Semaphore},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
#[derive(Default)]
pub struct ListenerStatus {
    address: Mutex<Option<SocketAddr>>,
}
impl ListenerStatus {
    pub fn address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap()
    }
}
pub struct PgwireFactory {
    pub metrics: Arc<EngineMetrics>,
    pub config: Arc<Config>,
    pub status: Arc<ListenerStatus>,
}
struct PgwireInitiator {
    metrics: Arc<EngineMetrics>,
    config: Arc<Config>,
    status: Arc<ListenerStatus>,
    listener: Option<TcpListener>,
    tls: Option<TlsAcceptor>,
    passwords: Arc<BTreeMap<String, Password>>,
    stop: CancellationToken,
}
fn descriptor() -> ComponentDescriptor {
    ComponentDescriptor::new(ComponentRef::initiator("pgwire-listener"))
}
impl InitiatorFactory for PgwireFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(PgwireInitiator {
            metrics: self.metrics.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            listener: None,
            tls: None,
            passwords: Arc::new(BTreeMap::new()),
            stop: CancellationToken::new(),
        }))
    }
}
pub(super) fn fw(message: impl Into<String>) -> FrameworkError {
    FrameworkError::InvalidConfig(message.into())
}
#[async_trait]
impl Lifecycle for PgwireInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        let users = self.config.users.clone();
        self.passwords = Arc::new(
            ctx.tasks
                .spawn_blocking_io(move |_| {
                    let mut passwords = BTreeMap::new();
                    for user in users {
                        let password = std::env::var(&user.password_secret_ref)
                            .map_err(|_| fw("authentication SecretRef is missing"))?;
                        if password.is_empty() {
                            return Err(fw("authentication secret is empty"));
                        }
                        let salt = rand::random::<[u8; 16]>().to_vec();
                        let hash = gen_salted_password(&password, &salt, 4096);
                        passwords.insert(user.name, Password::new(Some(salt), hash));
                    }
                    Ok(passwords)
                })?
                .wait_finished()
                .await?,
        );
        self.tls = load_tls(&self.config.pgwire)?;
        let listener = TcpListener::bind(&self.config.pgwire.listen)
            .await
            .map_err(|e| fw(e.to_string()))?;
        *self.status.address.lock().unwrap() =
            Some(listener.local_addr().map_err(|e| fw(e.to_string()))?);
        self.listener = Some(listener);
        Ok(())
    }
    async fn begin_shutdown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.stop.cancel();
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.listener.take();
        *self.status.address.lock().unwrap() = None;
        Ok(())
    }
}
#[async_trait]
impl Initiator for PgwireInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor()
    }
    async fn activate_ingress(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        let listener = self.listener.take().unwrap();
        let tasks = ctx.tasks.clone();
        let hub = ctx.event_hub.external();
        let config = self.config.clone();
        let tls = self.tls.clone();
        let passwords = self.passwords.clone();
        let stop = self.stop.clone();
        let slots = Arc::new(Semaphore::new(config.pgwire.max_connections));
        let cancellations = Arc::new(Cancellations::default());
        let metrics = self.metrics.clone();
        ctx.tasks.spawn(move |cancel|async move{
            loop{
                let (socket,_)=tokio::select!{biased;_=stop.cancelled()=>break,_=cancel.cancelled()=>break,result=listener.accept()=>result.map_err(|e|fw(e.to_string()))?};
                let Ok(permit)=slots.clone().try_acquire_owned()else{drop(socket);continue;};
                socket.set_nodelay(true).map_err(|e|fw(e.to_string()))?;
                let config=config.clone();let tls=tls.clone();let passwords=passwords.clone();let hub=hub.clone();let cancellations=cancellations.clone();let stopped=stop.clone();let metrics=metrics.clone();
                tasks.spawn(move |shutdown|async move{
                    let _session=GaugeGuard::enter(metrics.active_sessions.clone());
                    let _permit=permit;let ready=Arc::new(Notify::new());let authenticated=ready.clone();
                    let backend=Arc::new(Backend::new(hub, config.clone(), cancellations.clone()));
                    let auth=Arc::new(Authentication::new(passwords, config.pgwire.tls_required, ready));
                    let handlers=Arc::new(Handlers{backend,auth,cancellations});
                    let process=pgwire::tokio::process_socket(socket,tls,handlers);tokio::pin!(process);
                    let startup=async{tokio::time::timeout(Duration::from_millis(config.pgwire.startup_timeout_ms),authenticated.notified()).await};
                    tokio::pin!(startup);
                    tokio::select!{biased;_=stopped.cancelled()=>{},_=shutdown.cancelled()=>{},_=&mut process=>{},result=&mut startup=>{
                        if result.is_ok(){tokio::select!{_=stopped.cancelled()=>{},_=shutdown.cancelled()=>{},_=&mut process=>{}}}
                    }}
                    Ok(())
                })?;
            }Ok(())
        })?;
        Ok(())
    }
}
struct Handlers {
    backend: Arc<Backend>,
    auth: Arc<Authentication>,
    cancellations: Arc<Cancellations>,
}
impl PgWireServerHandlers for Handlers {
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.auth.clone()
    }
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.backend.clone()
    }
    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.backend.clone()
    }
    fn cancel_handler(&self) -> Arc<impl CancelHandler> {
        self.cancellations.clone()
    }
}
