//! Validate one completion retry after audited, output-free context recovery.
use super::*;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use work_model::{
    Attempt, AttemptStatus, PauseKind, RecoveryOperationRequest, TaskArtifactArea, WorkItem,
};

pub(crate) const APPROVE: &str = "writer-completion-context-retry";
pub(crate) const RESUME: &str = "writer-completion-context-resume";
pub(crate) const START: &str = "writer-completion-context-start";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Retry {
    pub schema_version: u32,
    pub before: Attempt,
    pub task_id: String,
    pub context: ArtifactRef,
    pub files: BTreeMap<String, String>,
    pub resolution: String,
}

fn has_retry_history(attempt: &Attempt) -> bool {
    attempt
        .recovery_operations
        .iter()
        .any(|r| r.operation_kind == APPROVE)
        || attempt.tasks.iter().any(|t| {
            t.kind == TaskKind::Write
                && t.status == TaskStatus::Failed
                && t.output.is_none()
                && t.input_artifacts.iter().any(|a| {
                    a.producer_id == "writer-completion-recovery"
                        && a.path
                            == work_model::work_artifact_path(
                                &attempt.work_item_id,
                                &attempt.id,
                                &format!("writer-completion-recoveries/{}/recovery.json", t.id),
                            )
                })
        })
}

pub(crate) fn recognized(root: &Path, item: &WorkItem, attempt: &Attempt) -> Result<bool> {
    if !has_retry_history(attempt) {
        return Ok(false);
    }
    if attempt
        .recovery_operations
        .iter()
        .any(|r| r.operation_kind == APPROVE)
    {
        // Recorded context authority must still reject stale or consumed retries.
        return Ok(true);
    }
    // The original completion approval can precede a launched quota failure.
    // Let the quota owner proceed only after it validates its separate approval.
    Ok(!crate::usage_limit_contracts::resume_ready(
        root,
        item,
        &attempt.id,
    )?)
}

pub(crate) fn inspect(
    root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    resolution: &str,
) -> Result<Retry> {
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .iter()
        .find(|a| a.id == attempt_id)
        .context("Attempt missing")?;
    ensure!(
        attempt.status == AttemptStatus::NeedsUser
            && matches!(
                attempt.pause_kind,
                Some(PauseKind::RoundCap | PauseKind::WriterCompletion)
            )
            && !attempt.cancellation_requested()
            && !attempt
                .recovery_operations
                .iter()
                .any(|r| r.operation_kind == APPROVE),
        "context retry requires fresh authorization for the unchanged paused failure"
    );
    let task = attempt
        .tasks
        .iter()
        .find(|t| t.kind == TaskKind::Write && t.status == TaskStatus::Failed && t.output.is_none())
        .context("context retry has no output-free failed Writer")?;
    let assignment = task
        .writer_correction_assignment
        .as_ref()
        .context("failed Writer assignment missing")?;
    assignment_context_recovery_target(
        root,
        item,
        attempt_id,
        &task.id,
        &assignment.source_candidate_commit,
    )?;
    validate_writer_correction_assignment(root, item, task, assignment)?;
    let mut recovered = None;
    for reference in attempt
        .artifacts
        .iter()
        .filter(|a| a.producer_id == "assignment-context-recovery")
    {
        let bytes = assignment_input_bytes(root, &reference.path)?;
        let audit: work_model::AssignmentContextRecoveryAudit = serde_json::from_slice(&bytes)?;
        if audit.after == *assignment && audit.output_free.is_some() {
            validate_output_free_context_files(root, &audit)?;
            ensure!(
                recovered.is_none(),
                "ambiguous output-free context recovery"
            );
            recovered = Some(reference.clone());
        }
    }
    let context = recovered
        .context("repair the bound stale assignment context before authorizing its retry")?;
    let mut files = output_free_context_binding(root, item, attempt, task)?
        .unwrap()
        .files;
    crate::usage_limit_contracts::bind_file(root, &context.path, &mut files)?;
    for input in assignment
        .source_task_inputs
        .iter()
        .chain(&assignment.evidence)
    {
        crate::usage_limit_contracts::bind_file(root, &input.artifact.path, &mut files)?;
    }
    Ok(Retry {
        schema_version: 1,
        before: attempt.clone(),
        task_id: task.id.clone(),
        context,
        files,
        resolution: resolution.into(),
    })
}

