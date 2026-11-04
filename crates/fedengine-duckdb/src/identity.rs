//! Durable local identities. Called inside the management writer's transaction only.
use crate::error::engine_error;
use duckdb::{Connection, OptionalExt, appender_params_from_iter, params};
use fedengine_core::*;
use std::collections::{BTreeSet, HashMap};

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE SEQUENCE IF NOT EXISTS _fedengine.metadata_oid_seq START 200000000 MAXVALUE 2147483647 NO CYCLE;
        CREATE TABLE IF NOT EXISTS _fedengine.metadata_identity_epochs(revision_key VARCHAR PRIMARY KEY, epoch VARCHAR NOT NULL);
        CREATE TABLE IF NOT EXISTS _fedengine.object_identity(identity_key VARCHAR PRIMARY KEY, epoch VARCHAR NOT NULL, oid UINTEGER NOT NULL UNIQUE, retired BOOLEAN NOT NULL);
        CREATE TABLE IF NOT EXISTS _fedengine.column_identity(table_oid UINTEGER NOT NULL, name VARCHAR NOT NULL, attnum SMALLINT NOT NULL, source_id VARCHAR, retired BOOLEAN NOT NULL, PRIMARY KEY(table_oid,attnum));")
        .map_err(engine_error)
}
pub(super) fn oid(conn: &Connection, epoch: &str, parts: &[&str]) -> Result<u32> {
    let key = serde_json::to_string(&(epoch, parts))
        .map_err(|_| EngineError::new("XX000", "invalid identity"))?;
    let existing: Option<(u32, bool)> = conn
        .query_row(
            "SELECT oid,retired FROM _fedengine.object_identity WHERE identity_key=?",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(engine_error)?;
    if let Some((oid, false)) = existing {
        return Ok(oid);
    }
    let next: u32 = conn
        .query_row("SELECT nextval('_fedengine.metadata_oid_seq')", [], |r| {
            r.get(0)
        })
        .map_err(engine_error)?;
    conn.execute("INSERT INTO _fedengine.object_identity VALUES (?,?,?,false) ON CONFLICT(identity_key) DO UPDATE SET oid=excluded.oid,retired=false", params![key,epoch,next]).map_err(engine_error)?;
    Ok(next)
}
/// Source-scoped identity working set; all changes remain in the caller's transaction.
pub(super) struct OidBatch<'a> {
    conn: &'a Connection,
    known: HashMap<String, (u32, bool)>,
    pending: Vec<(String, String, u32)>,
    reserved: Vec<u32>,
}
impl<'a> OidBatch<'a> {
    pub(super) fn load(conn: &'a Connection, epochs: &[String]) -> Result<Self> {
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_identity_epochs(epoch VARCHAR)")
            .map_err(engine_error)?;
        {
            let mut app = conn
                .appender_to_db("__fed_identity_epochs", "main")
                .map_err(engine_error)?;
            for epoch in epochs {
                app.append_row([epoch]).map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        let mut stmt = conn.prepare("SELECT identity_key,oid,retired FROM _fedengine.object_identity WHERE epoch IN (SELECT epoch FROM __fed_identity_epochs) OR epoch IN (SELECT 'default:'||oid FROM _fedengine.object_identity WHERE epoch IN (SELECT epoch FROM __fed_identity_epochs)) OR epoch IN (SELECT 'type:'||oid FROM _fedengine.object_identity WHERE epoch IN (SELECT epoch FROM __fed_identity_epochs))").map_err(engine_error)?;
        let mut rows = stmt.query([]).map_err(engine_error)?;
        let mut known = HashMap::new();
        while let Some(row) = rows.next().map_err(engine_error)? {
            known.insert(
                row.get(0).map_err(engine_error)?,
                (
                    row.get(1).map_err(engine_error)?,
                    row.get(2).map_err(engine_error)?,
                ),
            );
        }
        Ok(Self {
            conn,
            known,
            pending: Vec::new(),
            reserved: Vec::new(),
        })
    }
    pub(super) fn oid(&mut self, epoch: &str, parts: &[&str]) -> Result<u32> {
        let key = serde_json::to_string(&(epoch, parts))
            .map_err(|_| EngineError::new("XX000", "invalid identity"))?;
        if let Some((oid, false)) = self.known.get(&key) {
            return Ok(*oid);
        }
        if self.reserved.is_empty() {
            // Reserve bounded batches. Sequence gaps after rollback are intentional; never recycle.
            let mut stmt = self
                .conn
                .prepare("SELECT nextval('_fedengine.metadata_oid_seq')::UINTEGER FROM range(1024)")
                .map_err(engine_error)?;
            self.reserved = stmt
                .query_map([], |r| r.get(0))
                .map_err(engine_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(engine_error)?;
        }
        let oid = self.reserved.pop().unwrap();
        self.known.insert(key.clone(), (oid, false));
        self.pending.push((key, epoch.into(), oid));
        Ok(oid)
    }
    pub(super) fn flush(self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_identity_new(identity_key VARCHAR,epoch VARCHAR,oid UINTEGER)").map_err(engine_error)?;
        {
            let mut app = self
                .conn
                .appender_to_db("__fed_identity_new", "main")
                .map_err(engine_error)?;
            for (key, epoch, oid) in self.pending {
                use duckdb::types::Value as V;
                app.append_row(appender_params_from_iter([
                    V::Text(key),
                    V::Text(epoch),
                    V::UInt(oid),
                ]))
                .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        self.conn.execute_batch("INSERT INTO _fedengine.object_identity SELECT identity_key,epoch,oid,false FROM __fed_identity_new ON CONFLICT(identity_key) DO UPDATE SET oid=excluded.oid,retired=false; DROP TABLE __fed_identity_new").map_err(engine_error)
    }
}
#[derive(Default)]
struct ColumnHistory {
    entries: Vec<(String, i16, Option<String>, bool)>,
    max: i32,
}
struct ColumnBatch {
    histories: HashMap<u32, ColumnHistory>,
    changed: Vec<(u32, String, i16, Option<String>, bool)>,
}
impl ColumnBatch {
    fn load(conn: &Connection, tables: impl Iterator<Item = u32>) -> Result<Self> {
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_identity_tables(oid UINTEGER)")
            .map_err(engine_error)?;
        {
            let mut app = conn
                .appender_to_db("__fed_identity_tables", "main")
                .map_err(engine_error)?;
            for oid in tables {
                app.append_row([oid]).map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        let mut stmt=conn.prepare("SELECT table_oid,name,attnum,source_id,retired FROM _fedengine.column_identity WHERE table_oid IN (SELECT oid FROM __fed_identity_tables)").map_err(engine_error)?;
        let mut rows = stmt.query([]).map_err(engine_error)?;
        let mut histories: HashMap<u32, ColumnHistory> = HashMap::new();
        while let Some(r) = rows.next().map_err(engine_error)? {
            let h = histories
                .entry(r.get(0).map_err(engine_error)?)
                .or_default();
            let num: i16 = r.get(2).map_err(engine_error)?;
            h.max = h.max.max(num.into());
            h.entries.push((
                r.get(1).map_err(engine_error)?,
                num,
                r.get(3).map_err(engine_error)?,
                r.get(4).map_err(engine_error)?,
            ));
        }
        Ok(Self {
            histories,
            changed: Vec::new(),
        })
    }
    fn assign(&mut self, oids: &mut OidBatch<'_>, object: &mut CatalogObject) -> Result<()> {
        let table = object.metadata.oid;
        let history = self.histories.entry(table).or_default();
        let names: HashMap<_, _> = history
            .entries
            .iter()
            .filter(|e| !e.3)
            .map(|e| (e.0.as_str(), e))
            .collect();
        let sources: HashMap<_, _> = history
            .entries
            .iter()
            .filter(|e| !e.3)
            .filter_map(|e| e.2.as_deref().map(|s| (s, e)))
            .collect();
        let mut keep = BTreeSet::new();
        for col in &object.columns {
            let props = object.metadata.columns.entry(col.name.clone()).or_default();
            // Source identity wins. Name fallback is only allowed when either side lacks it.
            let matched = props
                .source_id
                .as_deref()
                .and_then(|s| sources.get(s).copied())
                .or_else(|| {
                    names
                        .get(col.name.as_str())
                        .copied()
                        .filter(|e| props.source_id.is_none() || e.2.is_none())
                });
            let num = if let Some(e) = matched {
                e.1
            } else {
                history.max += 1;
                i16::try_from(history.max)
                    .map_err(|_| EngineError::new("54000", "column identity space exhausted"))?
            };
            if !keep.insert(num) {
                return Err(EngineError::new("55000", "duplicate column identity"));
            }
            props.attnum = num;
            if props.default.is_some() || props.generated.is_some() {
                props.default_oid = oids.oid(&format!("default:{table}"), &[&num.to_string()])?;
            }
            props.type_oid = match col
                .mapped_type
                .as_ref()
                .and_then(|t| crate::codec::type_name(t).ok())
            {
                Some(t) => t.oid(),
                None => oids.oid(
                    &format!("type:{table}"),
                    &["unsupported_type", &col.remote_type],
                )?,
            };
            props.type_modifier = col
                .mapped_type
                .as_ref()
                .map(|t| crate::codec::type_modifier(t))
                .unwrap_or(-1);
            if matched.is_none_or(|e| e.0 != col.name || e.2 != props.source_id) {
                self.changed
                    .push((table, col.name.clone(), num, props.source_id.clone(), false));
            }
        }
        object.metadata.dropped_attnums.clear();
        for e in &history.entries {
            if !keep.contains(&e.1) {
                object.metadata.dropped_attnums.push(e.1);
                if !e.3 {
                    self.changed
                        .push((table, e.0.clone(), e.1, e.2.clone(), true));
                }
            }
        }
        object.metadata.dropped_attnums.sort_unstable();
        Ok(())
    }
    fn flush(self, conn: &Connection) -> Result<()> {
        if self.changed.is_empty() {
            return Ok(());
        }
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_columns_new(table_oid UINTEGER,name VARCHAR,attnum SMALLINT,source_id VARCHAR,retired BOOLEAN)").map_err(engine_error)?;
        {
            let mut app = conn
                .appender_to_db("__fed_columns_new", "main")
                .map_err(engine_error)?;
            for (oid, name, num, source, retired) in self.changed {
                use duckdb::types::Value as V;
                app.append_row(appender_params_from_iter([
                    V::UInt(oid),
                    V::Text(name),
                    V::SmallInt(num),
                    source.map(V::Text).unwrap_or(V::Null),
                    V::Boolean(retired),
                ]))
                .map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        conn.execute_batch("INSERT OR REPLACE INTO _fedengine.column_identity SELECT * FROM __fed_columns_new; DROP TABLE __fed_columns_new").map_err(engine_error)
    }
}
fn retained_categories(old: &CatalogObject, new: &mut CatalogObject) {
    let complete = |category: &str| {
        new.metadata.status.get(category).is_some_and(|s| {
            s.complete
                && matches!(
                    s.scan_state,
                    MetadataScanState::Success | MetadataScanState::Partial
                )
        })
    };
    let (constraints, indexes, columns) = (
        complete("constraints"),
        complete("indexes"),
        complete("column_properties"),
    );
    let details = complete("details");
    if !complete("statistics") {
        new.metadata.statistics = old.metadata.statistics.clone();
    }
    if !complete("storage") {
        new.metadata.storage = old.metadata.storage.clone();
    }
    if !complete("activity") {
        new.metadata.activity = old.metadata.activity.clone();
    }
    if !complete("column_statistics") {
        new.metadata.column_statistics = old.metadata.column_statistics.clone();
        new.metadata
            .column_statistics
            .retain(|name, _| new.columns.iter().any(|c| c.name == *name));
    }
    if !details {
        new.metadata.comment = old.metadata.comment.clone();
        new.metadata.view_definition = old.metadata.view_definition.clone();
        new.metadata.view_dependencies = old.metadata.view_dependencies.clone();
    }
    if !constraints {
        new.metadata.constraints = old.metadata.constraints.clone();
    }
    if !indexes {
        new.metadata.indexes = old.metadata.indexes.clone();
    }
    if !columns {
        for col in &new.columns {
            if let Some(props) = old.metadata.columns.get(&col.name) {
                new.metadata.columns.insert(col.name.clone(), props.clone());
            }
        }
    }
    for (category, status) in &mut new.metadata.status {
        if !status.complete {
            status.last_success_at = old
                .metadata
                .status
                .get(category)
                .and_then(|s| s.last_success_at.clone());
        }
    }
    // A changed core can make retained extension facts invalid. Never publish dangling keys.
    let names: BTreeSet<_> = new.columns.iter().map(|c| c.name.as_str()).collect();
    new.metadata
        .constraints
        .retain(|c| c.columns.iter().all(|n| names.contains(n.as_str())));
    new.metadata.indexes.retain(|i| {
        i.keys
            .iter()
            .filter_map(|k| k.column.as_ref())
            .chain(i.include.iter())
            .all(|n| names.contains(n.as_str()))
    });
}
fn identity_epoch(
    conn: &Connection,
    before: &ManagementState,
    after: &ManagementState,
    snapshot: &CatalogSnapshot,
) -> Result<String> {
    let key = revision_key(&snapshot.incarnation, snapshot.revision);
    if let Some(epoch) = conn
        .query_row(
            "SELECT epoch FROM _fedengine.metadata_identity_epochs WHERE revision_key=?",
            [&key],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(engine_error)?
    {
        return Ok(epoch);
    }
    let mut epoch = key.clone();
    if let (Some(current), Some(old)) = (
        after.revisions.get(&key),
        before
            .sources
            .get(&snapshot.incarnation)
            .and_then(|s| s.applied_revision)
            .and_then(|r| {
                before
                    .revisions
                    .get(&revision_key(&snapshot.incarnation, r))
            }),
    ) {
        // Binding/default-schema edits have no connection-target effect. Reapplying identical
        // credentials may rotate their contents, so it deliberately starts a new identity epoch.
        let mut a = current.definition.clone();
        let mut b = old.definition.clone();
        let bindings_changed = current.bindings != old.bindings
            || a.allowed_schemas != b.allowed_schemas
            || a.default_schema != b.default_schema;
        a.allowed_schemas.clear();
        b.allowed_schemas.clear();
        a.default_schema.clear();
        b.default_schema.clear();
        if a == b && bindings_changed {
            epoch = conn
                .query_row(
                    "SELECT epoch FROM _fedengine.metadata_identity_epochs WHERE revision_key=?",
                    [old.key()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(engine_error)?
                .unwrap_or_else(|| old.key());
        }
    }
    conn.execute(
        "INSERT INTO _fedengine.metadata_identity_epochs VALUES (?,?)",
        params![key, epoch],
    )
    .map_err(engine_error)?;
    Ok(epoch)
}
#[cfg(test)]
pub(super) fn prepare(
    conn: &Connection,
    before: &ManagementState,
    after: &mut ManagementState,
) -> Result<()> {
    prepare_with_previous(conn, before, after, |snapshot, visit| {
        for object in &snapshot.objects {
            visit(object)?;
        }
        Ok(())
    })
}
pub(super) type ObjectVisitor<'a> = dyn FnMut(&CatalogObject) -> Result<()> + 'a;
pub(super) fn prepare_with_previous(
    conn: &Connection,
    before: &ManagementState,
    after: &mut ManagementState,
    visit: impl Fn(&CatalogSnapshot, &mut ObjectVisitor<'_>) -> Result<()>,
) -> Result<()> {
    let epochs = after
        .catalogs
        .values()
        .filter(|c| c.database_oid == 0 && c.complete)
        .map(|c| Ok((c.id.clone(), identity_epoch(conn, before, after, c)?)))
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    let old_epochs = before
        .catalogs
        .values()
        .map(|c| {
            let epoch = conn
                .query_row(
                    "SELECT epoch FROM _fedengine.metadata_identity_epochs WHERE revision_key=?",
                    [revision_key(&c.incarnation, c.revision)],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(engine_error)?;
            Ok((c.id.clone(), epoch))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    for snapshot in after
        .catalogs
        .values_mut()
        .filter(|c| c.database_oid == 0 && c.complete)
    {
        let _memory = crate::memory::Stage::new("identity_assignment");
        let started = std::time::Instant::now();
        let epoch = epochs[&snapshot.id].clone();
        let previous = before
            .catalogs
            .values()
            .filter(|c| {
                c.incarnation == snapshot.incarnation
                    && (c.revision == snapshot.revision
                        || old_epochs.get(&c.id).and_then(Option::as_ref) == Some(&epoch))
            })
            .max_by_key(|c| &c.refreshed_at);
        let mut oids = OidBatch::load(conn, &[epoch.clone(), snapshot.incarnation.clone()])?;
        snapshot.database_oid = oids.oid(&snapshot.incarnation, &["database"])?;
        for object in &mut snapshot.objects {
            let key = object.metadata.source_id.as_deref().unwrap_or(&object.name);
            object.metadata.oid = if object.metadata.source_id.is_some() {
                oids.oid(&epoch, &["relation", "remote", key])?
            } else {
                oids.oid(&epoch, &["relation", "name", &object.schema, key])?
            };
        }
        let positions: HashMap<_, _> = snapshot
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.metadata.oid, i))
            .collect();
        let mut previous_ids = Vec::new();
        if let Some(previous) = previous {
            visit(previous, &mut |old| {
                if let Some(&position) = positions.get(&old.metadata.oid) {
                    retained_categories(old, &mut snapshot.objects[position]);
                }
                if snapshot.schemas.contains(&old.schema) {
                    previous_ids.extend(
                        std::iter::once(old.metadata.oid)
                            .chain(old.metadata.constraints.iter().map(|c| c.oid))
                            .chain(old.metadata.indexes.iter().map(|i| i.oid)),
                    );
                }
                Ok(())
            })?;
        }
        let mut columns = ColumnBatch::load(conn, snapshot.objects.iter().map(|o| o.metadata.oid))?;
        let mut active = BTreeSet::new();
        for schema in &snapshot.schemas {
            let id = oids.oid(&epoch, &["schema", schema])?;
            snapshot.schema_oids.insert(schema.clone(), id);
            active.insert(id);
        }
        for object in &mut snapshot.objects {
            active.insert(object.metadata.oid);
            columns.assign(&mut oids, object)?;
            object.metadata.validate(&object.columns)?;
            for c in &mut object.metadata.constraints {
                let key = c.source_id.as_deref().unwrap_or(&c.name);
                c.oid = oids.oid(
                    &epoch,
                    &["constraint", &object.metadata.oid.to_string(), key],
                )?;
                active.insert(c.oid);
            }
            for i in &mut object.metadata.indexes {
                let key = i.source_id.as_deref().unwrap_or(&i.name);
                i.oid = oids.oid(&epoch, &["index", &object.metadata.oid.to_string(), key])?;
                active.insert(i.oid);
            }
        }
        oids.flush()?;
        columns.flush(conn)?;
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE __fed_retired(oid UINTEGER)")
            .map_err(engine_error)?;
        {
            let mut app = conn
                .appender_to_db("__fed_retired", "main")
                .map_err(engine_error)?;
            for id in previous_ids.into_iter().filter(|id| !active.contains(id)) {
                app.append_row([id]).map_err(engine_error)?;
            }
            app.flush().map_err(engine_error)?;
        }
        conn.execute_batch("UPDATE _fedengine.object_identity SET retired=true WHERE oid IN (SELECT oid FROM __fed_retired); DROP TABLE __fed_retired").map_err(engine_error)?;
        tracing::debug!(snapshot=%snapshot.id, objects=snapshot.objects.len(), elapsed_ms=started.elapsed().as_secs_f64()*1000., "catalog identity assignment finished");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(id: &str, names: &[&str]) -> CatalogSnapshot {
        let mut metadata = ObjectMetadata::default();
        for category in [
            "column_properties",
            "constraints",
            "indexes",
            "details",
            "statistics",
            "storage",
            "activity",
            "column_statistics",
        ] {
            metadata
                .status
                .insert(category.into(), MetadataStatus::success());
        }
        CatalogSnapshot {
            id: id.into(),
            incarnation: "lifetime".into(),
            revision: 1,
            generation: 1,
            refreshed_at: id.into(),
            complete: true,
            stale: false,
            structure_digest: "core".into(),
            database_oid: 0,
            schema_oids: Default::default(),
            schemas: vec!["public".into()],
            objects: vec![CatalogObject {
                schema: "public".into(),
                name: "orders".into(),
                object_type: "BASE TABLE".into(),
                indexes: vec![],
                metadata,
                columns: names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| CatalogColumn {
                        name: (*n).into(),
                        ordinal: (i + 1) as u32,
                        remote_type: "integer".into(),
                        mapped_type: Some("INTEGER".into()),
                        nullable: true,
                        supported: true,
                    })
                    .collect(),
            }],
        }
    }
    fn publish(
        conn: &Connection,
        before: &ManagementState,
        snapshot: CatalogSnapshot,
    ) -> ManagementState {
        let mut next = before.clone();
        next.catalogs.insert(snapshot.id.clone(), snapshot);
        prepare(conn, before, &mut next).unwrap();
        next
    }
    #[test]
    fn durable_identities_preserve_survivors_and_never_reuse_dropped_slots() {
        let path = std::env::temp_dir().join(format!(
            "fedengine-identity-{}.duckdb",
            uuid::Uuid::new_v4()
        ));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        let one = publish(
            &conn,
            &ManagementState::default(),
            snapshot("1", &["id", "old"]),
        );
        let oid = one.catalogs["1"].objects[0].metadata.oid;
        let two = publish(&conn, &one, snapshot("2", &["id"]));
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        initialize(&conn).unwrap();
        let three = publish(&conn, &two, snapshot("3", &["id", "old"]));
        let object = &three.catalogs["3"].objects[0];
        assert_eq!(object.metadata.oid, oid);
        assert_eq!(object.metadata.columns["id"].attnum, 1);
        assert_eq!(object.metadata.columns["old"].attnum, 3);
        let mut empty = snapshot("4", &[]);
        empty.objects.clear();
        let four = publish(&conn, &three, empty);
        let five = publish(&conn, &four, snapshot("5", &["id"]));
        assert_ne!(five.catalogs["5"].objects[0].metadata.oid, oid);
        let mut recreated = snapshot("6", &["id"]);
        recreated.incarnation = "new-lifetime".into();
        let six = publish(&conn, &five, recreated);
        assert_ne!(
            six.catalogs["6"].database_oid,
            one.catalogs["1"].database_oid
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn incomplete_scan_retains_facts_but_complete_empty_scan_retires_them() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        let mut first = snapshot("1", &["id"]);
        first.objects[0].metadata.statistics.estimated_rows = Some(123.into());
        first.objects[0].metadata.storage.total_bytes = Some(8192);
        first.objects[0].metadata.activity.seq_scan = Some(9);
        first.objects[0].metadata.column_statistics.insert(
            "id".into(),
            ColumnStatistics {
                null_frac: Some(0.into()),
                avg_width: Some(4),
                n_distinct: Some((-1).into()),
            },
        );
        first.objects[0].metadata.indexes.push(MetadataIndex {
            name: "orders_idx".into(),
            storage: RelationStorage {
                main_bytes: Some(4096),
                ..Default::default()
            },
            activity: IndexActivity {
                idx_scan: Some(7),
                ..Default::default()
            },
            ..Default::default()
        });
        first.objects[0]
            .metadata
            .constraints
            .push(MetadataConstraint {
                name: "orders_pkey".into(),
                kind: "p".into(),
                columns: vec!["id".into()],
                ..Default::default()
            });
        let one = publish(&conn, &ManagementState::default(), first);
        let constraint = one.catalogs["1"].objects[0].metadata.constraints[0].oid;
        let mut failed = snapshot("2", &["id"]);
        for category in ["storage", "activity", "column_statistics", "indexes"] {
            failed.objects[0].metadata.status.insert(
                category.into(),
                MetadataStatus::failure(&EngineError::denied("denied")),
            );
        }
        failed.objects[0].metadata.status.insert(
            "statistics".into(),
            MetadataStatus::failure(&EngineError::denied("denied")),
        );
        failed.objects[0].metadata.status.insert(
            "constraints".into(),
            MetadataStatus::failure(&EngineError::denied("denied")),
        );
        let two = publish(&conn, &one, failed);
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.storage.total_bytes,
            Some(8192)
        );
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.activity.seq_scan,
            Some(9)
        );
        for category in ["storage", "activity", "column_statistics", "indexes"] {
            let status = &two.catalogs["2"].objects[0].metadata.status[category];
            assert!(status.stale);
            assert_eq!(
                status.last_success_at,
                one.catalogs["1"].objects[0].metadata.status[category].last_success_at
            );
        }
        assert_eq!(
            two.catalogs["2"].objects[0]
                .metadata
                .statistics
                .estimated_rows,
            Some(123.into())
        );
        assert!(two.catalogs["2"].objects[0].metadata.status["statistics"].stale);
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.constraints[0].oid,
            constraint
        );
        assert!(two.catalogs["2"].objects[0].metadata.status["constraints"].stale);
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.column_statistics["id"].avg_width,
            Some(4)
        );
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.indexes[0]
                .storage
                .main_bytes,
            Some(4096)
        );
        assert_eq!(
            two.catalogs["2"].objects[0].metadata.indexes[0]
                .activity
                .idx_scan,
            Some(7)
        );
        let three = publish(&conn, &two, snapshot("3", &["id"]));
        assert_eq!(
            three.catalogs["3"].objects[0].metadata.storage.total_bytes,
            None
        );
        assert_eq!(
            three.catalogs["3"].objects[0].metadata.activity.seq_scan,
            None
        );
        assert_eq!(
            three.catalogs["3"].objects[0]
                .metadata
                .statistics
                .estimated_rows,
            None
        );
        assert!(
            three.catalogs["3"].objects[0]
                .metadata
                .constraints
                .is_empty()
        );
        assert!(
            three.catalogs["3"].objects[0]
                .metadata
                .column_statistics
                .is_empty()
        );
        assert!(three.catalogs["3"].objects[0].metadata.indexes.is_empty());
        let retired: bool = conn
            .query_row(
                "SELECT retired FROM _fedengine.object_identity WHERE oid=?",
                [constraint],
                |r| r.get(0),
            )
            .unwrap();
        assert!(retired);
    }
    #[test]
    fn binding_edits_preserve_epoch_but_credential_reapplication_does_not() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE SCHEMA _fedengine").unwrap();
        initialize(&conn).unwrap();
        let definition = DataSourceDefinition {
            id: DataSourceId("crm".into()),
            kind: DataSourceKind::Postgres,
            catalog_alias: "pg_crm".into(),
            secret_ref: "test_dsn".into(),
            default_schema: "public".into(),
            allowed_schemas: vec!["public".into()],
            enabled: true,
            read_only: true,
            ddl_enabled: false,
        };
        let revision = SourceRevision {
            incarnation: "lifetime".into(),
            revision: 1,
            definition,
            bindings: vec![SchemaBinding {
                remote_schema: "public".into(),
                enabled: true,
            }],
        };
        let mut before = ManagementState::default();
        before.revisions.insert(revision.key(), revision.clone());
        before.sources.insert(
            "lifetime".into(),
            ManagedSource {
                id: DataSourceId("crm".into()),
                incarnation: "lifetime".into(),
                kind: DataSourceKind::Postgres,
                catalog_alias: "pg_crm".into(),
                desired_revision: 1,
                applied_revision: Some(1),
                desired_enabled: true,
                status: SourceStatus::Active,
                generation: 1,
                deleted: false,
                current_operation: None,
                last_error: None,
                catalog_snapshot: None,
            },
        );
        assert_eq!(
            identity_epoch(
                &conn,
                &ManagementState::default(),
                &before,
                &snapshot("one", &["id"])
            )
            .unwrap(),
            "lifetime:1"
        );
        let mut two = before.clone();
        let mut r = revision.clone();
        r.revision = 2;
        r.bindings.push(SchemaBinding {
            remote_schema: "sales".into(),
            enabled: true,
        });
        two.revisions.insert(r.key(), r);
        let mut snap = snapshot("two", &["id"]);
        snap.revision = 2;
        assert_eq!(
            identity_epoch(&conn, &before, &two, &snap).unwrap(),
            "lifetime:1"
        );
        two.sources.get_mut("lifetime").unwrap().applied_revision = Some(2);
        let mut three = two.clone();
        let mut r = three.revisions["lifetime:2"].clone();
        r.revision = 3;
        three.revisions.insert(r.key(), r);
        snap.revision = 3;
        assert_eq!(
            identity_epoch(&conn, &two, &three, &snap).unwrap(),
            "lifetime:3"
        );
    }
}
