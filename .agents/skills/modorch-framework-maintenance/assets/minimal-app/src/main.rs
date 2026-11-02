use modorch::{prelude::*, *};
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<&'static str>>>;
struct Greeting {
    log: Log,
}
struct GreetingFactory {
    log: Log,
}
impl ModuleFactory for GreetingFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::module("greeting"))
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(Greeting {
            log: self.log.clone(),
        }))
    }
}
#[async_trait]
impl Lifecycle for Greeting {
    async fn setup(&mut self, _: &SetupContext) -> FrameworkResult<()> {
        self.log.lock().unwrap().push("setup");
        Ok(())
    }
    async fn start(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        self.log.lock().unwrap().push("start");
        Ok(())
    }
    async fn ready(&mut self, _: &RuntimeContext) -> FrameworkResult<()> {
        self.log.lock().unwrap().push("ready");
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.log.lock().unwrap().push("teardown");
        Ok(())
    }
}
impl Module for Greeting {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::module("greeting"))
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let log = Log::default();
    let mut builder = ApplicationBuilder::new("basic-app");
    builder.register_module(GreetingFactory { log: log.clone() })?;
    let mut selection = ComponentSelection::new();
    selection.enable_module("greeting");
    let app = builder.selection(selection).build()?;
    assert!(
        log.lock().unwrap().is_empty(),
        "build must not create/setup components"
    );
    let handle = app.start().await?;
    handle.shutdown_checked(ShutdownOptions::graceful()).await?;
    assert_eq!(
        *log.lock().unwrap(),
        ["setup", "start", "ready", "teardown"]
    );
    assert_eq!(handle.health().pending.operations, 0);
    println!("basic-app: stopped");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await
}
#[tokio::test]
async fn selected_module_runs_and_releases() {
    run().await.unwrap();
}