pub(crate) fn request(retry: &Retry, kind: &str) -> Result<RecoveryOperationRequest> {
    let task = failed_task(retry)?;
    let assignment = task
        .writer_correction_assignment
        .as_ref()
        .context("retry assignment missing")?;
    Ok(RecoveryOperationRequest::new(
        kind,
        &retry.before.work_item_id,
        &retry.before.id,
        Some(assignment.source_candidate_commit.clone()),
        Some(task.id.clone()),
        None,
        Some(task.workspace_access.writes[0].path.clone()),
        Some(retry.resolution.clone()),
        Some(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(retry)?)
        )),
    ))
}

pub(crate) fn failed_task(retry: &Retry) -> Result<&Task> {
    retry
        .before
        .tasks
        .iter()
        .find(|t| t.id == retry.task_id)
        .context("retry lost its original failed Task")
}

pub(crate) fn audit_ref(retry: &Retry) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        producer_id: APPROVE.into(),
        path: work_model::work_artifact_path(
            &retry.before.work_item_id,
            &retry.before.id,
            &format!(
                "writer-completion-context-retries/{}/recovery.json",
                retry.task_id
            ),
        ),
    })
}

pub(crate) fn prepared_task(retry: &Retry) -> Result<Task> {
    let mut task = failed_task(retry)?.clone();
    task.status = TaskStatus::Planned;
    task.started_at = None;
    task.completed_at = None;
    task.artifact_area = Some(TaskArtifactArea {
        path: work_model::work_artifact_path(
            &retry.before.work_item_id,
            &retry.before.id,
            &format!(
                "writer-completion-retry-runs/{}",
                request(retry, APPROVE)?.operation_id()
            ),
        ),
    });
    task.input_artifacts.push(audit_ref(retry)?);
    Ok(task)
}

pub(crate) fn current(root: &Path, attempt: &Attempt) -> Result<Option<Retry>> {
    let Some(operation) = attempt
        .recovery_operations
        .iter()
        .find(|r| r.operation_kind == APPROVE)
    else {
        return Ok(None);
    };
    ensure!(
        attempt
            .recovery_operations
            .iter()
            .filter(|r| r.operation_kind == APPROVE)
            .count()
            == 1,
        "ambiguous completion retry authority"
    );
    let reference = ArtifactRef {
        producer_id: APPROVE.into(),
        path: work_model::work_artifact_path(
            &attempt.work_item_id,
            &attempt.id,
            &format!(
                "writer-completion-context-retries/{}/recovery.json",
                operation
                    .task_id
                    .as_deref()
                    .context("retry operation lost its Task")?
            ),
        ),
    };
    ensure!(
        attempt.artifacts.contains(&reference),
        "completion retry lost its audit reference"
    );
    let bytes = assignment_input_bytes(root, &reference.path)?;
    let retry: Retry = serde_json::from_slice(&bytes)?;
    ensure!(
        retry.schema_version == 1
            && retry.before.id == attempt.id
            && retry.before.work_item_id == attempt.work_item_id
            && operation.matches_request(&request(&retry, APPROVE)?)
            && reference == audit_ref(&retry)?
            && bytes == serde_json::to_vec_pretty(&retry)?,
        "completion retry audit or operation changed"
    );
    Ok(Some(retry))
}

