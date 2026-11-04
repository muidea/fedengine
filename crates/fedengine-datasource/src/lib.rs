//! Versioned effective snapshots, source admission and instance execution barriers.
use fedengine_contract::ManagementStorage;
use fedengine_core::*;
use modorch::{prelude::*, *};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock as SyncRwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::RwLock;

pub struct SourceState {
    pub definition: DataSourceDefinition,
    pub incarnation: String,
    pub revision: u64,
    pub generation: AtomicU64,
    pub stale: AtomicBool,
    pub admitted: AtomicBool,
    pub fence: Arc<RwLock<()>>,
}
impl SourceState {
    pub fn version(&self) -> SourceVersion {
        SourceVersion {
            incarnation: self.incarnation.clone(),
            revision: self.revision,
            generation: self.generation.load(Ordering::SeqCst),
        }
    }
}
pub struct DataSourceRegistry {
    sources: SyncRwLock<BTreeMap<DataSourceId, Arc<SourceState>>>,
    /// Always acquire this barrier before any source fence, including metadata work.
    pub instance: Arc<RwLock<()>>,
}
impl DataSourceRegistry {
    pub fn new(definitions: &[DataSourceDefinition]) -> Self {
        let registry = Self {
            sources: SyncRwLock::new(BTreeMap::new()),
            instance: Arc::new(RwLock::new(())),
        };
        for definition in definitions.iter().filter(|d| d.enabled) {
            registry.publish(
                definition.clone(),
                format!("static:{}", definition.id.0),
                1,
                1,
            );
        }
        registry
    }
    pub fn get(&self, id: &DataSourceId) -> Result<Arc<SourceState>> {
        self.sources
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::new("3D000", "unregistered or inactive data source"))
    }
    pub fn sources(&self) -> std::vec::IntoIter<Arc<SourceState>> {
        self.sources
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
    }
    pub fn definitions(&self) -> Vec<DataSourceDefinition> {
        self.sources()
            .filter(|s| s.admitted.load(Ordering::SeqCst))
            .map(|s| s.definition.clone())
            .collect()
    }
    pub fn generations(
        &self,
        ids: impl Iterator<Item = DataSourceId>,
    ) -> BTreeMap<DataSourceId, SourceVersion> {
        let sources = self.sources.read().unwrap();
        ids.filter_map(|id| sources.get(&id).map(|s| (id, s.version())))
            .collect()
    }
    pub fn publish(
        &self,
        definition: DataSourceDefinition,
        incarnation: String,
        revision: u64,
        generation: u64,
    ) {
        let mut sources = self.sources.write().unwrap();
        let fence = sources
            .get(&definition.id)
            .map(|s| s.fence.clone())
            .unwrap_or_default();
        if let Some(old) = sources.get(&definition.id) {
            old.admitted.store(false, Ordering::SeqCst);
        }
        sources.insert(
            definition.id.clone(),
            Arc::new(SourceState {
                definition,
                incarnation,
                revision,
                generation: AtomicU64::new(generation),
                stale: AtomicBool::new(false),
                admitted: AtomicBool::new(true),
                fence,
            }),
        );
    }
    pub fn close(&self, id: &DataSourceId) {
        if let Ok(s) = self.get(id) {
            s.admitted.store(false, Ordering::SeqCst);
        }
    }
    pub fn remove(&self, id: &DataSourceId) {
        self.close(id);
        self.sources.write().unwrap().remove(id);
    }
    pub fn invalidate(&self, id: &DataSourceId, stale: bool) -> Result<u64> {
        let s = self.get(id)?;
        s.stale.store(stale, Ordering::SeqCst);
        Ok(s.generation.fetch_add(1, Ordering::SeqCst) + 1)
    }
    pub fn check(&self, versions: &BTreeMap<DataSourceId, SourceVersion>) -> Result<()> {
        for (id, expected) in versions {
            let source = self.get(id)?;
            if source.version() != *expected {
                return Err(EngineError::new(
                    "0A000",
                    "source identity or catalog generation changed; Parse/Bind again",
                ));
            }
            if !source.admitted.load(Ordering::SeqCst) || source.stale.load(Ordering::SeqCst) {
                return Err(EngineError::new(
                    "55000",
                    "source is draining or metadata is stale; refresh it online",
                ));
            }
        }
        Ok(())
    }
}
pub struct DataSourceFactory {
    pub definitions: Vec<DataSourceDefinition>,
    pub managed: bool,
}
struct DataSourceInitiator {
    definitions: Vec<DataSourceDefinition>,
    managed: bool,
    registry: Option<Arc<DataSourceRegistry>>,
}
fn descriptor(managed: bool) -> ComponentDescriptor {
    let mut descriptor = ComponentDescriptor::new(ComponentRef::initiator("datasource-registry"));
    if managed {
        descriptor
            .requires
            .push(ComponentRef::initiator("duckdb-instance"));
    }
    descriptor
}
impl InitiatorFactory for DataSourceFactory {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn create(&self, _: &ComponentConfig) -> FrameworkResult<Box<dyn Initiator>> {
        Ok(Box::new(DataSourceInitiator {
            definitions: self.definitions.clone(),
            managed: self.managed,
            registry: None,
        }))
    }
}
#[async_trait]
impl Lifecycle for DataSourceInitiator {
    async fn setup(&mut self, ctx: &SetupContext) -> FrameworkResult<()> {
        if self.managed {
            let storage: ServiceHandle<ManagementStorage> = ctx
                .services
                .get(&ComponentRef::initiator("duckdb-instance"), "storage")?;
            // Validate persisted state before any runtime attachment; all admission starts closed.
            storage
                .acquire()?
                .port
                .read()
                .await
                .map_err(|e| FrameworkError::InvalidConfig(e.to_string()))?;
            self.registry = Some(Arc::new(DataSourceRegistry::new(&[])));
        } else {
            self.registry = Some(Arc::new(DataSourceRegistry::new(&self.definitions)));
        }
        Ok(())
    }
    async fn teardown(&mut self, _: &ShutdownContext) -> FrameworkResult<()> {
        self.registry.take();
        Ok(())
    }
}
impl Initiator for DataSourceInitiator {
    fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.managed)
    }
    fn export_services(&self, p: &mut ServicePublisher) -> FrameworkResult<()> {
        p.publish("registry", self.registry.as_ref().unwrap().clone())
    }
}
