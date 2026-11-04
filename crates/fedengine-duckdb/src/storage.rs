use crate::error::{engine_error, framework_error, fw};
use duckdb::{Connection, OptionalExt, appender_params_from_iter, params};
use fedengine_contract::*;
use fedengine_core::*;
use modorch::prelude::*;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

type SnapshotNames = Arc<BTreeSet<(String, String)>>;
// Persist bounded object facts independently from the directory's filter keys.
// Compression is prepared before publication, avoiding large JSON WAL/checkpoint work.
fn encode_object(id: &str, object: &CatalogObject) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    {
        use std::io::Write;
        let mut buffered = std::io::BufWriter::new(&mut encoder);
        serde_json::to_writer(&mut buffered, &(id, object)).map_err(|_| storage_error())?;
        buffered.flush().map_err(|_| storage_error())?;
    }
    encoder.finish().map_err(|_| storage_error())
}
fn decode_object(payload: &[u8]) -> Result<(String, CatalogObject)> {
    use std::io::Read;
    let reader = flate2::read::ZlibDecoder::new(payload).take(512 * 1024 * 1024 + 1);
    serde_json::from_reader(std::io::BufReader::new(reader)).map_err(|_| storage_error())
}

#[derive(Default)]
pub(super) struct CatalogViews {
    heads: Option<CatalogHeads>,
    names: VecDeque<(String, SnapshotNames)>,
    count: usize,
}
impl CatalogViews {
    fn names(&self, id: &str) -> Option<Arc<BTreeSet<(String, String)>>> {
        self.names
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, names)| names.clone())
    }
    fn insert(&mut self, id: String, names: Arc<BTreeSet<(String, String)>>) {
        if names.len() > 100_000 || self.names(&id).is_some() {
            return;
        }
        while self.names.len() >= 16 || self.count + names.len() > 100_000 {
            if let Some((_, old)) = self.names.pop_front() {
                self.count -= old.len();
            }
        }
        self.count += names.len();
        self.names.push_back((id, names));
    }
}
fn snapshot_ids(heads: &CatalogHeads) -> BTreeSet<String> {
    heads
        .sources
        .iter()
        .filter_map(|h| h.snapshot.as_ref().map(|s| s.id.clone()))
        .chain(
            heads
                .databases
                .iter()
                .flat_map(|d| d.schemas.iter().filter_map(|s| s.snapshot_id.clone())),
        )
        .collect()
}
fn relation_names(conn: &Connection, snapshot: &str) -> Result<BTreeSet<(String, String)>> {
    let mut stmt=conn.prepare("SELECT _schema,relname FROM _fedengine.meta_pg_class WHERE _snapshot_id=? AND relkind IN ('r','v','m','f','p') LIMIT 100001").map_err(engine_error)?;
    let mut rows = stmt.query([snapshot]).map_err(engine_error)?;
    let mut names = BTreeSet::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        names.insert((
            row.get(0).map_err(engine_error)?,
            row.get(1).map_err(engine_error)?,
        ));
    }
    if names.len() > 100000 {
        return Err(EngineError::new("54000", "catalog name budget exceeded"));
    }
    Ok(names)
}

pub(super) struct DuckdbStore {
    pub root: Arc<Mutex<Connection>>,
    pub tasks: TaskScheduler,
    pub writer: Arc<AsyncMutex<()>>,
    pub views: Arc<Mutex<CatalogViews>>,
}
#[async_trait]
impl ManagementStore for DuckdbStore {
    async fn prepare_publication(
        &self,
        incarnation: String,
        update: ManagementUpdate,
        ownership: Box<dyn Send>,
    ) -> Result<Box<dyn PreparedPublication>> {
        let writer = self.writer.clone().lock_owned().await;
        let root = self.root.clone();
        let tasks = self.tasks.clone();
        let started = std::time::Instant::now();
        let views = self.views.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                Ok((|| -> Result<Box<dyn PreparedPublication>> {
                    conn.execute_batch("BEGIN TRANSACTION")
                        .map_err(engine_error)?;
                    let result = prepare_changes(&conn, &incarnation, update);
                    if let Err(error) = result {
                        let _ = conn.execute_batch("ROLLBACK");
                        return Err(error);
                    }
                    tracing::debug!(
                        elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
                        "management publication preparation finished"
                    );
                    let heads = effective_heads(&conn)?;
                    let old_ids = views
                        .lock()
                        .unwrap()
                        .heads
                        .as_ref()
                        .map(snapshot_ids)
                        .unwrap_or_default();
                    let mut names = Vec::new();
                    // Only changed effective snapshots are warmed; never load other source facts.
                    let mut remaining = 100_000;
                    for id in snapshot_ids(&heads).difference(&old_ids) {
                        if remaining == 0 || names.len() >= 16 {
                            break;
                        }
                        let warmed = relation_names(&conn, id)?;
                        if warmed.len() <= remaining {
                            remaining -= warmed.len();
                            names.push((id.clone(), Arc::new(warmed)));
                        }
                    }
                    Ok(Box::new(PendingPublication {
                        heads,
                        names,
                        views,
                        conn,
                        tasks,
                        _writer: writer,
                        _ownership: ownership,
                    }))
                })())
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }

    async fn virtual_database_mutation(
        &self,
        mutation: VirtualDatabaseMutation,
        key: String,
        users: std::collections::BTreeSet<String>,
        ownership: Box<dyn Send>,
    ) -> Result<VirtualDatabaseReceipt> {
        // Keep lock order instance barrier -> writer -> transaction. Cancellation of the
        // waiter must never release the barrier while the blocking worker is committing.
        let permit = self.writer.clone().try_lock_owned().map_err(|_| {
            EngineError::new(
                "55P03",
                "metadata publication is preparing; retry with the same Idempotency-Key",
            )
        })?;
        let root = self.root.clone();
        let views = self.views.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let _ownership = ownership;
                let _writer = permit;
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                let result = crate::virtuals::mutate(&conn, mutation, key, users);
                if result.is_ok() {
                    views.lock().unwrap().heads = Some(read_headers(&conn).map_err(fw)?);
                }
                Ok(result)
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
    async fn relation_names(
        &self,
        snapshot: String,
    ) -> Result<Arc<std::collections::BTreeSet<(String, String)>>> {
        if let Some(names) = self.views.lock().unwrap().names(&snapshot) {
            return Ok(names);
        }
        let root = self.root.clone();
        let views = self.views.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                let names = Arc::new(relation_names(&conn, &snapshot).map_err(fw)?);
                views.lock().unwrap().insert(snapshot, names.clone());
                Ok(names)
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)
    }