pub(crate) fn validate_pending(
    root: &Path,
    item: &WorkItem,
    retry: &Retry,
    own_lease: bool,
    starting: bool,
) -> Result<()> {
    let attempt = item
        .attempts
        .iter()
        .find(|a| a.id == retry.before.id)
        .context("retry Attempt missing")?;
    ensure!(
        attempt.cancellation == retry.before.cancellation && !attempt.cancellation_requested(),
        "a later cancellation invalidates context retry authority"
    );
    ensure!(
        !item
            .merge_candidates
            .iter()
            .any(|c| c.attempt_id == attempt.id),
        "retry candidate is already finalized"
    );
    for (path, digest) in &retry.files {
        ensure!(
            assignment_input_digest(root, path)? == *digest,
            "retained completion retry evidence changed: {path}"
        );
    }
    let mut expected = prepared_task(retry)?;
    let task = attempt
        .tasks
        .iter()
        .find(|t| t.id == retry.task_id)
        .context("retry Task missing")?;
    if starting {
        ensure!(
            task.status == TaskStatus::Executing && task.started_at.is_some(),
            "retry start has no reservation"
        );
        expected.status = task.status.clone();
        expected.started_at = task.started_at.clone();
    }
    ensure!(
        *task == expected,
        "completion retry Task changed or its authorization was consumed"
    );
    ensure!(
        attempt.tasks.len() == retry.before.tasks.len(),
        "completion retry history changed"
    );
    let index = retry
        .before
        .tasks
        .iter()
        .position(|t| t.id == retry.task_id)
        .unwrap();
    for (i, (prior, actual)) in retry.before.tasks.iter().zip(&attempt.tasks).enumerate() {
        if i == index {
            continue;
        }
        let mut expected = prior.clone();
        if i > index {
            expected.status = TaskStatus::Superseded;
            expected.completed_at = Some(
                attempt
                    .recovery_operations
                    .iter()
                    .find(|r| r.operation_kind == APPROVE)
                    .context("retry has no operation")?
                    .completed_at
                    .clone(),
            );
        }
        ensure!(
            *actual == expected,
            "completion retry changed a source or reservation"
        );
        ensure!(
            actual.status != TaskStatus::Executing,
            "another Task executes"
        );
        crate::usage_limit_contracts::unlocked(
            &crate::lease::task_lock_path(root, &item.id, &actual.id),
            false,
        )?;
        if let Some(area) = &actual.artifact_area {
            crate::usage_limit_contracts::unlocked(
                &root.join(&area.path).join("execution-owner.lock"),
                false,
            )?;
            ensure!(
                !crate::execution::execution_is_live(
                    &root.join(&area.path).join("execution.json")
                )?,
                "another execution remains live"
            );
        }
    }
    if !own_lease {
        crate::usage_limit_contracts::unlocked(
            &crate::lease::task_lock_path(root, &item.id, &task.id),
            false,
        )?;
    }
    crate::usage_limit_contracts::unlocked(
        &crate::lease::learner_lock_path(root, &item.id, &attempt.id),
        false,
    )?;
    let original = failed_task(retry)?;
    let assignment = original.writer_correction_assignment.as_ref().unwrap();
    let source = attempt
        .tasks
        .iter()
        .find(|t| t.id == assignment.source_task_id)
        .context("retry source missing")?;
    let output = source
        .output
        .as_ref()
        .context("retry source has no candidate")?;
    ensure!(
        attempt.current_candidate_commit() == Some(assignment.source_candidate_commit.as_str())
            && candidate_workspace_is_clean_at_managed(
                root,
                output,
                &assignment.source_candidate_commit
            ),
        "completion retry candidate changed or is dirty"
    );
    validate_writer_correction_assignment(root, item, task, assignment)?;
    writer_completion::validated_writer_context(root, item, task)?
        .context("retry completion authority missing")?;
    ensure!(
        attempt.evidence_recoveries == retry.before.evidence_recoveries
            && attempt.round_cap_extensions == retry.before.round_cap_extensions
            && attempt.coder_mapping == retry.before.coder_mapping,
        "completion retry authority changed"
    );
    let started = attempt
        .recovery_operations
        .iter()
        .any(|r| r.operation_kind == START);
    ensure!(
        started == starting,
        "completion retry authorization already consumed"
    );
    Ok(())
}

/// Queue admission and foreground resume share this check before any mutation.
pub(crate) fn resume_ready(root: &Path, item: &WorkItem, attempt_id: &str) -> Result<bool> {
    let attempt = item
        .attempts
        .iter()
        .find(|a| a.id == attempt_id)
        .context("Attempt missing")?;
    if !recognized(root, item, attempt)? {
        return Ok(false);
    }
    let retry = current(root, attempt)?.context("recover assignment context, then separately authorize repair-writer-completion before resume")?;
    let task = attempt
        .tasks
        .iter()
        .find(|t| t.id == retry.task_id)
        .context("retry Task missing")?;
    if task.status == TaskStatus::Complete && task.output.is_some() {
        return Ok(false);
    }
    validate_pending(root, item, &retry, false, false)?;
    Ok(true)
}

