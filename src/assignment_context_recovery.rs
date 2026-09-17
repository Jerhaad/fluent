//! Recover one proven historical context binding without resuming its Attempt.
use crate::{recovery_contracts as contracts, work_model::*};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub struct RecoveryRequest<'a> {
    pub work: &'a str,
    pub attempt: &'a str,
    pub task: &'a str,
    pub candidate: &'a str,
    pub assignment_digest: &'a str,
    pub input: &'a str,
    pub original_file: &'a Path,
    pub resolution: &'a str,
}

/// Preserve exact original bytes and their before/after assignment audit.
pub fn recover(
    root: &Path,
    store: &WorkModelStore,
    request: &RecoveryRequest<'_>,
) -> Result<String> {
    ensure!(
        !request.resolution.trim().is_empty() && request.resolution.len() <= 4096,
        "supply a bounded explicit resolution"
    );
    let bytes = contracts::assignment_original_bytes(root, request.original_file)?;
    let supplied_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let mut operation = operation(request, &supplied_digest);
    let audit_path = work_artifact_path(
        request.work,
        request.attempt,
        &format!(
            "assignment-context-recoveries/{}.json",
            operation.operation_id()
        ),
    );
    // Serialize observation and lease probes without creating a land lock on a
    // rejected fresh request. Release the model lock before acquiring land.
    fs::metadata(store.work_item_path(request.work)?)?;
    let mut observation_lock = Some(store.lock_work_item_model(request.work)?);
    let mut land = None;
    let existing_audit = match fs::symlink_metadata(root.join(&audit_path)) {
        Ok(_) => Some(serde_json::from_slice::<AssignmentContextRecoveryAudit>(
            &contracts::assignment_input_bytes(root, &audit_path)?,
        )?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    // A partial child publication cannot provide an observation snapshot. Only
    // this request's exact audit may authorize reconciliation under both locks.
    if let Some(audit) = &existing_audit {
        ensure!(
            matches!(audit.schema_version, 1 | 2)
                && audit.request_id == operation.operation_id()
                && contracts::assignment_digest(&audit.before)? == request.assignment_digest
                && audit.original.digest == supplied_digest
                && audit.original.artifact.path == request.input,
            "saved recovery audit does not match this request"
        );
        let audit_bytes = serde_json::to_vec_pretty(&audit)?;
        ensure!(
            contracts::assignment_input_bytes(root, &audit_path)? == audit_bytes,
            "saved recovery audit bytes changed"
        );
        operation.input_digest = Some(format!("sha256:{:x}", Sha256::digest(&audit_bytes)));
        drop(observation_lock.take());
        land = Some(crate::land_lock::acquire(&crate::land_lock::lock_path(
            root,
        ))?);
        if store.reconcile_assignment_context_transaction(&operation, audit)? {
            return Ok(operation.operation_id());
        }
        observation_lock = Some(store.lock_work_item_model(request.work)?);
    }
    // All other rejection paths use observations and leave journals untouched.
    let observed = store.read_work_item_snapshot(request.work)?;
    let (attempt, task) = contracts::assignment_context_recovery_target(
        root,
        &observed,
        request.attempt,
        request.task,
        request.candidate,
    )?;
    let current_assignment = task
        .writer_correction_assignment
        .as_ref()
        .context("Writer has no saved assignment")?;
    let audit = if let Some(audit) = existing_audit {
        if observed
            .validate_recovery_operation(root, &operation)?
            .is_some()
        {
            ensure!(
                current_assignment == &audit.after,
                "recovered assignment changed"
            );
            contracts::validate_assignment_context_recovery_inputs(root, task, current_assignment)?;
            return Ok(operation.operation_id());
        }
        ensure!(
            audit.attempt_digest == contracts::assignment_attempt_digest(attempt)?,
            "Attempt changed after interrupted publication"
        );
        ensure!(
            current_assignment == &audit.before,
            "saved assignment changed"
        );
        contracts::validate_output_free_context_files(root, &audit)?;
        audit
    } else {
        ensure!(
            contracts::assignment_digest(current_assignment)? == request.assignment_digest,
            "saved assignment digest does not match"
        );
        ensure!(
            attempt.status == AttemptStatus::NeedsUser
                && (attempt.pause_kind == Some(PauseKind::WriterCompletion)
                    || (task.status == TaskStatus::Failed
                        && attempt.pause_kind == Some(PauseKind::RoundCap)))
                && !attempt.cancellation_requested(),
            "recovery requires a settled Writer-completion pause"
        );
        let original = current_assignment
            .source_task_inputs
            .iter()
            .find(|input| input.artifact.path == request.input)
            .context("requested input is not assigned")?
            .clone();
        ensure!(
            contracts::mutable_assignment_context(task, &original.artifact),
            "input is not recognized mutable assignment context"
        );
        // Validate the current context's path, without treating its current bytes as authority.
        contracts::assignment_input_bytes(root, request.input)?;
        ensure!(
            original.digest == supplied_digest,
            "original bytes do not match the recorded digest"
        );
        if task.status == TaskStatus::Failed {
            ensure!(
                contracts::assignment_input_digest(root, request.input)? != original.digest,
                "output-free context recovery requires a stale input binding"
            );
        }
        let (snapshot, provenance, _) = contracts::assignment_context_snapshot(task, &original)?;
        let mut after = current_assignment.clone();
        after.schema_version = WRITER_CORRECTION_ASSIGNMENT_SCHEMA_VERSION;
        *after
            .source_task_inputs
            .iter_mut()
            .find(|input| **input == original)
            .unwrap() = snapshot;
        after.evidence.push(provenance);
        let output_free = contracts::output_free_context_binding(root, &observed, attempt, task)?;
        AssignmentContextRecoveryAudit {
            schema_version: if output_free.is_some() { 2 } else { 1 },
            request_id: operation.operation_id(),
            attempt_digest: contracts::assignment_attempt_digest(attempt)?,
            before: current_assignment.clone(),
            after,
            original,
            output_free,
        }
    };
    operation.input_digest = Some(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec_pretty(&audit)?)
    ));
    contracts::validate_assignment_context_recovery_inputs(root, task, &audit.before)?;
    drop(observation_lock.take());
    let _land = match land {
        Some(lock) => lock,
        None => crate::land_lock::acquire(&crate::land_lock::lock_path(root))?,
    };
    store.mutate_work_item_from_external(
        request.work,
        "retain original Writer assignment context",
        Some(audit_path.clone()),
        |fresh| {
            // A prior invocation may have durably journaled this same change.
            // The store reconciles it before entering the reducer.
            if fresh
                .validate_recovery_operation(root, &operation)?
                .is_some()
            {
                let check = || -> Result<()> {
                    let (_, task) = contracts::assignment_context_recovery_target(
                        root,
                        fresh,
                        request.attempt,
                        request.task,
                        request.candidate,
                    )?;
                    ensure!(
                        task.writer_correction_assignment.as_ref() == Some(&audit.after),
                        "recovered assignment changed"
                    );
                    contracts::validate_assignment_context_recovery_inputs(root, task, &audit.after)
                };
                return check().map_err(|e| WorkModelError::RecoveryOperationIneligible {
                    attempt_id: request.attempt.into(),
                    reason: format!("{e:#}"),
                });
            }
            let apply = || -> Result<()> {
                ensure!(
                    fresh == &observed,
                    "Work changed before context recovery publication"
                );
                let (attempt, task) = contracts::assignment_context_recovery_target(
                    root,
                    fresh,
                    request.attempt,
                    request.task,
                    request.candidate,
                )?;
                ensure!(
                    audit.attempt_digest == contracts::assignment_attempt_digest(attempt)?,
                    "Attempt changed before publication"
                );
                contracts::validate_assignment_context_recovery_inputs(root, task, &audit.before)?;
                contracts::validate_output_free_context_files(root, &audit)?;
                fresh.validate_recovery_operation(root, &operation)?;
                let (snapshot, provenance, provenance_bytes) =
                    contracts::assignment_context_snapshot(task, &audit.original)?;
                publish_recovery_snapshot(root, &snapshot.artifact.path, &bytes)?;
                publish_recovery_snapshot(root, &provenance.artifact.path, &provenance_bytes)?;
                publish_recovery_snapshot(root, &audit_path, &serde_json::to_vec_pretty(&audit)?)?;
                test_interrupt("audit");
                Ok(())
            };
            apply().map_err(|e| WorkModelError::RecoveryOperationIneligible {
                attempt_id: request.attempt.into(),
                reason: format!("{e:#}"),
            })?;
            let attempt = fresh
                .attempts
                .iter_mut()
                .find(|a| a.id == request.attempt)
                .unwrap();
            attempt
                .tasks
                .iter_mut()
                .find(|t| t.id == request.task)
                .unwrap()
                .writer_correction_assignment = Some(audit.after.clone());
            attempt.artifacts.push(ArtifactRef {
                producer_id: "assignment-context-recovery".into(),
                path: audit_path.clone(),
            });
            attempt.record_recovery_operation(
                &operation,
                Some(format!(
                    "fluent attempt run {} {}",
                    request.work, request.attempt
                )),
            )?;
            Ok(())
        },
    )?;
    Ok(operation.operation_id())
}

fn operation(request: &RecoveryRequest<'_>, original_digest: &str) -> RecoveryOperationRequest {
    let input = serde_json::to_vec(&(request.assignment_digest, request.input, original_digest))
        .expect("serialize string tuple");
    RecoveryOperationRequest::new(
        "assignment-context-recovery",
        request.work,
        request.attempt,
        Some(request.candidate.into()),
        Some(request.task.into()),
        None,
        None,
        Some(request.resolution.trim().into()),
        Some(format!("sha256:{:x}", Sha256::digest(input))),
    )
}

pub(crate) fn test_interrupt(point: &str) {
    #[cfg(feature = "test-support")]
    if std::env::var("FLUENT_TEST_ASSIGNMENT_CONTEXT_INTERRUPT").as_deref() == Ok(point) {
        std::process::exit(86);
    }
    let _ = point;
}
