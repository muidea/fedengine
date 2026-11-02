use modorch::{prelude::*, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Settings {
    prefix: String,
}
struct Config;
struct ConfigFactory;
#[async_trait]
impl Lifecycle for Config {}
impl Initiator for Config {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::initiator("settings"))
    }
    fn export_services(&self, publisher: &mut ServicePublisher) -> FrameworkResult<()> {
        publisher.publish(
            "settings",
            Arc::new(Settings {
                prefix: "hello".into(),
            }),
        )
    }
}
impl InitiatorFactory for ConfigFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        Config.descriptor()
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(Config))
    }
}
struct EchoFactory {
    ticks: Arc<AtomicUsize>,
}
struct Echo {
    settings: Option<ServiceHandle<Settings>>,
    ticks: Arc<AtomicUsize>,
}
struct EchoHandler {
    settings: ServiceHandle<Settings>,
}
impl ModuleFactory for EchoFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut d = ComponentDescriptor::new(ComponentRef::module("echo"));
        d.requires.push(ComponentRef::initiator("settings"));
        d
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(Echo {
            settings: None,
            ticks: self.ticks.clone(),
        }))
    }
}
#[async_trait]
impl Lifecycle for Echo {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        self.settings = Some(
            ctx.services
                .get(&ComponentRef::initiator("settings"), "settings")?,
        );
        Ok(())
    }
    async fn start(&mut self, ctx: &RuntimeContext) -> FrameworkResult<()> {
        let ticks = self.ticks.clone();
        ctx.tasks.interval(
            IntervalSpec {
                period: Duration::from_millis(5),
                policy: TickPolicy::SkipIfRunning,
            },
            Arc::new(move |_| {
                let ticks = ticks.clone();
                Box::pin(async move {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
        )?;
        Ok(())
    }
}
impl Module for Echo {
    fn descriptor(&self) -> ComponentDescriptor {
        EchoFactory {
            ticks: self.ticks.clone(),
        }
        .descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Serial {
            handler: Box::new(EchoHandler {
                settings: self.settings.as_ref().unwrap().clone(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                1,
                self.descriptor().component(),
                "echo/value/v1",
            )],
        }))
    }
}
#[async_trait]
impl SerialHandler for EchoHandler {
    async fn handle(
        &mut self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let lease = self.settings.acquire()?;
        let value = message.payload::<usize>()?;
        Ok(Arc::new(format!("{} {}", lease.prefix, value)))
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config =
        modorch::config::CompositionConfig::parse(include_str!("../config/application.toml"))?;
    let ticks = Arc::new(AtomicUsize::new(0));
    let mut builder = ApplicationBuilder::new(&config.application.name)
        .selection(config.selection()?)
        .queue_capacities(
            config.messaging.default_queue_capacity,
            config.messaging.per_shard_capacity,
        );
    builder
        .register_initiator(ConfigFactory)?
        .register_module(EchoFactory {
            ticks: ticks.clone(),
        })?;
    let handle = builder.build()?.start().await?;
    let reply = handle
        .event_hub()
        .external()
        .request::<_, String>(
            "echo/value/v1",
            Destination::Component(ComponentRef::module("echo")),
            42usize,
            Default::default(),
        )?
        .wait(Duration::from_secs(1))
        .await?;
    assert_eq!(reply.as_str(), "hello 42");
    tokio::time::timeout(Duration::from_secs(1), async {
        while ticks.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    handle
        .shutdown_checked(ShutdownOptions {
            timeout: config.grace_period(),
        })
        .await?;
    assert_eq!(handle.health().pending.operations, 0);
    assert_eq!(handle.task_metrics().tasks, 0);
    println!("modular-service: hello 42, ticks>=2, drained");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await
}
#[tokio::test]
async fn config_service_request_and_timer_are_integrated() {
    run().await.unwrap();
}
