//! Explicit logical databases. All writes run in the management publication transaction.
use crate::{error::engine_error, identity, projection};
use duckdb::{Connection, OptionalExt, params};
use fedengine_contract::{VirtualDatabaseMutation, VirtualDatabaseReceipt};
use fedengine_core::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn read(conn: &Connection) -> Result<Vec<VirtualDatabase>> {
    let mut stmt = conn
        .prepare("SELECT payload FROM _fedengine.virtual_databases ORDER BY name")
        .map_err(engine_error)?;
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map_err(engine_error)?
        .map(|r| {
            serde_json::from_str(&r.map_err(engine_error)?)
                .map_err(|_| EngineError::new("XX000", "invalid virtual database storage"))
        })
        .collect()
}
fn save(conn: &Connection, db: &VirtualDatabase) -> Result<()> {
    let payload = serde_json::to_string(db)
        .map_err(|_| EngineError::new("XX000", "invalid virtual database"))?;
    conn.execute("INSERT INTO _fedengine.virtual_databases VALUES (?,?) ON CONFLICT(name) DO UPDATE SET payload=excluded.payload", params![db.config.name,payload]).map_err(engine_error)?;
    Ok(())
}
pub(super) fn initialize(conn: &Connection, config: &Config) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS _fedengine.virtual_databases(name VARCHAR PRIMARY KEY,payload VARCHAR NOT NULL);
        CREATE TABLE IF NOT EXISTS _fedengine.virtual_database_overrides(name VARCHAR PRIMARY KEY,config VARCHAR);
        CREATE TABLE IF NOT EXISTS _fedengine.virtual_database_receipts(key VARCHAR PRIMARY KEY,request VARCHAR NOT NULL,payload VARCHAR NOT NULL)").map_err(engine_error)?;
    let mut definitions = config
        .virtual_databases
        .iter()
        .map(|d| (d.name.clone(), d.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut stmt = conn
        .prepare("SELECT name,config FROM _fedengine.virtual_database_overrides ORDER BY name")
        .map_err(engine_error)?;
    let overrides = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(engine_error)?;
    for entry in overrides {
        let (name, value) = entry.map_err(engine_error)?;
        definitions.remove(&name);
        if let Some(value) = value {
            let definition: VirtualDatabaseConfig = serde_json::from_str(&value)
                .map_err(|_| EngineError::new("XX000", "invalid virtual database override"))?;
            if definition.name != name {
                return Err(EngineError::new(
                    "XX000",
                    "virtual database override identity mismatch",
                ));
            }
            definitions.insert(name, definition);
        }
    }
    let users = config.users.iter().map(|u| u.name.clone()).collect();
    validate_definitions(&definitions.values().cloned().collect::<Vec<_>>(), &users)?;
    reconcile(conn, &definitions.into_values().collect::<Vec<_>>())
}
fn validate_definitions(
    definitions: &[VirtualDatabaseConfig],
    users: &BTreeSet<String>,
) -> Result<()> {
    if definitions.len() > 256 {
        return Err(EngineError::new("54000", "virtual database limit exceeded"));
    }
    let mut names = BTreeSet::new();
    for db in definitions {
        db.validate(users)?;
        if !names.insert(db.name.to_ascii_lowercase()) {
            return Err(EngineError::config("duplicate virtual database name"));
        }
    }
    Ok(())
}
fn reconcile(conn: &Connection, definitions: &[VirtualDatabaseConfig]) -> Result<()> {
    let previous = read(conn)?;
    for old in &previous {
        let new = definitions.iter().find(|d| d.name == old.config.name);
        let mut retire = vec![];
        if new.is_none() {
            retire.extend([old.oid, old.public_oid]);
        }
        for schema in &old.schemas {
            if new.is_none_or(|d| !d.schemas.contains(&schema.config)) {
                retire.push(schema.oid);
            }
        }
        for oid in retire {
            conn.execute(
                "UPDATE _fedengine.object_identity SET retired=true WHERE oid=?",
                [oid],
            )
            .map_err(engine_error)?;
        }
    }
    conn.execute("DELETE FROM _fedengine.virtual_databases", [])
        .map_err(engine_error)?;
    for definition in definitions {
        let old = previous.iter().find(|d| d.config.name == definition.name);
        let revision = match old {
            Some(db) if db.config == *definition => db.revision,
            Some(db) => db
                .revision
                .checked_add(1)
                .ok_or_else(|| EngineError::new("54000", "virtual database revision exhausted"))?,
            None => 1,
        };
        let epoch = format!("virtual-db:{}", definition.name);
        let mut schemas = vec![];
        for binding in &definition.schemas {
            let oid = identity::oid(
                conn,
                &epoch,
                &[
                    "schema",
                    &binding.name,
                    &binding.source.0,
                    &binding.remote_schema,
                ],
            )?;
            schemas.push(VirtualSchema {
                config: binding.clone(),
                oid,
                snapshot_id: None,
            });
        }
        save(
            conn,
            &VirtualDatabase {
                config: definition.clone(),
                revision,
                oid: identity::oid(conn, &epoch, &["database"])?,
                public_oid: identity::oid(conn, &epoch, &["public"])?,
                schemas,
            },
        )?;
    }
    Ok(())
}
/// Local atomic publication, CAS and replay journal. No network work is permitted here.
pub(super) fn mutate(
    conn: &Connection,
    mutation: VirtualDatabaseMutation,
    key: String,
    users: BTreeSet<String>,
) -> Result<VirtualDatabaseReceipt> {
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(EngineError::config("invalid Idempotency-Key"));
    }
    let request = serde_json::to_string(&mutation)
        .map_err(|_| EngineError::config("invalid virtual database mutation"))?;
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(engine_error)?;
    let result = (|| {
        let replay: Option<(String, String)> = conn
            .query_row(
                "SELECT request,payload FROM _fedengine.virtual_database_receipts WHERE key=?",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(engine_error)?;
        if let Some((original, payload)) = replay {
            if request != original {
                return Err(EngineError::new(
                    "40001",
                    "Idempotency-Key was used for a different virtual database request",
                ));
            }
            return serde_json::from_str(&payload)
                .map_err(|_| EngineError::new("XX000", "invalid virtual database receipt"));
        }
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.virtual_database_receipts",
                [],
                |r| r.get(0),
            )
            .map_err(engine_error)?;
        if count >= 10000 {
            return Err(EngineError::new(
                "54000",
                "virtual database receipt budget exceeded",
            ));
        }
        let current = read(conn)?;
        let (name, target, definition) = match &mutation {
            VirtualDatabaseMutation::Create { definition } => {
                (definition.name.clone(), None, Some(definition.clone()))
            }
            VirtualDatabaseMutation::Replace {
                name,
                target,
                definition,
            } => {
                if name != &definition.name {
                    return Err(EngineError::config(
                        "virtual database rename is unsupported",
                    ));
                }
                (name.clone(), Some(target), Some(definition.clone()))
            }
            VirtualDatabaseMutation::Delete { name, target } => (name.clone(), Some(target), None),
        };
        let old = current.iter().find(|d| d.config.name == name);
        if let Some(target) = target {
            let old =
                old.ok_or_else(|| EngineError::new("3D000", "virtual database does not exist"))?;
            if old.oid != target.oid || old.revision != target.expected_revision {
                return Err(EngineError::new(
                    "40001",
                    "virtual database identity or revision changed",
                ));
            }
        } else if current
            .iter()
            .any(|d| d.config.name.eq_ignore_ascii_case(&name))
        {
            return Err(EngineError::new("40001", "virtual database already exists"));
        }
        let mut definitions = current
            .iter()
            .filter(|d| d.config.name != name)
            .map(|d| d.config.clone())
            .collect::<Vec<_>>();
        if let Some(definition) = &definition {
            definitions.push(definition.clone());
        }
        validate_definitions(&definitions, &users)?;
        let state = crate::storage::read_heads(conn)?;
        if let Some(definition) = &definition {
            for binding in &definition.schemas {
                let source = state.source(&binding.source)?;
                let revision = state.revision(&source.incarnation, source.desired_revision)?;
                if !revision
                    .bindings
                    .iter()
                    .any(|s| s.enabled && s.remote_schema == binding.remote_schema)
                {
                    return Err(EngineError::config(
                        "virtual schema is outside the source schema bindings",
                    ));
                }
            }
        }
        let exists: bool = conn
            .query_row(
                "SELECT count(*)>0 FROM _fedengine.virtual_database_overrides WHERE name=?",
                [&name],
                |r| r.get(0),
            )
            .map_err(engine_error)?;
        let overrides: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.virtual_database_overrides",
                [],
                |r| r.get(0),
            )
            .map_err(engine_error)?;
        if !exists && overrides >= 4096 {
            return Err(EngineError::new(
                "54000",
                "virtual database override budget exceeded",
            ));
        }
        let value = definition
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| EngineError::config("invalid virtual database definition"))?;
        conn.execute("INSERT INTO _fedengine.virtual_database_overrides VALUES (?,?) ON CONFLICT(name) DO UPDATE SET config=excluded.config",params![name,value]).map_err(engine_error)?;
        reconcile(conn, &definitions)?;
        publish(conn, &state)?;
        let next = read(conn)?.into_iter().find(|d| d.config.name == name);
        let (oid, revision) = match next {
            Some(db) => (db.oid, db.revision),
            None => {
                let db = old.unwrap();
                (
                    db.oid,
                    db.revision.checked_add(1).ok_or_else(|| {
                        EngineError::new("54000", "virtual database revision exhausted")
                    })?,
                )
            }
        };
        let receipt = VirtualDatabaseReceipt {
            idempotency_key: key.clone(),
            name,
            oid,
            revision,
            deleted: definition.is_none(),
        };
        let payload = serde_json::to_string(&receipt)
            .map_err(|_| EngineError::new("XX000", "invalid virtual database receipt"))?;
        conn.execute(
            "INSERT INTO _fedengine.virtual_database_receipts VALUES (?,?,?)",
            params![key, request, payload],
        )
        .map_err(engine_error)?;
        Ok(receipt)
    })();
    match result {
        Ok(receipt) => match conn.execute_batch("COMMIT").map_err(engine_error) {
            Ok(()) => Ok(receipt),
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        },
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}
fn empty_snapshot(
    id: String,
    db: &VirtualDatabase,
    schemas: BTreeMap<String, u32>,
) -> CatalogSnapshot {
    CatalogSnapshot {
        id,
        incarnation: format!("virtual:{}", db.oid),
        revision: 1,
        generation: 1,
        database_oid: db.oid,
        schemas: schemas.keys().cloned().collect(),
        schema_oids: schemas,
        refreshed_at: String::new(),
        complete: true,
        stale: false,
        structure_digest: String::new(),
        objects: vec![],
    }
}
pub(super) fn publish(conn: &Connection, state: &ManagementState) -> Result<()> {
    let mut keep = BTreeSet::new();
    for mut db in read(conn)? {
        if state
            .sources
            .values()
            .any(|s| !s.deleted && s.catalog_alias.eq_ignore_ascii_case(&db.config.name))
        {
            return Err(EngineError::config(
                "virtual database conflicts with a source database",
            ));
        }
        let base = format!("virtual:{}:public", db.oid);
        let snapshot = empty_snapshot(base.clone(), &db, [("public".into(), db.public_oid)].into());
        projection::publish(conn, &snapshot, &db.config.name)?;
        // Owner identities are resolved for the caller by the catalog planner.
        conn.execute(
            "UPDATE _fedengine.meta_pg_database SET datallowconn=? WHERE _snapshot_id=?",
            params![db.config.allow_connections, base],
        )
        .map_err(engine_error)?;
        keep.insert(base);
        for index in 0..db.schemas.len() {
            let binding = &db.schemas[index];
            let source = state
                .sources
                .values()
                .find(|s| s.id == binding.config.source && !s.deleted);
            let source_snapshot = source
                .and_then(|s| s.catalog_snapshot.as_ref())
                .and_then(|id| state.catalogs.get(id));
            let Some(source_snapshot) = source_snapshot else {
                db.schemas[index].snapshot_id = None;
                continue;
            };
            let id = format!("virtual:{}:{}", binding.oid, source_snapshot.id);
            let exists:bool=conn.query_row("SELECT count(*)>0 FROM _fedengine.metadata_projection_state WHERE snapshot_id=?",[&id],|r|r.get(0)).map_err(engine_error)?;
            if !exists {
                let snapshot = empty_snapshot(
                    id.clone(),
                    &db,
                    [(binding.config.name.clone(), binding.oid)].into(),
                );
                let epoch = format!("virtual-schema:{}", binding.oid);
                let mut oids = identity::OidBatch::load(conn, std::slice::from_ref(&epoch))?;
                projection::ForeignProjection {
                    snapshot: &snapshot,
                    source: source_snapshot,
                    alias: &db.config.name,
                    binding,
                }
                .publish(conn, &mut oids)?;
                oids.flush()?;
            }
            keep.insert(id.clone());
            db.schemas[index].snapshot_id = Some(id);
        }
        save(conn, &db)?;
    }
    // Readers hold a DuckDB read transaction; queued stale scopes fail explicitly on admission.
    let mut stmt=conn.prepare("SELECT snapshot_id FROM _fedengine.metadata_projection_state WHERE starts_with(snapshot_id,'virtual:')").map_err(engine_error)?;
    let old = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(engine_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(engine_error)?;
    for id in old.into_iter().filter(|id| !keep.contains(id)) {
        projection::remove(conn, &id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapped_publication_is_durable_isolated_and_does_not_invent_local_constraints() {
        let mut config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/config/federation.toml"),
        )
        .unwrap();
        config.virtual_databases = vec![VirtualDatabaseConfig {
            name: "fedengine".into(),
            owner: "fedengine".into(),
            connect: vec![],
            allow_connections: true,
            schemas: vec![VirtualSchemaConfig {
                name: "crm".into(),
                source: DataSourceId("crm".into()),
                remote_schema: "public".into(),
            }],
        }];
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        let before = read(&conn).unwrap().remove(0);
        let state = crate::storage::transaction(
            &conn,
            Box::new(|state| {
                let source = state
                    .sources
                    .values_mut()
                    .find(|s| s.id.0 == "crm")
                    .unwrap();
                source.applied_revision = Some(1);
                source.status = SourceStatus::Active;
                source.catalog_snapshot = Some("original".into());
                let mut object = CatalogObject {
                    schema: "public".into(),
                    name: "items".into(),
                    object_type: "TABLE".into(),
                    indexes: vec![],
                    metadata: Default::default(),
                    columns: vec![CatalogColumn {
                        name: "id".into(),
                        ordinal: 1,
                        remote_type: "integer".into(),
                        mapped_type: Some("INTEGER".into()),
                        nullable: false,
                        supported: true,
                    }],
                };
                object.metadata.constraints.push(MetadataConstraint {
                    name: "pk".into(),
                    kind: "p".into(),
                    columns: vec!["id".into()],
                    ..Default::default()
                });
                let snapshot = CatalogSnapshot {
                    id: "original".into(),
                    incarnation: source.incarnation.clone(),
                    revision: 1,
                    generation: 1,
                    database_oid: 0,
                    schema_oids: BTreeMap::new(),
                    refreshed_at: "now".into(),
                    complete: true,
                    stale: false,
                    structure_digest: "v1".into(),
                    schemas: vec!["public".into()],
                    objects: vec![object],
                };
                state.catalogs.insert(snapshot.id.clone(), snapshot);
                Ok(())
            }),
        )
        .unwrap();
        let db = read(&conn).unwrap().remove(0);
        assert_eq!(db.oid, before.oid);
        let id = db.schemas[0].snapshot_id.as_ref().unwrap();
        let (oid,namespace,kind):(u32,u32,String)=conn.query_row("SELECT oid,relnamespace,relkind FROM _fedengine.meta_pg_class WHERE _snapshot_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_ne!(oid, state.catalogs["original"].objects[0].metadata.oid);
        assert_eq!(namespace, db.schemas[0].oid);
        assert_eq!(kind, "f");
        let constraints: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.meta_pg_constraint WHERE _snapshot_id=?",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(constraints, 0);
        // Restart/config reconciliation and unchanged refresh retain the logical identities.
        crate::storage::initialize(&conn, &config).unwrap();
        assert_eq!(read(&conn).unwrap()[0], db);
        // A fresh capture supplies its objects explicitly; control transactions
        // deliberately expose headers rather than loading historical payloads.
        let mut next = crate::storage::read_state(&conn)
            .unwrap()
            .catalogs
            .remove("original")
            .unwrap();
        crate::storage::transaction(
            &conn,
            Box::new(move |state| {
                next.id = "refreshed".into();
                next.refreshed_at = "later".into();
                // A changed comment prevents the unchanged-fact optimization.
                next.objects[0].metadata.comment = Some("new comment".into());
                state
                    .sources
                    .values_mut()
                    .find(|s| s.id.0 == "crm")
                    .unwrap()
                    .catalog_snapshot = Some(next.id.clone());
                state.catalogs.insert(next.id.clone(), next);
                Ok(())
            }),
        )
        .unwrap();
        let next = read(&conn).unwrap().remove(0);
        let next_id = next.schemas[0].snapshot_id.as_ref().unwrap();
        assert_ne!(id, next_id);
        assert_eq!(
            conn.query_row(
                "SELECT oid FROM _fedengine.meta_pg_class WHERE _snapshot_id=?",
                [next_id],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            oid
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM _fedengine.metadata_projection_state WHERE snapshot_id=?",
                [id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        // Removing and recreating a mapping gets a new schema and relation identity.
        config.virtual_databases[0].schemas.clear();
        crate::storage::initialize(&conn, &config).unwrap();
        config.virtual_databases[0]
            .schemas
            .push(db.schemas[0].config.clone());
        crate::storage::initialize(&conn, &config).unwrap();
        let recreated = read(&conn).unwrap().remove(0);
        assert_eq!(recreated.oid, db.oid);
        assert_ne!(recreated.schemas[0].oid, db.schemas[0].oid);
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use fedengine_contract::VirtualDatabaseTarget;
    #[test]
    fn cas_replay_overrides_tombstones_and_projection_rollback_are_durable() {
        let path = std::env::temp_dir().join(format!(
            "fedengine-virtual-api-{}.duckdb",
            uuid::Uuid::new_v4()
        ));
        let mut config = Config::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/config/federation.toml"),
        )
        .unwrap();
        let definition = VirtualDatabaseConfig {
            name: "account".into(),
            owner: "fedengine".into(),
            connect: vec![],
            allow_connections: true,
            schemas: vec![],
        };
        config.virtual_databases = vec![definition.clone()];
        let users = config
            .users
            .iter()
            .map(|u| u.name.clone())
            .collect::<BTreeSet<_>>();
        let conn = Connection::open(&path).unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        let original = read(&conn).unwrap().remove(0);
        let target = VirtualDatabaseTarget {
            oid: original.oid,
            expected_revision: original.revision,
        };
        // Publication error rolls back definition, override, identities and receipt together.
        let collision = VirtualDatabaseMutation::Create {
            definition: VirtualDatabaseConfig {
                name: "pg_crm".into(),
                ..definition.clone()
            },
        };
        assert!(mutate(&conn, collision, "collision".into(), users.clone()).is_err());
        assert_eq!(read(&conn).unwrap(), vec![original.clone()]);
        let receipts: i64 = conn
            .query_row(
                "SELECT count(*) FROM _fedengine.virtual_database_receipts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(receipts, 0);
        let mut changed = definition.clone();
        changed.allow_connections = false;
        let replace = VirtualDatabaseMutation::Replace {
            name: definition.name.clone(),
            target: target.clone(),
            definition: changed.clone(),
        };
        let receipt = mutate(&conn, replace.clone(), "replace".into(), users.clone()).unwrap();
        assert_eq!(receipt.revision, 2);
        assert_eq!(
            mutate(&conn, replace.clone(), "replace".into(), users.clone()).unwrap(),
            receipt
        );
        assert_eq!(
            mutate(&conn, replace, "new-key".into(), users.clone())
                .unwrap_err()
                .sqlstate,
            "40001"
        );
        assert_eq!(
            mutate(
                &conn,
                VirtualDatabaseMutation::Delete {
                    name: definition.name.clone(),
                    target: target.clone()
                },
                "replace".into(),
                users.clone()
            )
            .unwrap_err()
            .sqlstate,
            "40001"
        );
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        assert_eq!(read(&conn).unwrap()[0].config, changed);
        let deletion = VirtualDatabaseMutation::Delete {
            name: definition.name.clone(),
            target: VirtualDatabaseTarget {
                oid: original.oid,
                expected_revision: 2,
            },
        };
        let deleted = mutate(&conn, deletion.clone(), "delete".into(), users.clone()).unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        assert!(
            read(&conn).unwrap().is_empty(),
            "TOML must not resurrect an API deletion"
        );
        let created = mutate(
            &conn,
            VirtualDatabaseMutation::Create { definition },
            "create".into(),
            users.clone(),
        )
        .unwrap();
        assert_ne!(created.oid, original.oid);
        assert_eq!(
            mutate(&conn, deletion.clone(), "delete".into(), users.clone()).unwrap(),
            deleted
        );
        assert_eq!(
            mutate(&conn, deletion, "aba".into(), users.clone())
                .unwrap_err()
                .sqlstate,
            "40001"
        );
        assert_eq!(read(&conn).unwrap()[0].oid, created.oid);
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        crate::storage::initialize(&conn, &config).unwrap();
        assert_eq!(read(&conn).unwrap()[0].oid, created.oid);
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
}
