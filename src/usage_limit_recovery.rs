//! Publish approval and resume one validated usage-limit-interrupted Writer.
pub use crate::usage_limit_contracts::RecoveryApproval;
use crate::usage_limit_contracts::*;
use crate::work_model::{self, ArtifactRef, AttemptStatus, TaskStatus, WorkItem, WorkModelStore};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

fn write_once(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    canonical_relative(relative)?;
    let path = root.join(relative);
    let mut cursor = root.to_path_buf();
    for part in Path::new(relative)
        .parent()
        .context("audit parent")?
        .components()
    {
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "audit path traverses an invalid directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&cursor)?,
            Err(e) => return Err(e.into()),
        }
    }
    if path.try_exists()? {
        ensure!(
            file_bytes(&path)? == bytes,
            "conflicting immutable recovery audit"
        );
        return Ok(());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                file_bytes(&path)? == bytes,
                "conflicting immutable recovery audit"
            );
        }
        Err(e) => return Err(e.error.into()),
    }
    Ok(())
}

/// Record one approval without launching or queueing a Writer.
pub fn approve(
    root: &Path,
    store: &WorkModelStore,
    work: &str,
    attempt_id: &str,
    resolution: &str,
) -> Result<RecoveryApproval> {
    ensure!(
        !resolution.trim().is_empty() && resolution.len() <= 4096,
        "supply a bounded explicit resolution"
    );
    let observed = store.read_work_item_snapshot(work)?;
    if let Some(receipt) = replay_approval(root, &observed, attempt_id, resolution.trim())? {
        return Ok(receipt);
    }
    inspect(root, &observed, attempt_id)?;
    let _land = crate::land_lock::acquire(&crate::land_lock::lock_path(root))?;
    let item = store.read_work_item(work)?;
    let approval = Approval { schema_version: 1, binding: inspect(root, &item, attempt_id)?, resolution: resolution.trim().into(),
        limitation: "Only the surviving final native stream proves this interruption. Earlier launches and the origin of partial edits cannot be reconstructed. Session rollouts are continuation material. Approval grants no completion, host-evidence, review, Tester, or Learner credit.".into() };
    let operation = request(&approval, APPROVE)?;
    let audit = audit_path(&approval)?;
    let existing = item.validate_recovery_operation(root, &operation)?;
    if existing.is_none() {
        store.mutate_work_item_from_external(
            work,
            "approve one usage-limit Writer retry",
            Some(audit.clone()),
            |fresh| {
                let current =
                    inspect(root, fresh, attempt_id).map_err(|e| invalid(attempt_id, e))?;
                if current != approval.binding {
                    return Err(invalid(
                        attempt_id,
                        "recovery inputs changed before publication",
                    ));
                }
                fresh.validate_recovery_operation(root, &operation)?;
                write_once(
                    root,
                    &audit,
                    &serde_json::to_vec_pretty(&approval).map_err(|e| invalid(attempt_id, e))?,
                )
                .map_err(|e| invalid(attempt_id, e))?;
                test_interrupt("audit");
                let attempt = fresh
                    .attempts
                    .iter_mut()
                    .find(|a| a.id == attempt_id)
                    .unwrap();
                attempt.artifacts.push(ArtifactRef {
                    producer_id: APPROVE.into(),
                    path: audit.clone(),
                });
                attempt.record_recovery_operation(
                    &operation,
                    Some(format!("fluent attempt run {work} {attempt_id}")),
                )?;
                Ok(())
            },
        )?;
    } else {
        ensure!(
            file_bytes(&root.join(&audit))? == serde_json::to_vec_pretty(&approval)?,
            "approval audit changed"
        );
    }
    Ok(RecoveryApproval {
        operation_id: operation.operation_id(),
        task_id: approval.binding.failed_task.id,
        audit_artifact: audit,
        partial_commit: approval.binding.partial.head,
    })
}