/// Derive guidance through the same contracts used by publication and resume.
pub(crate) fn status_action(root: &Path, attempt: &Attempt) -> Result<Option<Option<String>>> {
    if !has_retry_history(attempt) {
        return Ok(None);
    }
    let mut item =
        work_model::WorkModelStore::new(root).read_work_item_snapshot(&attempt.work_item_id)?;
    // Pause derivation runs before the reducer's target is published. Validate
    // that exact Attempt so replay does not refresh previously stale guidance.
    *item
        .attempts
        .iter_mut()
        .find(|a| a.id == attempt.id)
        .context("Attempt missing")? = attempt.clone();
    if !recognized(root, &item, attempt)? {
        return Ok(None);
    }
    if current(root, attempt)?.is_some() {
        return if resume_ready(root, &item, &attempt.id)? {
            Ok(Some(Some(format!(
                "fluent attempt run {} {}",
                attempt.work_item_id, attempt.id
            ))))
        } else {
            Ok(None)
        };
    }
    ensure!(
        attempt.status == AttemptStatus::NeedsUser
            && matches!(
                attempt.pause_kind,
                Some(PauseKind::RoundCap | PauseKind::WriterCompletion)
            )
            && !attempt.cancellation_requested(),
        "no eligible context recovery pause"
    );
    let task = attempt
        .tasks
        .iter()
        .find(|t| t.kind == TaskKind::Write && t.status == TaskStatus::Failed && t.output.is_none())
        .context("failed Writer missing")?;
    let assignment = task
        .writer_correction_assignment
        .as_ref()
        .context("assignment missing")?;
    assignment_context_recovery_target(
        root,
        &item,
        &attempt.id,
        &task.id,
        &assignment.source_candidate_commit,
    )?;
    if let Some(action) =
        stale_assignment_context_action(root, attempt, task, &assignment.source_candidate_commit)?
    {
        return Ok(Some(Some(action)));
    }
    inspect(root, &item, &attempt.id, "<describe the retry decision>")?;
    Ok(Some(Some(format!(
        "fluent attempt repair-writer-completion {} {} --resolution '<describe the retry decision>'",
        attempt.work_item_id, attempt.id
    ))))
}

pub(crate) fn validate_start(
    root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    task_id: &str,
    own_lease: bool,
) -> Result<()> {
    let attempt = item
        .attempts
        .iter()
        .find(|a| a.id == attempt_id)
        .context("Attempt missing")?;
    if !recognized(root, item, attempt)? {
        return Ok(());
    }
    let retry =
        current(root, attempt)?.context("same-Task context retry has not been authorized")?;
    if task_id != retry.task_id {
        ensure!(
            attempt.tasks.iter().any(|t| t.id == retry.task_id
                && t.status == TaskStatus::Complete
                && t.output.is_some()),
            "the original failed Writer must retain its assignment; a later Task cannot replace it"
        );
        return Ok(());
    }
    ensure!(
        attempt
            .recovery_operations
            .iter()
            .any(|r| r.matches_request(&request(&retry, RESUME).expect("serialize retry"))),
        "explicit Attempt resume must prepare the approved context retry"
    );
    validate_pending(root, item, &retry, own_lease, false)
}

pub(crate) fn pending_publications(item: &WorkItem, prior: Option<&WorkItem>) -> Vec<String> {
    item.attempts
        .iter()
        .flat_map(|a| &a.recovery_operations)
        .filter(|r| {
            matches!(r.operation_kind.as_str(), APPROVE | RESUME | START)
                && prior.is_none_or(|p| {
                    !p.attempts
                        .iter()
                        .any(|a| a.recovery_operation(&r.operation_id).is_some())
                })
        })
        .map(|r| r.operation_id.clone())
        .collect()
}

pub(crate) fn validate_publication(
    root: &Path,
    item: &WorkItem,
    operations: &[String],
) -> Result<()> {
    for operation in operations {
        let attempt = item
            .attempts
            .iter()
            .find(|a| a.recovery_operation(operation).is_some())
            .context("pending retry operation missing")?;
        let record = attempt.recovery_operation(operation).unwrap();
        let retry = current(root, attempt)?.context("pending retry audit missing")?;
        let starting = record.operation_kind == START;
        ensure!(
            record.matches_request(&request(&retry, &record.operation_kind)?),
            "pending retry operation changed"
        );
        validate_pending(root, item, &retry, starting, starting)?;
    }
    Ok(())
}

pub(crate) fn test_interrupt(point: &str) {
    #[cfg(feature = "test-support")]
    if std::env::var("FLUENT_TEST_CONTEXT_RETRY_INTERRUPT").as_deref() == Ok(point) {
        std::process::exit(86);
    }
    let _ = point;
}
