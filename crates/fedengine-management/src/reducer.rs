use fedengine_core::*;
use std::collections::BTreeSet;

pub(super) fn conflict(message: &str) -> EngineError {
    EngineError::new("40001", message)
}
pub(super) fn accept(
    state: &mut ManagementState,
    action: ManagementAction,
    key: String,
    digest: String,
    operation_id: String,
    capacity: usize,
    mysql_identity_policy: MysqlIdentityPolicy,
) -> Result<()> {
    if key.is_empty() || key.len() > 256 {
        return Err(EngineError::config(
            "Idempotency-Key must contain 1 to 256 bytes",
        ));
    }
    if let Some(previous) = state.operations.values().find(|o| o.idempotency_key == key) {
        return if previous.parameter_digest == digest {
            Ok(())
        } else {
            Err(conflict(
                "idempotency key was used with different parameters",
            ))
        };
    }
    if state
        .operations
        .values()
        .filter(|o| !o.status.finished())
        .count()
        >= capacity
    {
        return Err(EngineError::new(
            "53300",
            "management operation queue is full",
        ));
    }
    let (incarnation, target_revision) = match &action {
        ManagementAction::Register { definition } => {
            validate_source(definition)?;
            if state.sources.values().filter(|s| !s.deleted).count() >= 128 {
                return Err(EngineError::new("54000", "source limit exceeded"));
            }
            if state.sources.values().any(|s| {
                !s.deleted
                    && (s.id == definition.id
                        || s.catalog_alias
                            .eq_ignore_ascii_case(&definition.catalog_alias))
            }) {
                return Err(conflict("source id or catalog alias already exists"));
            }
            let incarnation = uuid::Uuid::new_v4().to_string();
            let mut definition = definition.clone();
            definition.enabled = false;
            let revision = SourceRevision {
                incarnation: incarnation.clone(),
                revision: 1,
                bindings: definition
                    .allowed_schemas
                    .iter()
                    .map(|s| SchemaBinding {
                        remote_schema: s.clone(),
                        enabled: true,
                    })
                    .collect(),
                definition,
            };
            state.revisions.insert(revision.key(), revision);
            state.sources.insert(
                incarnation.clone(),
                ManagedSource {
                    id: action.id().clone(),
                    incarnation: incarnation.clone(),
                    kind: match &action {
                        ManagementAction::Register { definition } => definition.kind,
                        _ => unreachable!(),
                    },
                    catalog_alias: match &action {
                        ManagementAction::Register { definition } => {
                            definition.catalog_alias.clone()
                        }
                        _ => unreachable!(),
                    },
                    desired_revision: 1,
                    applied_revision: None,
                    desired_enabled: false,
                    status: SourceStatus::Registered,
                    generation: 0,
                    deleted: false,
                    current_operation: Some(operation_id.clone()),
                    last_error: None,
                    catalog_snapshot: None,
                },
            );
            (incarnation, 1)
        }
        _ => {
            let current = state.source(action.id())?.clone();
            let target = action.target().unwrap();
            if target.incarnation != current.incarnation
                || target.expected_revision != current.desired_revision
            {
                return Err(conflict("source incarnation or revision changed"));
            }
            if current.current_operation.as_ref().is_some_and(|id| {
                state
                    .operations
                    .get(id)
                    .is_some_and(|o| !o.status.finished())
            }) {
                return Err(conflict("another operation is unfinished for this source"));
            }
            let mut next_revision = None;
            match &action {
                ManagementAction::Patch { secret_ref, .. } => {
                    let mut revision = state
                        .revision(&current.incarnation, current.desired_revision)?
                        .clone();
                    revision.revision += 1;
                    revision.definition.secret_ref = secret_ref.clone();
                    validate_source(&revision.effective_definition())?;
                    next_revision = Some(revision);
                }
                ManagementAction::Schemas {
                    bindings,
                    default_schema,
                    ..
                } => {
                    let mut unique = BTreeSet::new();
                    if bindings.len() > 10_000
                        || bindings.iter().any(|b| {
                            !allowed_namespace(&b.remote_schema)
                                || !unique.insert(b.remote_schema.clone())
                        })
                    {
                        return Err(EngineError::config("invalid or duplicate schema binding"));
                    }
                    if default_schema.as_ref().is_some_and(|s| {
                        !bindings.iter().any(|b| b.enabled && b.remote_schema == *s)
                    }) {
                        return Err(EngineError::config(
                            "default schema must be included and enabled, or explicitly cleared",
                        ));
                    }
                    let mut revision = state
                        .revision(&current.incarnation, current.desired_revision)?
                        .clone();
                    revision.revision += 1;
                    revision.bindings = bindings.clone();
                    revision.definition.default_schema = default_schema.clone().unwrap_or_default();
                    // This list is derived, never independently editable after bootstrap.
                    revision.definition.allowed_schemas = bindings
                        .iter()
                        .filter(|b| b.enabled)
                        .map(|b| b.remote_schema.clone())
                        .collect();
                    next_revision = Some(revision);
                }
                ManagementAction::Apply { .. } => {
                    let revision =
                        state.revision(&current.incarnation, current.desired_revision)?;
                    if revision.bindings.iter().all(|b| !b.enabled) {
                        return Err(EngineError::config(
                            "activation requires at least one enabled schema",
                        ));
                    }
                    // Resuming an unchanged disabled mount reuses its connection identity.
                    // Active reapply and pending credential revisions still rotate identity.
                    if current.applied_revision == Some(current.desired_revision)
                        && (current.status != SourceStatus::Disabled
                            || mysql_identity_policy.resets(current.kind))
                    {
                        let mut revision = revision.clone();
                        revision.revision += 1;
                        next_revision = Some(revision);
                    }
                }
                ManagementAction::Delete { .. } => {
                    if current.desired_enabled || current.status != SourceStatus::Disabled {
                        return Err(conflict(
                            "disable the source before deleting its registration",
                        ));
                    }
                }
                ManagementAction::Refresh { .. } => {
                    if current.applied_revision.is_none()
                        || !matches!(
                            current.status,
                            SourceStatus::Active | SourceStatus::MetadataStale
                        )
                    {
                        return Err(conflict(
                            "refresh requires an active or metadata-stale source",
                        ));
                    }
                    if mysql_identity_policy.resets(current.kind)
                        && current.status == SourceStatus::MetadataStale
                    {
                        let mut revision = state
                            .revision(&current.incarnation, current.desired_revision)?
                            .clone();
                        revision.revision += 1;
                        next_revision = Some(revision);
                    }
                }
                _ => {}
            }
            let source = state.sources.get_mut(&current.incarnation).unwrap();
            if let Some(revision) = next_revision {
                source.desired_revision = revision.revision;
                state.revisions.insert(revision.key(), revision);
            }
            if matches!(action, ManagementAction::Apply { .. }) {
                source.desired_enabled = true;
            }
            if matches!(action, ManagementAction::Disable { .. }) {
                source.desired_enabled = false;
            }
            source.current_operation = Some(operation_id.clone());
            (current.incarnation, source.desired_revision)
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    state.operations.insert(
        operation_id.clone(),
        ManagementOperation {
            id: operation_id,
            idempotency_key: key,
            parameter_digest: digest,
            incarnation,
            target_revision,
            action,
            status: OperationStatus::Pending,
            phase: "Accepted".into(),
            result_schemas: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
        },
    );
    Ok(())
}
