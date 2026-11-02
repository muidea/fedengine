//! Default-deny object authorization and versioned DDL capability decisions.
use fedengine_core::*;
use fedengine_ddl::{Analysis, Kind};
use modorch::{prelude::*, *};
use std::sync::Arc;
pub const POLICY_TOPIC: &str = "policy/authorize/v1";
#[derive(Clone)]
pub struct Authorize {
    pub principal: String,
    pub analysis: Analysis,
}
pub fn authorize(config: &Config, request: &Authorize) -> Result<()> {
    let a = &request.analysis;
    if !config.users.iter().any(|u| u.name == request.principal) {
        return Err(EngineError::denied("unknown principal"));
    }
    for object in &a.objects {
        let source = config
            .datasources
            .iter()
            .find(|s| s.id == object.source && s.enabled)
            .ok_or_else(|| EngineError::denied("disabled source"))?;
        if a.kind != Kind::Query && source.read_only {
            return Err(EngineError::denied("data source is read-only"));
        }
        let action = if a.kind == Kind::Query
            || (a.kind == Kind::Ddl && a.primary.as_ref().is_some_and(|p| p != object))
        {
            "SELECT"
        } else {
            &a.action
        };
        let allowed = config.grants.iter().any(|g| {
            g.principal == request.principal
                && g.source == object.source
                && (g.schema == "*" || g.schema == object.schema)
                && (g.object == "*" || g.object == object.name)
                && g.actions.contains(action)
        });
        if !allowed {
            return Err(EngineError::denied("object action is not granted"));
        }
    }
    if a.kind == Kind::Ddl {
        if !config.ddl.enabled || a.sources.len() != 1 {
            return Err(EngineError::denied("DDL disabled or not single-source"));
        }
        let source = &a.sources.iter().next().unwrap();
        let s = config
            .datasources
            .iter()
            .find(|s| &s.id == *source)
            .unwrap();
        if !s.ddl_enabled {
            return Err(EngineError::denied("source does not allow DDL"));
        }
        if a.action == "DROP_SCHEMA" && !config.ddl.allow_drop_schema {
            return Err(EngineError::denied("DROP SCHEMA is disabled"));
        }
        let capability = config.capabilities.iter().find(|c| &c.source == *source);
        if !capability.is_some_and(|c| {
            c.operations.contains(&a.action) && c.operations.contains(&a.capability)
        }) {
            return Err(EngineError::unsupported(
                "DDL operation has no verified version capability record",
            ));
        }
        if a.index.is_some() && !capability.unwrap().index_adapter {
            return Err(EngineError::unsupported(
                "remote index adapter has not been verified",
            ));
        }
    }
    Ok(())
}
pub struct PolicyFactory {
    pub config: Arc<Config>,
}
struct PolicyModule {
    config: Arc<Config>,
}
impl ModuleFactory for PolicyFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut d = ComponentDescriptor::new(ComponentRef::module("policy-manager"));
        d.requires
            .push(ComponentRef::initiator("datasource-registry"));
        d
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Module>> {
        Ok(Box::new(PolicyModule {
            config: self.config.clone(),
        }))
    }
}
#[async_trait]
impl Lifecycle for PolicyModule {}
impl Module for PolicyModule {
    fn descriptor(&self) -> ComponentDescriptor {
        PolicyFactory {
            config: self.config.clone(),
        }
        .descriptor()
    }
    fn handlers(&mut self) -> FrameworkResult<Option<HandlerRegistration>> {
        Ok(Some(HandlerRegistration::Serial {
            handler: Box::new(PolicyHandler {
                config: self.config.clone(),
            }),
            subscriptions: vec![SubscriptionSpec::new(
                20,
                self.descriptor().component(),
                POLICY_TOPIC,
            )],
        }))
    }
}
struct PolicyHandler {
    config: Arc<Config>,
}
#[async_trait]
impl SerialHandler for PolicyHandler {
    async fn handle(
        &mut self,
        _: &HandlerContext,
        message: MessageEnvelope,
    ) -> FrameworkResult<HandlerReply> {
        let r = message.payload::<Authorize>()?;
        Ok(Arc::new(authorize(&self.config, &r)))
    }
}
