//! Registered source identities, generation fences and resource lifecycle.
use fedengine_core::*;
use modorch::{prelude::*, *};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::RwLock;
pub struct SourceState {
    pub definition: DataSourceDefinition,
    pub generation: AtomicU64,
    pub stale: AtomicBool,
    pub fence: Arc<RwLock<()>>,
}
pub struct DataSourceRegistry {
    sources: BTreeMap<DataSourceId, Arc<SourceState>>,
}
impl DataSourceRegistry {
    pub fn new(definitions: &[DataSourceDefinition]) -> Self {
        Self {
            sources: definitions
                .iter()
                .filter(|d| d.enabled)
                .map(|d| {
                    (
                        d.id.clone(),
                        Arc::new(SourceState {
                            definition: d.clone(),
                            generation: AtomicU64::new(1),
                            stale: AtomicBool::new(false),
                            fence: Arc::new(RwLock::new(())),
                        }),
                    )
                })
                .collect(),
        }
    }
    pub fn get(&self, id: &DataSourceId) -> Result<Arc<SourceState>> {
        self.sources
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::new("3D000", "unregistered data source"))
    }
    pub fn sources(&self) -> impl Iterator<Item = &Arc<SourceState>> {
        self.sources.values()
    }
    pub fn generations(
        &self,
        ids: impl Iterator<Item = DataSourceId>,
    ) -> BTreeMap<DataSourceId, u64> {
        ids.filter_map(|id| {
            self.sources
                .get(&id)
                .map(|s| (id, s.generation.load(Ordering::SeqCst)))
        })
        .collect()
    }
    pub fn invalidate(&self, id: &DataSourceId, stale: bool) -> Result<u64> {
        let s = self.get(id)?;
        s.stale.store(stale, Ordering::SeqCst);
        Ok(s.generation.fetch_add(1, Ordering::SeqCst) + 1)
    }
}
pub struct DataSourceFactory {
    pub definitions: Vec<DataSourceDefinition>,
}
struct DataSourceInitiator {
    definitions: Vec<DataSourceDefinition>,
    registry: Option<Arc<DataSourceRegistry>>,
}
impl InitiatorFactory for DataSourceFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::initiator("datasource-registry"))
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(DataSourceInitiator {
            definitions: self.definitions.clone(),
            registry: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for DataSourceInitiator {
    async fn setup(&mut self, _: &SetupContext) -> FrameworkResult<()> {
        self.registry = Some(Arc::new(DataSourceRegistry::new(&self.definitions)));
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.registry.take();
        Ok(())
    }
}
impl Initiator for DataSourceInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentRef::initiator("datasource-registry"))
    }
    fn export_services(&self, p: &mut ServicePublisher) -> FrameworkResult<()> {
        p.publish("registry", self.registry.as_ref().unwrap().clone())
    }
}