/// Explicitly prepare the same Writer while preserving its failed execution.
pub(crate) fn resume(
    root: &Path,
    store: &WorkModelStore,
    work: &str,
    attempt_id: &str,
) -> Result<bool> {
    let item = store.read_work_item(work)?;
    if !resume_ready(root, &item, attempt_id)? {
        return Ok(false);
    }
    store.mutate_work_item_from_external(
        work,
        "prepare approved usage-limit retry",
        None,
        |fresh| {
            let attempt = fresh.attempts.iter().find(|a| a.id == attempt_id).unwrap();
            let approval = current_approval(root, attempt)
                .map_err(|e| invalid(attempt_id, e))?
                .unwrap();
            if recorded(attempt, &approval, START).map_err(|e| invalid(attempt_id, e))?
                && attempt
                    .tasks
                    .last()
                    .is_some_and(|t| t.status == TaskStatus::Executing)
            {
                unlaunched(root, attempt, &approval, false).map_err(|e| invalid(attempt_id, e))?;
                let prior = bind_file(
                    root,
                    &format!(
                        "{}/execution.json",
                        retry_area(&approval).map_err(|e| invalid(attempt_id, e))?
                    ),
                    &mut BTreeMap::new(),
                )
                .map_err(|e| invalid(attempt_id, e))?;
                let audit = format!(
                    "{}/setup-{}.json",
                    Path::new(&audit_path(&approval).map_err(|e| invalid(attempt_id, e))?)
                        .parent()
                        .unwrap()
                        .display(),
                    digest(&prior)
                );
                write_once(root, &audit, &prior).map_err(|e| invalid(attempt_id, e))?;
                let restored = fresh
                    .attempts
                    .iter_mut()
                    .find(|a| a.id == attempt_id)
                    .unwrap();
                *restored.tasks.last_mut().unwrap() =
                    prepared_task(&approval).map_err(|e| invalid(attempt_id, e))?;
                restored.status = AttemptStatus::Planned;
            }
            let attempt = fresh.attempts.iter().find(|a| a.id == attempt_id).unwrap();
            validate_pending(root, fresh, &approval, false).map_err(|e| invalid(attempt_id, e))?;
            if recorded(attempt, &approval, PREPARE).map_err(|e| invalid(attempt_id, e))? {
                if attempt
                    .tasks
                    .last()
                    .is_some_and(|task| task.status == TaskStatus::NeedsUser)
                {
                    let attempt = fresh
                        .attempts
                        .iter_mut()
                        .find(|a| a.id == attempt_id)
                        .unwrap();
                    *attempt.tasks.last_mut().unwrap() =
                        prepared_task(&approval).map_err(|e| invalid(attempt_id, e))?;
                    work_model::reopen_attempt(attempt);
                }
                return Ok(());
            }
            let path = retry_area(&approval).map_err(|e| invalid(attempt_id, e))?;
            write_once(
                root,
                &format!("{path}/write-baseline-commit"),
                approval.binding.source_candidate.as_bytes(),
            )
            .map_err(|e| invalid(attempt_id, e))?;
            let attempt = fresh
                .attempts
                .iter_mut()
                .find(|a| a.id == attempt_id)
                .unwrap();
            *attempt.tasks.last_mut().unwrap() =
                prepared_task(&approval).map_err(|e| invalid(attempt_id, e))?;
            work_model::reopen_attempt(attempt);
            attempt.record_recovery_operation(
                &request(&approval, PREPARE).map_err(|e| invalid(attempt_id, e))?,
                None,
            )?;
            Ok(())
        },
    )?;
    Ok(true)
}

pub(crate) fn record_start(
    root: &Path,
    item: &mut WorkItem,
    attempt_id: &str,
    task_id: &str,
) -> Result<()> {
    let attempt = item
        .attempts
        .iter_mut()
        .find(|a| a.id == attempt_id)
        .context("Attempt disappeared")?;
    let Some(approval) = current_approval(root, attempt)? else {
        return Ok(());
    };
    if approval.binding.failed_task.id == task_id {
        ensure!(!has_launched(root, &approval)?, "approval already consumed");
        attempt.record_recovery_operation(&request(&approval, START)?, None)?;
    }
    Ok(())
}

pub(crate) fn before_spawn(
    root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    transcript: &Path,
) -> Result<()> {
    if let Some(approval) = inspect_spawn(root, item, attempt_id, transcript)? {
        write_once(
            root,
            &launch_marker(&approval)?,
            request(&approval, START)?.operation_id().as_bytes(),
        )?;
    }
    Ok(())
}

/// Exercise the final read-only launch check without publishing a launch marker.
#[cfg(feature = "test-support")]
pub fn inspect_spawn_for_test(
    root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    transcript: &Path,
) -> Result<()> {
    inspect_spawn(root, item, attempt_id, transcript)?
        .context("expected a pending approved Writer launch")?;
    Ok(())
}
