//! Durable local identities. Called inside the management writer's transaction only.
use crate::error::engine_error;
use duckdb::{Connection, OptionalExt, params};
use fedengine_core::*;
use std::collections::BTreeSet;

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
fn attnums(conn: &Connection, object: &mut CatalogObject) -> Result<()> {
    let table = object.metadata.oid;
    let mut old = conn.prepare("SELECT name,attnum,source_id FROM _fedengine.column_identity WHERE table_oid=? AND NOT retired").map_err(engine_error)?;
    let old = old
        .query_map([table], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i16>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(engine_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(engine_error)?;
    let mut max: i32 = conn.query_row("SELECT coalesce(max(attnum),0)::INTEGER FROM _fedengine.column_identity WHERE table_oid=?", [table], |r| r.get(0)).map_err(engine_error)?;
    let mut keep = BTreeSet::new();
    for col in &object.columns {
        let props = object.metadata.columns.entry(col.name.clone()).or_default();
        let matched = old
            .iter()
            .find(|(name, _, source)| match (&props.source_id, source) {
                (Some(want), Some(have)) => want == have,
                _ => name == &col.name,
            });
        let attnum = if let Some((_, num, _)) = matched {
            *num
        } else {
            max += 1;
            i16::try_from(max)
                .map_err(|_| EngineError::new("54000", "column identity space exhausted"))?
        };
        if !keep.insert(attnum) {
            return Err(EngineError::new("55000", "duplicate column identity"));
        }
        props.attnum = attnum;
        if props.default.is_some() || props.generated.is_some() {
            props.default_oid = oid(conn, &format!("default:{table}"), &[&attnum.to_string()])?;
        }
        props.type_oid = match col
            .mapped_type
            .as_ref()
            .and_then(|t| crate::codec::type_name(t).ok())
        {
            Some(t) => t.oid(),
            None => oid(
                conn,
                &format!("type:{table}"),
                &["unsupported_type", &col.remote_type],
            )?,
        };
        props.type_modifier = col
            .mapped_type
            .as_ref()
            .map(|t| crate::codec::type_modifier(t))
            .unwrap_or(-1);
        conn.execute(
            "INSERT OR REPLACE INTO _fedengine.column_identity VALUES (?,?,?,?,false)",
            params![table, col.name, attnum, props.source_id],
        )
        .map_err(engine_error)?;
    }
    for (_, num, _) in old {
        if !keep.contains(&num) {
            conn.execute(
                "UPDATE _fedengine.column_identity SET retired=true WHERE table_oid=? AND attnum=?",
                params![table, num],
            )
            .map_err(engine_error)?;
        }
    }
    let mut stmt=conn.prepare("SELECT attnum FROM _fedengine.column_identity WHERE table_oid=? AND retired ORDER BY attnum").map_err(engine_error)?;
    object.metadata.dropped_attnums = stmt
        .query_map([table], |r| r.get(0))
        .map_err(engine_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(engine_error)?;
    Ok(())
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
pub(super) fn prepare(
    conn: &Connection,
    before: &ManagementState,
    after: &mut ManagementState,
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
        snapshot.database_oid = oid(conn, &snapshot.incarnation, &["database"])?;
        let mut active = BTreeSet::new();
        for schema in &snapshot.schemas {
            let id = oid(conn, &epoch, &["schema", schema])?;
            snapshot.schema_oids.insert(schema.clone(), id);
            active.insert(id);
        }
        for object in &mut snapshot.objects {
            let key = object
                .metadata
                .source_id
                .clone()
                .unwrap_or_else(|| object.name.clone());
            object.metadata.oid = if object.metadata.source_id.is_some() {
                oid(conn, &epoch, &["relation", "remote", &key])?
            } else {
                oid(conn, &epoch, &["relation", "name", &object.schema, &key])?
            };
            active.insert(object.metadata.oid);
            if let Some(old) = previous.and_then(|c| {
                c.objects
                    .iter()
                    .find(|o| o.metadata.oid == object.metadata.oid)
            }) {
                retained_categories(old, object);
            }
            attnums(conn, object)?;
            object.metadata.validate(&object.columns)?;
            for c in &mut object.metadata.constraints {
                let key = c.source_id.as_deref().unwrap_or(&c.name);
                c.oid = oid(
                    conn,
                    &epoch,
                    &["constraint", &object.metadata.oid.to_string(), key],
                )?;
                active.insert(c.oid);
            }
            for i in &mut object.metadata.indexes {
                let key = i.source_id.as_deref().unwrap_or(&i.name);
                i.oid = oid(
                    conn,
                    &epoch,
                    &["index", &object.metadata.oid.to_string(), key],
                )?;
                active.insert(i.oid);
            }
        }
        if let Some(previous) = previous {
            for old in previous
                .objects
                .iter()
                .filter(|o| snapshot.schemas.contains(&o.schema))
            {
                let ids = std::iter::once(old.metadata.oid)
                    .chain(old.metadata.constraints.iter().map(|c| c.oid))
                    .chain(old.metadata.indexes.iter().map(|i| i.oid));
                for id in ids.filter(|id| !active.contains(id)) {
                    conn.execute(
                        "UPDATE _fedengine.object_identity SET retired=true WHERE oid=?",
                        [id],
                    )
                    .map_err(engine_error)?;
                }
            }
        }
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