    async fn catalog_headers(&self) -> Result<CatalogHeads> {
        if let Some(heads) = self.views.lock().unwrap().heads.clone() {
            return Ok(heads);
        }
        let views = self.views.clone();
        let root = self.root.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                let heads = read_headers(&conn).map_err(fw)?;
                // A concurrent committed update may already have filled the cache.
                let heads = views.lock().unwrap().heads.get_or_insert(heads).clone();
                Ok(Ok(heads))
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
    async fn directory(&self, request: DirectoryRequest) -> Result<CatalogPage> {
        let root = self.root.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                Ok(read_directory(&conn, &request))
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
    async fn read(&self) -> Result<ManagementState> {
        let root = self.root.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                Ok(local_read(&conn, read_heads))
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
    async fn update(&self, update: ManagementUpdate) -> Result<ManagementState> {
        let permit = self.writer.clone().lock_owned().await;
        let views = self.views.clone();
        let root = self.root.clone();
        self.tasks
            .spawn_blocking_io(move |_| {
                let _writer = permit;
                let conn = root
                    .lock()
                    .unwrap()
                    .try_clone()
                    .map_err(engine_error)
                    .map_err(fw)?;
                let result = metadata_transaction(&conn, update);
                if result.is_ok() {
                    views.lock().unwrap().heads = Some(read_headers(&conn).map_err(fw)?);
                }
                Ok(result)
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?
    }
}
fn read_headers(conn: &Connection) -> Result<CatalogHeads> {
    local_read(conn, effective_heads)
}
fn effective_heads(conn: &Connection) -> Result<CatalogHeads> {
    let mut stmt=conn.prepare("SELECT s.payload,r.payload,c.payload FROM _fedengine.data_sources s LEFT JOIN _fedengine.source_revisions r ON r.key=json_extract_string(s.payload,'$.incarnation')||':'||json_extract_string(s.payload,'$.applied_revision') LEFT JOIN _fedengine.catalog_snapshots c ON c.key=json_extract_string(s.payload,'$.catalog_snapshot') WHERE NOT CAST(json_extract(s.payload,'$.deleted') AS BOOLEAN) ORDER BY s.key").map_err(engine_error)?;
    let mut rows = stmt.query([]).map_err(engine_error)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(engine_error)? {
        fn decode<T: DeserializeOwned>(s: String) -> Result<T> {
            serde_json::from_str(&s).map_err(|_| storage_error())
        }
        out.push(CatalogHeader {
            source: decode(r.get(0).map_err(engine_error)?)?,
            applied: r
                .get::<_, Option<String>>(1)
                .map_err(engine_error)?
                .map(decode)
                .transpose()?,
            snapshot: r
                .get::<_, Option<String>>(2)
                .map_err(engine_error)?
                .map(decode)
                .transpose()?,
        });
    }
    Ok(CatalogHeads {
        sources: out,
        databases: crate::virtuals::read(conn)?,
    })
}

fn local_read<T>(conn: &Connection, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(engine_error)?;
    let result = read(conn);
    if result.is_ok() {
        conn.execute_batch("COMMIT").map_err(engine_error)?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn read_directory(conn: &Connection, r: &DirectoryRequest) -> Result<CatalogPage> {
    if r.limit == 0 || r.limit > 1000 {
        return Err(EngineError::config(
            "catalog limit must be between 1 and 1000",
        ));
    }
    local_read(conn, |conn| {
        let source:Option<String>=conn.query_row("SELECT payload FROM _fedengine.data_sources WHERE json_extract_string(payload,'$.id')=? AND NOT CAST(json_extract(payload,'$.deleted') AS BOOLEAN)",[&r.id.0],|row|row.get(0)).optional().map_err(engine_error)?;
        let source: ManagedSource = serde_json::from_str(
            &source.ok_or_else(|| EngineError::new("42704", "source does not exist"))?,
        )
        .map_err(|_| storage_error())?;
        let snapshot = source
            .catalog_snapshot
            .ok_or_else(|| EngineError::new("42704", "source has no catalog snapshot"))?;
        if r.snapshot_id.as_ref().is_some_and(|id| id != &snapshot) {
            return Err(EngineError::new(
                "40001",
                "catalog changed; restart pagination",
            ));
        }
        let payload: String = conn
            .query_row(
                "SELECT payload FROM _fedengine.catalog_snapshots WHERE key=?",
                [&snapshot],
                |row| row.get(0),
            )
            .map_err(engine_error)?;
        let catalog: CatalogSnapshot =
            serde_json::from_str(&payload).map_err(|_| storage_error())?;
        let filter = "snapshot_id=? AND (? IS NULL OR schema_name=?) AND (? IS NULL OR lower(object_type)=lower(?)) AND (? IS NULL OR object_name=?)";
        let total: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM _fedengine.catalog_objects WHERE {filter}"),
                params![
                    snapshot,
                    r.schema,
                    r.schema,
                    r.object_type,
                    r.object_type,
                    r.object,
                    r.object
                ],
                |row| row.get(0),
            )
            .map_err(engine_error)?;
        let mut stmt=conn.prepare(&format!("SELECT payload FROM _fedengine.catalog_objects WHERE {filter} ORDER BY key LIMIT ? OFFSET ?")).map_err(engine_error)?;
        let mut rows = stmt
            .query(params![
                snapshot,
                r.schema,
                r.schema,
                r.object_type,
                r.object_type,
                r.object,
                r.object,
                r.limit as i64,
                i64::try_from(r.offset)
                    .map_err(|_| EngineError::config("invalid catalog offset"))?
            ])
            .map_err(engine_error)?;
        let mut objects = Vec::new();
        while let Some(row) = rows.next().map_err(engine_error)? {
            let payload: Vec<u8> = row.get(0).map_err(engine_error)?;
            let (_, object) = decode_object(&payload)?;
            objects.push(object);
        }
        Ok(CatalogPage {
            snapshot_id: snapshot,
            incarnation: catalog.incarnation,
            revision: catalog.revision,
            generation: catalog.generation,
            refreshed_at: catalog.refreshed_at,
            stale: catalog.stale || source.status == SourceStatus::MetadataStale,
            schemas: catalog.schemas,
            total: total as usize,
            offset: r.offset,
            objects,
        })
    })
}
fn storage_error() -> EngineError {
    EngineError::new("XX000", "management storage is invalid or unavailable")
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| storage_error())
}
fn read_map<T: DeserializeOwned>(conn: &Connection, table: &str) -> Result<BTreeMap<String, T>> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT key, payload FROM _fedengine.{table} ORDER BY key"
        ))
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    let mut values = BTreeMap::new();
    while let Some(row) = rows.next().map_err(engine_error)? {
        let key: String = row.get(0).map_err(engine_error)?;
        let payload: String = row.get(1).map_err(engine_error)?;
        values.insert(
            key,
            serde_json::from_str(&payload).map_err(|_| storage_error())?,
        );
    }
    Ok(values)
}
pub(super) fn read_heads(conn: &Connection) -> Result<ManagementState> {
    Ok(ManagementState {
        sources: read_map(conn, "data_sources")?,
        revisions: read_map(conn, "source_revisions")?,
        operations: read_map(conn, "management_operations")?,
        catalogs: read_map(conn, "catalog_snapshots")?,
    })
}
fn visit_objects(
    conn: &Connection,
    snapshot: &CatalogSnapshot,
    visit: &mut crate::identity::ObjectVisitor<'_>,
) -> Result<()> {
    if !snapshot.objects.is_empty() {
        for object in &snapshot.objects {
            visit(object)?;
        }
        return Ok(());
    }
    let mut stmt = conn
        .prepare("SELECT payload FROM _fedengine.catalog_objects WHERE snapshot_id=? ORDER BY key")
        .map_err(engine_error)?;
    let mut rows = stmt.query([&snapshot.id]).map_err(engine_error)?;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let payload: Vec<u8> = row.get(0).map_err(engine_error)?;
        let (_, object) = decode_object(&payload)?;
        visit(&object)?;
    }
    Ok(())
}
/// Metadata-only updates cannot create relation payloads or rebuild projections.
fn metadata_transaction(conn: &Connection, update: ManagementUpdate) -> Result<ManagementState> {
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(engine_error)?;
    let result = (|| {
        let before = read_heads(conn)?;
        let mut after = before.clone();
        update(&mut after)?;
        if before
            .revisions
            .iter()
            .any(|(key, value)| after.revisions.get(key) != Some(value))
        {
            return Err(EngineError::config("source revision history is immutable"));
        }
        if after.catalogs.values().any(|c| !c.objects.is_empty())
            || after.catalogs.len() != before.catalogs.len()
            || after
                .catalogs
                .keys()
                .any(|id| !before.catalogs.contains_key(id))
        {
            return Err(EngineError::config(
                "catalog payload requires explicit publication",
            ));
        }
        validate_state(&after)?;
        write_map(conn, "data_sources", &before.sources, &after.sources)?;
        write_map(
            conn,
            "source_revisions",
            &before.revisions,
            &after.revisions,
        )?;
        write_map(
            conn,
            "schema_bindings",
            &bindings(&before),
            &bindings(&after),
        )?;
        write_map(
            conn,
            "management_operations",
            &before.operations,
            &after.operations,
        )?;
        write_map(conn, "catalog_snapshots", &before.catalogs, &after.catalogs)?;
        conn.execute_batch("COMMIT").map_err(engine_error)?;
        Ok(after)
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn prepare_changes(conn: &Connection, _incarnation: &str, update: ManagementUpdate) -> Result<()> {
    let _memory = crate::memory::Stage::new("local_preparation");
    let before = read_heads(conn)?;
    let mut after = before.clone();
    update(&mut after)?;
    crate::identity::prepare_with_previous(conn, &before, &mut after, |snapshot, visit| {
        visit_objects(conn, snapshot, visit)
    })?;
    reuse_unchanged_snapshots(conn, &before, &mut after)?;
    after.prune_catalogs(3);
    write_changes(conn, &before, &after)?;
    Ok(())
}
struct PendingPublication {
    heads: CatalogHeads,
    names: Vec<(String, SnapshotNames)>,
    views: Arc<Mutex<CatalogViews>>,
    conn: Connection,
    tasks: TaskScheduler,
    _writer: tokio::sync::OwnedMutexGuard<()>,
    _ownership: Box<dyn Send>,
}
#[async_trait]
impl PreparedPublication for PendingPublication {
    async fn commit(
        self: Box<Self>,
        ownership: Box<dyn Send>,
        published: Box<dyn FnOnce() + Send>,
    ) -> Result<()> {
        let tasks = self.tasks.clone();
        let barrier_started = std::time::Instant::now();
        let pending = tasks
            .spawn_blocking_io(move |_| {
                let _ownership = ownership;
                let pending = self;
                let started = std::time::Instant::now();
                let _memory = crate::memory::Stage::new("publication_commit");
                pending
                    .conn
                    .execute_batch("COMMIT")
                    .map_err(engine_error)
                    .map_err(fw)?;
                {
                    let mut views = pending.views.lock().unwrap();
                    views.heads = Some(pending.heads.clone());
                    for (id, names) in &pending.names {
                        views.insert(id.clone(), names.clone());
                    }
                }
                published();
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
                    "management local commit finished"
                );
                Ok(pending)
            })
            .map_err(framework_error)?
            .wait_finished()
            .await
            .map_err(framework_error)?;
        tracing::debug!(
            elapsed_ms = barrier_started.elapsed().as_secs_f64() * 1000.,
            "management instance publication barrier released"
        );
        // A distinct worker starts only after the commit worker has really exited
        // and released the instance barrier. Never FORCE CHECKPOINT or abort readers.
        let checkpoint = tasks.spawn_blocking_io(move |_| {
            let _memory = crate::memory::Stage::new("checkpoint");
            let started = std::time::Instant::now();
            if let Err(error) = pending.conn.execute_batch("CHECKPOINT") {
                tracing::warn!(%error, "post-publication checkpoint deferred; committed WAL is retained");
            }
            tracing::debug!(elapsed_ms=started.elapsed().as_secs_f64()*1000., "management post-publication checkpoint finished");
            drop(pending);
            Ok(())
        });
        // Publication is already committed. Shutdown or maintenance failure must
        // never relabel that confirmed outcome as an aborted publication.
        match checkpoint {
            Ok(job) => {
                if let Err(error) = job.wait_finished().await {
                    tracing::warn!(%error, "post-publication checkpoint worker failed; committed WAL is retained");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "post-publication checkpoint was not scheduled; committed WAL is retained")
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn read_state(conn: &Connection) -> Result<ManagementState> {
    let mut state = ManagementState {
        sources: read_map(conn, "data_sources")?,
        revisions: read_map(conn, "source_revisions")?,
        operations: read_map(conn, "management_operations")?,
        catalogs: read_map(conn, "catalog_snapshots")?,
    };
    let mut statement = conn
        .prepare("SELECT payload FROM _fedengine.catalog_objects ORDER BY key")
        .map_err(engine_error)?;
    let mut rows = statement.query([]).map_err(engine_error)?;
    while let Some(row) = rows.next().map_err(engine_error)? {
        let payload: Vec<u8> = row.get(0).map_err(engine_error)?;
        let (id, object) = decode_object(&payload)?;
        state
            .catalogs
            .get_mut(&id)
            .ok_or_else(storage_error)?
            .objects
            .push(object);
    }
    validate_state(&state)?;
    Ok(state)
}
fn write_map<T: Serialize + PartialEq>(
    conn: &Connection,
    table: &str,
    before: &BTreeMap<String, T>,
    after: &BTreeMap<String, T>,
) -> Result<()> {
    for (key, value) in after {
        if before.get(key) == Some(value) {
            continue;
        }
        conn.execute(
            &format!("INSERT OR REPLACE INTO _fedengine.{table} VALUES (?, ?)"),
            params![key, json(value)?],
        )
        .map_err(engine_error)?;
    }
    for key in before.keys().filter(|k| !after.contains_key(*k)) {
        conn.execute(
            &format!("DELETE FROM _fedengine.{table} WHERE key=?"),
            [key],
        )
        .map_err(engine_error)?;
    }
    Ok(())
}
fn write_catalogs(
    conn: &Connection,
    before: &ManagementState,
    after: &ManagementState,
) -> Result<()> {
    let old_heads = before
        .catalogs
        .iter()
        .map(|(id, c)| (id.clone(), c.header()))
        .collect();
    let heads = after
        .catalogs
        .iter()
        .map(|(id, c)| (id.clone(), c.header()))
        .collect();
    write_map(conn, "catalog_snapshots", &old_heads, &heads)?;
    for (id, snapshot) in &after.catalogs {
        if before
            .catalogs
            .get(id)
            .is_some_and(|old| old.objects == snapshot.objects)
        {
            continue;
        }
        // Existing untouched headers have no payload in source-scoped updates.
        if snapshot.objects.is_empty() && before.catalogs.contains_key(id) {
            continue;
        }
        conn.execute(
            "DELETE FROM _fedengine.catalog_objects WHERE starts_with(key,?)",
            [format!("{id}:")],
        )
        .map_err(engine_error)?;
        let mut app = conn
            .appender_to_db("catalog_objects", "_fedengine")
            .map_err(engine_error)?;
        for (index, object) in snapshot.objects.iter().enumerate() {
            use duckdb::types::Value as V;
            app.append_row(appender_params_from_iter([
                V::Text(format!("{id}:{index:010}")),
                V::Text(id.clone()),
                V::Text(object.schema.clone()),
                V::Text(object.name.clone()),
                V::Text(object.object_type.clone()),
                V::Blob(encode_object(id, object)?),
            ]))
            .map_err(engine_error)?;
        }
        app.flush().map_err(engine_error)?;
    }
    for id in before
        .catalogs
        .keys()
        .filter(|id| !after.catalogs.contains_key(*id))
    {
        conn.execute(
            "DELETE FROM _fedengine.catalog_objects WHERE starts_with(key,?)",
            [format!("{id}:")],
        )
        .map_err(engine_error)?;
    }
    Ok(())
}
fn bindings(state: &ManagementState) -> BTreeMap<String, SchemaBinding> {
    state
        .revisions
        .values()
        .flat_map(|r| {
            r.bindings.iter().map(move |b| {
                (
                    format!("{}:{}:{}", r.incarnation, r.revision, b.remote_schema),
                    b.clone(),
                )
            })
        })
        .collect()
}
fn validate_state(state: &ManagementState) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    let mut aliases = std::collections::BTreeSet::new();
    for (key, source) in &state.sources {
        if key != &source.incarnation
            || source.incarnation.is_empty()
            || source.desired_revision == 0
            || (!source.deleted
                && (!ids.insert(&source.id)
                    || !aliases.insert(source.catalog_alias.to_ascii_lowercase())))
        {
            return Err(storage_error());
        }
        for revision in [Some(source.desired_revision), source.applied_revision]
            .into_iter()
            .flatten()
        {
            let revision = state
                .revision(&source.incarnation, revision)
                .map_err(|_| storage_error())?;
            if revision.definition.id != source.id
                || revision.definition.kind != source.kind
                || revision.definition.catalog_alias != source.catalog_alias
            {
                return Err(storage_error());
            }
        }
        if let Some(operation) = &source.current_operation
            && state
                .operations
                .get(operation)
                .is_none_or(|o| o.incarnation != source.incarnation || o.status.finished())
        {
            return Err(storage_error());
        }
        if let Some(catalog) = &source.catalog_snapshot
            && state.catalogs.get(catalog).is_none_or(|c| {
                c.incarnation != source.incarnation || Some(c.revision) != source.applied_revision
            })
        {
            return Err(storage_error());
        }
    }
    for (key, revision) in &state.revisions {
        if *key != revision.key()
            || revision.revision == 0
            || !state.sources.contains_key(&revision.incarnation)
        {
            return Err(storage_error());
        }
        validate_source(&revision.effective_definition()).map_err(|_| storage_error())?;
        let mut bindings = std::collections::BTreeSet::new();
        if revision
            .bindings
            .iter()
            .any(|b| !allowed_namespace(&b.remote_schema) || !bindings.insert(&b.remote_schema))
        {
            return Err(storage_error());
        }
    }
    for (key, operation) in &state.operations {
        if key != &operation.id
            || state
                .sources
                .get(&operation.incarnation)
                .is_none_or(|s| s.id != *operation.action.id())
            || state
                .revision(&operation.incarnation, operation.target_revision)
                .is_err()
        {
            return Err(storage_error());
        }
    }
    for (key, catalog) in &state.catalogs {
        if key != &catalog.id
            || state
                .revision(&catalog.incarnation, catalog.revision)
                .is_err()
            || !catalog.complete
        {
            return Err(storage_error());
        }
    }
    Ok(())
}

fn write_changes(
    conn: &Connection,
    before: &ManagementState,
    after: &ManagementState,
) -> Result<()> {
    validate_state(after)?;
    for snapshot in after.catalogs.values() {
        let source = after
            .sources
            .get(&snapshot.incarnation)
            .ok_or_else(storage_error)?;
        crate::projection::publish(conn, snapshot, &source.catalog_alias)?;
    }
    crate::projection::prune(conn, before, after)?;
    crate::virtuals::publish(conn, after)?;
    if before
        .revisions
        .iter()
        .any(|(key, value)| after.revisions.get(key) != Some(value))
    {
        return Err(EngineError::config("source revision history is immutable"));
    }
    write_map(conn, "data_sources", &before.sources, &after.sources)?;
    write_map(
        conn,
        "source_revisions",
        &before.revisions,
        &after.revisions,
    )?;
    write_map(conn, "schema_bindings", &bindings(before), &bindings(after))?;
    write_map(
        conn,
        "management_operations",
        &before.operations,
        &after.operations,
    )?;
    write_catalogs(conn, before, after)?;
    Ok(())
}
fn reuse_unchanged_snapshots(
    conn: &Connection,
    before: &ManagementState,
    after: &mut ManagementState,
) -> Result<()> {
    let mut replacements = Vec::new();
    for new in after
        .catalogs
        .values()
        .filter(|c| !before.catalogs.contains_key(&c.id))
    {
        let Some(old) = before
            .catalogs
            .values()
            .filter(|c| c.incarnation == new.incarnation && c.revision == new.revision)
            .max_by_key(|c| &c.refreshed_at)
        else {
            continue;
        };
        if old.stale != new.stale
            || old.schemas != new.schemas
            || old.structure_digest != new.structure_digest
        {
            continue;
        }
        let mut position = 0;
        let mut equal = true;
        visit_objects(conn, old, &mut |old| {
            if let Some(new) = new.objects.get(position) {
                let mut a = old.clone();
                let mut b = new.clone();
                for object in [&mut a, &mut b] {
                    for status in object.metadata.status.values_mut() {
                        status.last_attempt_at = None;
                        status.last_success_at = None;
                    }
                }
                equal &= a == b;
            } else {
                equal = false;
            }
            position += 1;
            Ok(())
        })?;
        if equal && position == new.objects.len() {
            replacements.push((new.id.clone(), old.id.clone()));
        }
    }
    for (new_id, old_id) in replacements {
        let mut snapshot = after.catalogs.remove(&new_id).unwrap();
        snapshot.id = old_id.clone();
        after.catalogs.insert(old_id.clone(), snapshot);
        for source in after.sources.values_mut() {
            if source.catalog_snapshot.as_ref() == Some(&new_id) {
                source.catalog_snapshot = Some(old_id.clone());
            }
        }
    }
    Ok(())
}
pub(super) fn transaction(conn: &Connection, update: ManagementUpdate) -> Result<ManagementState> {
    let started = std::time::Instant::now();
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(engine_error)?;
    let result = (|| {
        let before = read_heads(conn)?;
        let mut after = before.clone();
        update(&mut after)?;
        crate::identity::prepare_with_previous(conn, &before, &mut after, |snapshot, visit| {
            visit_objects(conn, snapshot, visit)
        })?;
        reuse_unchanged_snapshots(conn, &before, &mut after)?;
        write_changes(conn, &before, &after)?;
        conn.execute_batch("COMMIT").map_err(engine_error)?;
        Ok(after)
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    tracing::debug!(
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
        success = result.is_ok(),
        "management local transaction finished"
    );
    result
}
/// Initialize the current schema and import configuration once. Older layouts
/// are deliberately unsupported; development does not need migration chains.
pub(super) fn initialize(conn: &Connection, config: &Config) -> Result<()> {
    conn.execute_batch("CREATE SCHEMA IF NOT EXISTS _fedengine; CREATE TABLE IF NOT EXISTS _fedengine.schema_migrations(version INTEGER PRIMARY KEY, applied_at VARCHAR NOT NULL)").map_err(engine_error)?;
    let version: i32 = conn
        .query_row(
            "SELECT coalesce(max(version),0) FROM _fedengine.schema_migrations",
            [],
            |r| r.get(0),
        )
        .map_err(engine_error)?;
    if version != 0 && version != 6 {
        return Err(EngineError::config(
            "management schema version does not match this program",
        ));
    }
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(engine_error)?;
    let result = (|| {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _fedengine.bootstrap_state(id INTEGER PRIMARY KEY, input_digest VARCHAR, completed_at VARCHAR)").map_err(engine_error)?;
        for table in [
            "data_sources",
            "source_revisions",
            "schema_bindings",
            "catalog_snapshots",
            "management_operations",
        ] {
            conn.execute_batch(&format!("CREATE TABLE IF NOT EXISTS _fedengine.{table}(key VARCHAR PRIMARY KEY, payload VARCHAR NOT NULL)")).map_err(engine_error)?;
        }
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _fedengine.catalog_objects(key VARCHAR PRIMARY KEY,snapshot_id VARCHAR NOT NULL,schema_name VARCHAR NOT NULL,object_name VARCHAR NOT NULL,object_type VARCHAR NOT NULL,payload BLOB NOT NULL)").map_err(engine_error)?;
        crate::identity::initialize(conn)?;
        crate::projection::initialize(conn)?;
        crate::virtuals::initialize(conn, config)?;
        if version == 0 {
            conn.execute(
                "INSERT INTO _fedengine.schema_migrations VALUES (6, ?)",
                [chrono::Utc::now().to_rfc3339()],
            )
            .map_err(engine_error)?;
        }
        let count: i64 = conn
            .query_row("SELECT count(*) FROM _fedengine.bootstrap_state", [], |r| {
                r.get(0)
            })
            .map_err(engine_error)?;
        let before = read_heads(conn)?;
        let mut after = before.clone();
        if count == 0 {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(json(&config.datasources)?.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            for definition in &config.datasources {
                let incarnation = uuid::Uuid::new_v4().to_string();
                let revision = SourceRevision {
                    incarnation: incarnation.clone(),
                    revision: 1,
                    definition: definition.clone(),
                    bindings: definition
                        .allowed_schemas
                        .iter()
                        .map(|s| SchemaBinding {
                            remote_schema: s.clone(),
                            enabled: true,
                        })
                        .collect(),
                };
                after.revisions.insert(revision.key(), revision);
                after.sources.insert(
                    incarnation.clone(),
                    ManagedSource {
                        id: definition.id.clone(),
                        incarnation,
                        kind: definition.kind,
                        catalog_alias: definition.catalog_alias.clone(),
                        desired_revision: 1,
                        applied_revision: None,
                        desired_enabled: definition.enabled,
                        status: if definition.enabled {
                            SourceStatus::Registered
                        } else {
                            SourceStatus::Disabled
                        },
                        generation: 0,
                        deleted: false,
                        current_operation: None,
                        last_error: None,
                        catalog_snapshot: None,
                    },
                );
            }
            conn.execute(
                "INSERT INTO _fedengine.bootstrap_state VALUES (1, ?, ?)",
                params![digest, chrono::Utc::now().to_rfc3339()],
            )
            .map_err(engine_error)?;
        } else if !config.datasources.is_empty() {
            tracing::info!(
                "persistent management state is authoritative; TOML sources were not imported again"
            );
        }
        for operation in after
            .operations
            .values_mut()
            .filter(|o| !o.status.finished())
        {
            operation.status = OperationStatus::Interrupted;
            operation.phase = "InterruptedByRestart".into();
            operation.error = Some(
                "process exited before operation completion; runtime will reconcile desired state"
                    .into(),
            );
            operation.updated_at = chrono::Utc::now().to_rfc3339();
        }
        for source in after.sources.values_mut() {
            source.current_operation = None;
        }
        crate::identity::prepare_with_previous(conn, &before, &mut after, |snapshot, visit| {
            visit_objects(conn, snapshot, visit)
        })?;
        reuse_unchanged_snapshots(conn, &before, &mut after)?;
        write_changes(conn, &before, &after)?;
        conn.execute_batch("COMMIT").map_err(engine_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_name_cache_shares_indexes_and_bounds_retained_entries() {
        let mut cache = CatalogViews::default();
        let large: SnapshotNames = Arc::new(
            (0..60_000)
                .map(|i| ("public".into(), format!("t_{i}")))
                .collect(),
        );
        cache.insert("old".into(), large.clone());
        assert!(Arc::ptr_eq(&cache.names("old").unwrap(), &large));
        cache.insert("new".into(), large);
        assert!(cache.names("old").is_none());
        assert!(cache.count <= 100_000);
        for i in 0..20 {
            cache.insert(format!("empty-{i}"), Arc::default());
        }
        assert_eq!(cache.names.len(), 16);
        assert_eq!(cache.count, 0);
    }
    #[test]
    fn prepared_projection_is_invisible_until_commit_and_drop_rolls_it_back() {
        let mut config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        config.datasources = vec![DataSourceDefinition {
            id: DataSourceId("source".into()),
            kind: DataSourceKind::Postgres,
            catalog_alias: "source".into(),
            secret_ref: "UNUSED".into(),
            default_schema: "public".into(),
            allowed_schemas: vec!["public".into()],
            enabled: false,
            read_only: true,
            ddl_enabled: false,
        }];
        let root = Connection::open_in_memory().unwrap();
        initialize(&root, &config).unwrap();
        let incarnation = read_heads(&root)
            .unwrap()
            .source(&DataSourceId("source".into()))
            .unwrap()
            .incarnation
            .clone();
        fn update(id: &str) -> ManagementUpdate {
            let id = id.to_owned();
            Box::new(move |state| {
                let source = state.sources.values_mut().next().unwrap();
                source.status = SourceStatus::Active;
                source.applied_revision = Some(1);
                source.catalog_snapshot = Some(id.clone());
                state.catalogs.insert(
                    id.clone(),
                    CatalogSnapshot {
                        structure_digest: id.clone(),
                        id,
                        incarnation: source.incarnation.clone(),
                        revision: 1,
                        generation: 1,
                        refreshed_at: "now".into(),
                        complete: true,
                        stale: false,
                        database_oid: 0,
                        schema_oids: Default::default(),
                        schemas: vec!["public".into()],
                        objects: vec![],
                    },
                );
                Ok(())
            })
        }
        let conn = root.try_clone().unwrap();
        conn.execute_batch("BEGIN TRANSACTION").unwrap();
        prepare_changes(&conn, &incarnation, update("published")).unwrap();
        assert!(read_heads(&root).unwrap().catalogs.is_empty());
        assert_eq!(root.query_row("SELECT count(*) FROM _fedengine.metadata_projection_state WHERE snapshot_id='published'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        conn.execute_batch("COMMIT").unwrap();
        drop(conn);
        let visible = read_heads(&root).unwrap();
        assert_eq!(
            visible.sources[&incarnation].catalog_snapshot.as_deref(),
            Some("published")
        );
        let conn = root.try_clone().unwrap();
        conn.execute_batch("BEGIN TRANSACTION").unwrap();
        prepare_changes(&conn, &incarnation, update("aborted")).unwrap();
        drop(conn);
        assert_eq!(read_heads(&root).unwrap(), visible);
        assert_eq!(root.query_row("SELECT count(*) FROM _fedengine.metadata_projection_state WHERE snapshot_id='aborted'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[test]
    fn metadata_only_update_never_reads_or_rewrites_relation_payloads() {
        let config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn, &config).unwrap();
        // Poisoned payload makes any accidental full-state read fail deterministically.
        conn.execute(
            "INSERT INTO _fedengine.catalog_objects VALUES ('unrelated','unrelated','public','poison','BASE TABLE','not compressed'::BLOB)",
            [],
        )
        .unwrap();
        assert!(read_state(&conn).is_err());
        metadata_transaction(&conn, Box::new(|_| Ok(()))).unwrap();
        transaction(&conn, Box::new(|_| Ok(()))).unwrap();
        // Startup and virtual-database/control updates also use headers only.
        initialize(&conn, &config).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT payload FROM _fedengine.catalog_objects WHERE key='unrelated'",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            b"not compressed"
        );
    }
    #[test]
    fn bootstrap_is_once_transactions_rollback_and_other_versions_are_refused() {
        let mut config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/application.toml"),
        )
        .unwrap();
        config.datasources.clear();
        let connection = Connection::open_in_memory().unwrap();
        initialize(&connection, &config).unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM _fedengine.bootstrap_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        // Restart initializes the same current layout without importing twice.
        initialize(&connection, &config).unwrap();
        let version: i32 = connection
            .query_row(
                "SELECT max(version) FROM _fedengine.schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, 6);
        let source = DataSourceDefinition {
            id: DataSourceId("must_not_reappear".into()),
            kind: DataSourceKind::Postgres,
            catalog_alias: "pg_test".into(),
            secret_ref: "TEST_DSN".into(),
            default_schema: "public".into(),
            read_only: true,
            ddl_enabled: false,
            allowed_schemas: vec!["public".into()],
            enabled: true,
        };
        config.datasources.push(source);
        initialize(&connection, &config).unwrap();
        assert!(read_state(&connection).unwrap().sources.is_empty());
        let before = read_state(&connection).unwrap();
        let failure = transaction(
            &connection,
            Box::new(|state| {
                state.catalogs.insert(
                    "incomplete".into(),
                    CatalogSnapshot {
                        database_oid: 0,
                        schema_oids: BTreeMap::new(),
                        id: "incomplete".into(),
                        incarnation: "absent".into(),
                        revision: 1,
                        generation: 1,
                        refreshed_at: "now".into(),
                        complete: false,
                        stale: true,
                        structure_digest: "partial".into(),
                        schemas: vec![],
                        objects: vec![],
                    },
                );
                Err(EngineError::config("injected transaction failure"))
            }),
        );
        assert!(failure.is_err());
        assert_eq!(read_state(&connection).unwrap(), before);
        for version in [1, 4, 5, 99] {
            connection.execute_batch(&format!("DELETE FROM _fedengine.schema_migrations; INSERT INTO _fedengine.schema_migrations VALUES ({version}, 'unsupported')")).unwrap();
            assert!(
                initialize(&connection, &config)
                    .unwrap_err()
                    .message
                    .contains("does not match")
            );
        }
    }
}
