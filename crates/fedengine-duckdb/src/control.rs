use crate::{
    catalog::{capture, discover},
    credentials::resolve,
    error::engine_error,
    extensions::verify_extension,
    storage,
};
use duckdb::Connection;
use fedengine_contract::*;
use fedengine_core::*;
use fedengine_datasource::DataSourceRegistry;

pub(super) fn attach(
    conn: &Connection,
    config: &Config,
    source: &DataSourceDefinition,
) -> Result<()> {
    validate_source(source)?;
    let kind = source.kind;
    let extension = kind.extension();
    let d = &config.duckdb;
    if d.install_extensions {
        conn.execute_batch("SET custom_extension_repository='https://extensions.duckdb.org'")
            .map_err(engine_error)?;
        conn.execute_batch(&format!("INSTALL {extension}"))
            .map_err(engine_error)?;
    }
    if let Some(directory) = &d.extension_directory {
        verify_extension(directory, extension)?;
    }
    conn.execute_batch(&format!("LOAD {extension}"))
        .map_err(engine_error)?;
    let settings = if kind == DataSourceKind::Postgres {
        format!(
            "SET pg_pool_acquire_mode='wait'; SET pg_pool_max_connections={}; SET pg_pool_wait_timeout_millis={}",
            d.pool_max_connections, d.pool_wait_timeout_ms
        )
    } else {
        format!(
            "SET mysql_pool_acquire_mode='wait'; SET mysql_pool_size={}; SET mysql_pool_wait_timeout_millis={}",
            d.pool_max_connections, d.pool_wait_timeout_ms
        )
    };
    conn.execute_batch(&settings).map_err(engine_error)?;
    let secret = resolve(&source.secret_ref, &config.management.credentials_directory)?;
    conn.execute_batch(&format!(
        "ATTACH {} AS {} (TYPE {extension}{})",
        quote_literal(&secret),
        quote_identifier(&source.catalog_alias),
        if source.read_only { ", READ_ONLY" } else { "" }
    ))
    .map_err(engine_error)?;
    let result = verify_capability(conn, config, source);
    if result.is_err() {
        let _ = detach(conn, &source.catalog_alias);
    }
    result
}
fn verify_capability(
    conn: &Connection,
    config: &Config,
    source: &DataSourceDefinition,
) -> Result<()> {
    if let Some(capability) = config.capabilities.iter().find(|c| c.source == source.id) {
        let extension = source.kind.extension();
        let version: String = conn
            .query_row(
                "SELECT extension_version FROM duckdb_extensions() WHERE extension_name=?",
                [format!("{extension}_scanner")],
                |r| r.get(0),
            )
            .map_err(engine_error)?;
        let query = if source.kind == DataSourceKind::Postgres {
            "SELECT current_setting('server_version') AS version"
        } else {
            "SELECT VERSION() AS version"
        };
        let remote: String = conn
            .query_row(
                &format!(
                    "SELECT version FROM {extension}_query({}, {})",
                    quote_literal(&source.catalog_alias),
                    quote_literal(query)
                ),
                [],
                |r| r.get(0),
            )
            .map_err(engine_error)?;
        if version != capability.extension_version || remote != capability.remote_version {
            return Err(EngineError::config(
                "capability extension/remote version does not match",
            ));
        }
    }
    Ok(())
}
pub(super) fn detach(conn: &Connection, alias: &str) -> Result<()> {
    conn.execute_batch(&format!("DETACH {}", quote_identifier(alias)))
        .map_err(engine_error)
}
pub(super) fn execute(
    conn: &Connection,
    config: &Config,
    action: ControlAction,
) -> Result<ControlOutcome> {
    let mut outcome = ControlOutcome {
        schemas: vec![],
        catalog: None,
        restored_previous: false,
        error: None,
    };
    match action {
        ControlAction::Inspect {
            revision,
            reuse_attached,
        } => {
            if reuse_attached {
                outcome.schemas = discover(conn, &revision.effective_definition())?;
                return Ok(outcome);
            }
            let mut definition = revision.effective_definition();
            definition.catalog_alias =
                format!("_fedengine_probe_{}", uuid::Uuid::new_v4().simple());
            attach(conn, config, &definition)?;
            let result = discover(conn, &definition);
            let cleanup = detach(conn, &definition.catalog_alias);
            outcome.schemas = result?;
            cleanup?;
        }
        ControlAction::Switch { previous, next } => {
            let strict_identity = config
                .management
                .mysql_identity_policy
                .resets(next.definition.kind);
            // Probe another revision without disturbing the old applied attachment.
            let mut temporary = next.effective_definition();
            temporary.catalog_alias = format!("_fedengine_probe_{}", uuid::Uuid::new_v4().simple());
            let probe = (|| {
                attach(conn, config, &temporary)?;
                let result = discover(conn, &temporary).and_then(|schemas| {
                    if next
                        .bindings
                        .iter()
                        .any(|b| b.enabled && !schemas.contains(&b.remote_schema))
                    {
                        Err(EngineError::config(
                            "selected schema does not exist in the target database",
                        ))
                    } else {
                        Ok(())
                    }
                });
                let cleanup = detach(conn, &temporary.catalog_alias);
                result?;
                cleanup
            })();
            if let Err(error) = probe {
                if strict_identity {
                    if let Some(old) = &previous {
                        detach(conn, &old.definition.catalog_alias)?;
                    }
                } else {
                    outcome.restored_previous = previous.is_some();
                }
                outcome.error = Some(error);
                return Ok(outcome);
            }
            if let Some(old) = &previous {
                detach(conn, &old.definition.catalog_alias)?;
            }
            let definition = next.effective_definition();
            let result = attach(conn, config, &definition).and_then(|_| capture(conn, &next));
            match result {
                Ok(snapshot) => outcome.catalog = Some(snapshot),
                Err(error) => {
                    let _ = detach(conn, &definition.catalog_alias);
                    if let Some(old) = previous.filter(|_| !strict_identity)
                        && let Ok(snapshot) = attach(conn, config, &old.effective_definition())
                            .and_then(|_| capture(conn, &old))
                    {
                        outcome.restored_previous = true;
                        outcome.catalog = Some(snapshot);
                    }
                    outcome.error = Some(error);
                }
            }
        }
        ControlAction::Disable(revision) => detach(conn, &revision.definition.catalog_alias)?,
        ControlAction::Refresh(revision) => {
            conn.execute_batch(&format!(
                "CALL {}_clear_cache()",
                revision.definition.kind.extension()
            ))
            .map_err(engine_error)?;
            let mut scanned = revision;
            if !config.management.enabled {
                let available = discover(conn, &scanned.effective_definition())?;
                scanned
                    .bindings
                    .retain(|b| available.contains(&b.remote_schema));
            }
            outcome.catalog = Some(capture(conn, &scanned)?);
        }
    }
    Ok(outcome)
}
/// Startup reattaches desired definitions and records a distinct recovery operation.
pub(super) fn recover(
    conn: &Connection,
    config: &Config,
    registry: &DataSourceRegistry,
) -> Result<()> {
    let state = storage::read_state(conn)?;
    for source in state.sources.values().filter(|s| !s.deleted) {
        if !source.desired_enabled {
            if source.status != SourceStatus::Registered {
                let incarnation = source.incarnation.clone();
                storage::transaction(
                    conn,
                    Box::new(move |state| {
                        let source = state.sources.get_mut(&incarnation).unwrap();
                        source.status = SourceStatus::Disabled;
                        source.current_operation = None;
                        Ok(())
                    }),
                )?;
            }
            continue;
        }
        let mut desired = state
            .revision(&source.incarnation, source.desired_revision)?
            .clone();
        let strict_identity = config.management.mysql_identity_policy.resets(source.kind);
        if strict_identity {
            // No claim of identifying a specific offline DROP/CREATE: the whole
            // attachment receives a fresh epoch when continuity cannot be proved.
            desired.revision = desired
                .revision
                .checked_add(1)
                .ok_or_else(|| EngineError::new("54000", "source revision exhausted"))?;
            let revision = desired.clone();
            storage::transaction(
                conn,
                Box::new(move |state| {
                    state
                        .sources
                        .get_mut(&revision.incarnation)
                        .unwrap()
                        .desired_revision = revision.revision;
                    state.revisions.insert(revision.key(), revision);
                    Ok(())
                }),
            )?;
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let incarnation = source.incarnation.clone();
        let now = chrono::Utc::now().to_rfc3339();
        let recovery = ManagementOperation {
            id: operation_id.clone(),
            idempotency_key: format!("recovery:{operation_id}"),
            parameter_digest: "startup-reconciliation".into(),
            incarnation: incarnation.clone(),
            target_revision: desired.revision,
            action: ManagementAction::Apply {
                id: source.id.clone(),
                target: SourceTarget {
                    incarnation: incarnation.clone(),
                    expected_revision: desired.revision,
                },
            },
            status: OperationStatus::Running,
            phase: "Recovering".into(),
            result_schemas: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        let operation_key = operation_id.clone();
        storage::transaction(
            conn,
            Box::new(move |state| {
                state
                    .sources
                    .get_mut(&recovery.incarnation)
                    .unwrap()
                    .current_operation = Some(operation_key.clone());
                state.operations.insert(operation_key, recovery);
                Ok(())
            }),
        )?;
        let result = attach(conn, config, &desired.effective_definition())
            .and_then(|_| capture(conn, &desired));
        let (effective, catalog, error) = match result {
            Ok(catalog) => (Some(desired.clone()), Some(catalog), None),
            Err(error) => {
                let _ = detach(conn, &desired.definition.catalog_alias);
                let fallback = source
                    .applied_revision
                    .filter(|_| !strict_identity)
                    .filter(|r| *r != desired.revision)
                    .and_then(|r| state.revision(&incarnation, r).ok())
                    .cloned();
                let restored = fallback.as_ref().and_then(|r| {
                    attach(conn, config, &r.effective_definition())
                        .and_then(|_| capture(conn, r))
                        .ok()
                });
                if restored.is_some() {
                    (fallback, restored, Some(error.to_string()))
                } else {
                    let _ = detach(conn, &desired.definition.catalog_alias);
                    (None, None, Some(error.to_string()))
                }
            }
        };
        let mut catalog = catalog;
        let generation = source.generation + 1;
        if let Some(catalog) = &mut catalog {
            catalog.generation = generation;
        }
        let incarnation_update = incarnation.clone();
        let operation_update = operation_id.clone();
        let applied = effective.as_ref().map(|r| r.revision);
        let succeeded = error.is_none();
        storage::transaction(
            conn,
            Box::new(move |state| {
                let source = state.sources.get_mut(&incarnation_update).unwrap();
                source.status = if applied.is_some() {
                    SourceStatus::Active
                } else {
                    SourceStatus::Degraded
                };
                source.last_error = error.clone();
                source.generation = generation;
                if let Some(applied) = applied {
                    source.applied_revision = Some(applied);
                }
                if let Some(catalog) = catalog {
                    source.catalog_snapshot = Some(catalog.id.clone());
                    state.catalogs.insert(catalog.id.clone(), catalog);
                } else if let Some(catalog) = source
                    .catalog_snapshot
                    .as_ref()
                    .and_then(|id| state.catalogs.get_mut(id))
                {
                    catalog.stale = true;
                }
                let operation = state.operations.get_mut(&operation_update).unwrap();
                operation.phase = "Publishing".into();
                operation.error = error;
                Ok(())
            }),
        )?;
        if let Some(effective) = effective {
            registry.publish(
                effective.effective_definition(),
                incarnation.clone(),
                effective.revision,
                generation,
            );
        }
        storage::transaction(
            conn,
            Box::new(move |state| {
                let operation = state.operations.get_mut(&operation_id).unwrap();
                operation.status = if succeeded {
                    OperationStatus::Succeeded
                } else {
                    OperationStatus::Failed
                };
                operation.phase = "Complete".into();
                operation.updated_at = chrono::Utc::now().to_rfc3339();
                state
                    .sources
                    .get_mut(&incarnation)
                    .unwrap()
                    .current_operation = None;
                state.prune_catalogs(3);
                Ok(())
            }),
        )?;
    }
    Ok(())
}

/// Static mode still publishes the same catalog facts; source admission remains fail-fast.
pub(super) fn initialize_static(
    conn: &Connection,
    config: &Config,
    registry: &DataSourceRegistry,
) -> Result<()> {
    let state = storage::read_state(conn)?;
    let mut publications = Vec::new();
    for definition in &config.datasources {
        let existing = state
            .sources
            .values()
            .find(|s| s.id == definition.id && !s.deleted);
        let incarnation = existing
            .map(|s| s.incarnation.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let previous =
            existing.and_then(|s| state.revision(&s.incarnation, s.desired_revision).ok());
        let revision = match previous {
            Some(p)
                if p.definition == *definition
                    && !(definition.enabled
                        && config
                            .management
                            .mysql_identity_policy
                            .resets(definition.kind)) =>
            {
                p.revision
            }
            Some(p) => p.revision + 1,
            None => 1,
        };
        let revision = SourceRevision {
            incarnation: incarnation.clone(),
            revision,
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
        let generation = existing.map(|s| s.generation + 1).unwrap_or(1);
        let catalog = if definition.enabled {
            attach(conn, config, definition)?;
            let available = discover(conn, definition)?;
            let mut scanned = revision.clone();
            scanned
                .bindings
                .retain(|b| available.contains(&b.remote_schema));
            let mut catalog = capture(conn, &scanned)?;
            catalog.generation = generation;
            Some(catalog)
        } else {
            None
        };
        publications.push((revision, generation, catalog));
    }
    let registry_publications = publications.clone();
    storage::transaction(
        conn,
        Box::new(move |state| {
            for source in state.sources.values_mut() {
                source.deleted = true;
                source.status = SourceStatus::Disabled;
            }
            for (revision, generation, catalog) in publications {
                let d = &revision.definition;
                let source = ManagedSource {
                    id: d.id.clone(),
                    incarnation: revision.incarnation.clone(),
                    kind: d.kind,
                    catalog_alias: d.catalog_alias.clone(),
                    desired_revision: revision.revision,
                    applied_revision: d.enabled.then_some(revision.revision),
                    desired_enabled: d.enabled,
                    status: if d.enabled {
                        SourceStatus::Active
                    } else {
                        SourceStatus::Disabled
                    },
                    generation,
                    deleted: false,
                    current_operation: None,
                    last_error: None,
                    catalog_snapshot: catalog.as_ref().map(|c| c.id.clone()),
                };
                state.sources.insert(source.incarnation.clone(), source);
                state.revisions.insert(revision.key(), revision);
                if let Some(catalog) = catalog {
                    state.catalogs.insert(catalog.id.clone(), catalog);
                }
            }
            state.prune_catalogs(3);
            Ok(())
        }),
    )?;
    for (revision, generation, _) in registry_publications {
        if revision.definition.enabled {
            registry.publish(
                revision.effective_definition(),
                revision.incarnation,
                revision.revision,
                generation,
            );
        }
    }
    Ok(())
}
