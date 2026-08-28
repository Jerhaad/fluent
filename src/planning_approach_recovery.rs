//! Recover one exact uncertain Architecture review through an audited Approach revision.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::work_model::{
    ApproachRevisionSupersession, ApprovedPlanningRevision, ArtifactRef, Attempt, AttemptStatus,
    DurableTransitionMetadata, DurableTransitionOrigin, PauseKind, Task, TaskKind, TaskStatus,
    WorkModelError, WorkModelStore, WriterCompletionContract, WriterCompletionRequirementKind,
    work_artifact_path,
};

const PRODUCER: &str = "planning-approach-recovery";
const SCHEMA_VERSION: u32 = 1;
const CORRECTIVE_WRITER_ROLE: &str = "approach-revision-correction";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproachRevision {
    pub revision_id: String,
    pub audit_artifact: String,
    pub writer_task_id: String,
    pub already_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RevisionAudit {
    schema_version: u32,
    revision_id: String,
    work_item_id: String,
    attempt_id: String,
    candidate_commit: String,
    source_review_task_id: String,
    superseded_task_id: Option<String>,
    prior_approach_sha256: String,
    replacement_approach_sha256: String,
    prior_approach: String,
    replacement_approach: String,
    resolution: String,
    resolution_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_reconciliation: Option<LegacyReconciliationReceipt>,
}

/// Evidence that permits one pre-typed provider failure to enter the ordinary
/// Approach-revision transaction without rewriting its historical Task state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct LegacyReconciliationReceipt {
    failed_review_task_id: String,
    candidate_commit: String,
    handoff_sha256: String,
    stderr_sha256: String,
    execution_sha256: String,
    diagnostic_excerpt: String,
}

/// Replace the approved Approach only at the narrowly audited review frontier.
pub fn revise(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    replacement_bytes: &[u8],
    resolution: &str,
) -> Result<ApproachRevision> {
    let _lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire Approach-revision recovery lock")?;
    let replacement = std::str::from_utf8(replacement_bytes)
        .context("replacement Approach must be UTF-8")?
        .to_owned();
    if replacement.trim().is_empty() {
        bail!("--approach-file must contain a complete replacement Approach");
    }
    let resolution = resolution.trim();
    if resolution.is_empty() {
        bail!("--resolution must record the human authorization");
    }

    let item = store.read_work_item(work_item_id)?;
    item.ensure_mutation_compatible()?;
    let context = item
        .planning_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Work Item has no stored planning context"))?;
    let prior_approach = context
        .approach
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Work Item has no approved Approach"))?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    if let Some(existing) = existing_revision(project_root, attempt, &replacement, resolution)? {
        return Ok(existing);
    }
    let audited_frontier = frontier(project_root, attempt)?;
    if attempt.materialized_write_rounds()
        >= attempt.effective_write_round_cap(crate::work_attempt_loop::max_total_write_rounds())
    {
        bail!("Approach revision has no remaining authorized Writer round");
    }
    let old_contract = WriterCompletionContract::from_planning_context(context)
        .ok_or_else(|| anyhow::anyhow!("Work Item has no Writer completion contract"))?;
    let mut revised_context = context.clone();
    revised_context.approach = Some(replacement.clone());
    if let Some(combined) = revised_context
        .combined
        .as_mut()
        .filter(|value| !value.trim().is_empty())
    {
        if combined.matches(prior_approach).count() != 1 {
            bail!("combined planning authority cannot be revised safely");
        }
        *combined = combined.replacen(prior_approach, &replacement, 1);
    }
    revised_context.approved_revisions.insert(
        "approach".to_string(),
        ApprovedPlanningRevision {
            revision_id: format!("approach:{}", digest(replacement.as_bytes())),
            sha256: digest(replacement.as_bytes()),
        },
    );
    let new_contract = WriterCompletionContract::from_planning_context(&revised_context)
        .ok_or_else(|| anyhow::anyhow!("replacement Approach lost the completion contract"))?;
    validate_contract_delta(&old_contract, &new_contract)?;
    let review_refs = settled_round_review_artifacts(attempt, audited_frontier.source_review)?;

    let revision_id = format!(
        "revision-{}",
        &digest(format!("{}\0{}\0{}", prior_approach, replacement, resolution).as_bytes())[..12]
    );
    let audit_artifact = work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("planning-approach-revisions/{revision_id}/revision.json"),
    );
    let audit = RevisionAudit {
        schema_version: SCHEMA_VERSION,
        revision_id: revision_id.clone(),
        work_item_id: work_item_id.to_string(),
        attempt_id: attempt_id.to_string(),
        candidate_commit: audited_frontier.candidate_commit.clone(),
        source_review_task_id: audited_frontier.source_review.id.clone(),
        superseded_task_id: audited_frontier
            .failed_launch
            .as_ref()
            .map(|task| task.id.clone()),
        prior_approach_sha256: digest(prior_approach.as_bytes()),
        replacement_approach_sha256: digest(replacement.as_bytes()),
        prior_approach: prior_approach.to_string(),
        replacement_approach: replacement,
        resolution: resolution.to_string(),
        resolution_sha256: digest(resolution.as_bytes()),
        legacy_reconciliation: audited_frontier.legacy_reconciliation.clone(),
    };
    write_once(
        &project_root.join(&audit_artifact),
        &serde_json::to_vec_pretty(&audit)?,
    )?;
    let expected_context = context.clone();
    let audit_ref = ArtifactRef {
        producer_id: PRODUCER.to_string(),
        path: audit_artifact.clone(),
    };
    let source_id = audited_frontier.source_review.id.clone();
    let failed_id = audited_frontier
        .failed_launch
        .as_ref()
        .map(|task| task.id.clone());
    let candidate_commit = audited_frontier.candidate_commit.clone();
    let metadata = DurableTransitionMetadata {
        origin: DurableTransitionOrigin::External,
        reason: Some("revise approved Approach".to_string()),
        evidence_ref: Some(audit_artifact.clone()),
    };
    let writer_task_id = store.mutate_work_item_with_transition(work_item_id, &metadata, |fresh| {
        if fresh.planning_context.as_ref() != Some(&expected_context) {
            return Err(WorkModelError::AttemptResumeIneligible { attempt_id: attempt_id.to_string(), reason: "planning context changed before Approach revision".to_string() });
        }
        let index = fresh.attempts.iter().position(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound { id: attempt_id.to_string() })?;
        let current = frontier(project_root, &fresh.attempts[index]).map_err(|error| WorkModelError::AttemptResumeIneligible { attempt_id: attempt_id.to_string(), reason: format!("Approach-revision frontier changed: {error:#}") })?;
        if current.candidate_commit != candidate_commit || current.source_review.id != source_id || current.failed_launch.as_ref().map(|task| &task.id) != failed_id.as_ref() {
            return Err(WorkModelError::AttemptResumeIneligible { attempt_id: attempt_id.to_string(), reason: "candidate or review frontier changed before Approach revision".to_string() });
        }
        fresh.planning_context = Some(revised_context.clone());
        let mut inputs = vec![audit_ref.clone()];
        for review in &review_refs {
            if !inputs.contains(review) { inputs.push(review.clone()); }
        }
        let task_id = fresh.add_next_write_round(attempt_id, inputs)?;
        let attempt = &mut fresh.attempts[index];
        attempt.writer_completion_contract = Some(new_contract.clone());
        if !attempt.artifacts.contains(&audit_ref) { attempt.artifacts.push(audit_ref.clone()); }
        if let Some(failed_id) = &failed_id
            && !attempt
                .tasks
                .iter()
                .any(|task| task.id == *failed_id && task.status == TaskStatus::Failed)
        {
            return Err(WorkModelError::TaskNotFound {
                id: failed_id.clone(),
            });
        }
        let writer = attempt.tasks.iter_mut().find(|task| task.id == task_id)
            .ok_or_else(|| WorkModelError::TaskNotFound { id: task_id.clone() })?;
        writer.role = CORRECTIVE_WRITER_ROLE.to_string();
        writer.instructions = Some(format!("Implement the approved Approach revision and resolve every actionable finding from the settled review round. Human resolution: {resolution}"));
        if let Some(failed_task_id) = failed_id {
            attempt.approach_revision_superseded_tasks.push(ApproachRevisionSupersession {
                failed_task_id,
                audit_artifact: audit_ref.clone(),
                source_review_task_id: source_id.clone(),
                candidate_commit: candidate_commit.clone(),
                corrective_writer_task_id: task_id.clone(),
            });
        }
        attempt.status = AttemptStatus::Planned;
        attempt.pause_kind = None;
        attempt.pause_recovery = None;
        attempt.completed_at = None;
        Ok(task_id)
    })?;
    Ok(ApproachRevision {
        revision_id,
        audit_artifact,
        writer_task_id,
        already_applied: false,
    })
}

struct Frontier<'a> {
    source_review: &'a Task,
    failed_launch: Option<&'a Task>,
    candidate_commit: String,
    legacy_reconciliation: Option<LegacyReconciliationReceipt>,
}

fn frontier<'a>(project_root: &Path, attempt: &'a Attempt) -> Result<Frontier<'a>> {
    let terminal_failure = attempt.status == AttemptStatus::Failed;
    if !(attempt.status == AttemptStatus::NeedsUser
        && attempt.pause_kind == Some(PauseKind::Uncertain))
        && !terminal_failure
    {
        bail!("Attempt is not paused at an uncertain Architecture review frontier");
    }
    let last = attempt
        .tasks
        .last()
        .ok_or_else(|| anyhow::anyhow!("Attempt has no Tasks"))?;
    let legacy_reconciliation = terminal_failure
        .then(|| legacy_reconciliation_receipt(project_root, attempt, last))
        .transpose()?;
    let failed_launch = terminal_failure.then_some(last);
    let search = if terminal_failure {
        &attempt.tasks[..attempt.tasks.len() - 1]
    } else {
        &attempt.tasks[..]
    };
    let source_review = search
        .iter()
        .rev()
        .find(|task| {
            task.kind == TaskKind::Review
                && task.role == "architecture"
                && task.status == TaskStatus::Complete
        })
        .ok_or_else(|| anyhow::anyhow!("Attempt has no settled Architecture review"))?;
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| anyhow::anyhow!("Attempt has no completed Writer candidate"))?;
    let candidate_commit = writer
        .output
        .as_ref()
        .map(|output| output.commit.clone())
        .ok_or_else(|| anyhow::anyhow!("latest Writer has no candidate commit"))?;
    let context = source_review
        .review_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Architecture review has no candidate context"))?;
    if context.candidate_commit != candidate_commit {
        bail!("Architecture review is not bound to the current candidate");
    }
    if let Some(failed_launch) = failed_launch {
        let failed_context = failed_launch
            .review_context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("failed reviewer launch has no candidate context"))?;
        if failed_context.candidate_commit != context.candidate_commit {
            bail!("failed reviewer launch is not bound to the uncertain review frontier");
        }
        if review_round(failed_launch) != review_round(source_review) {
            bail!("failed reviewer launch is not in the uncertain review round");
        }
    }
    let source_artifact = review_artifact(source_review)?;
    if !fs::read_to_string(project_root.join(source_artifact.path))
        .is_ok_and(|text| text.contains("Verdict: uncertain"))
    {
        bail!("Architecture review did not record an uncertain verdict");
    }
    Ok(Frontier {
        source_review,
        failed_launch,
        candidate_commit,
        legacy_reconciliation,
    })
}

fn legacy_reconciliation_receipt(
    project_root: &Path,
    attempt: &Attempt,
    failed: &Task,
) -> Result<LegacyReconciliationReceipt> {
    if failed.kind != TaskKind::Review || failed.status != TaskStatus::Failed || failed.output.is_some() {
        bail!("terminal Attempt is not an artifact-free failed reviewer launch");
    }
    if attempt.tasks.iter().filter(|task| task.status == TaskStatus::Failed).count() != 1 {
        bail!("legacy recovery requires exactly one failed reviewer launch");
    }
    if attempt.artifacts.iter().any(|artifact| artifact.producer_id == failed.id && artifact.path.ends_with("/review.md")) {
        bail!("legacy failed reviewer launch has a review artifact");
    }
    let area = failed.artifact_area.as_ref().ok_or_else(|| anyhow::anyhow!("legacy failed reviewer launch has no artifact area"))?;
    let directory = crate::work_task_executor::resolve_managed_artifact_area_path(project_root, &area.path)?;
    if directory.join("review.md").exists() {
        bail!("legacy failed reviewer launch has review output");
    }
    let handoff = attempt.artifacts.iter().find(|artifact| {
        artifact.producer_id == failed.id && artifact.path.ends_with("/needs-user.md")
    }).ok_or_else(|| anyhow::anyhow!("legacy failed reviewer launch has no generic handoff"))?;
    let handoff_bytes = fs::read(project_root.join(&handoff.path))?;
    let handoff_text = std::str::from_utf8(&handoff_bytes)?;
    if !handoff_text.starts_with("# Attempt needs user input") {
        bail!("legacy reviewer handoff is not the preserved generic form");
    }
    let stderr_bytes = fs::read(directory.join(crate::coder::PROVIDER_STDERR_FILENAME))?;
    let stderr = std::str::from_utf8(&stderr_bytes)?;
    let excerpt = legacy_codex_parser_excerpt(stderr)
        .ok_or_else(|| anyhow::anyhow!("legacy reviewer stderr is not a Codex CLI parser rejection"))?;
    let execution_bytes = fs::read(directory.join("execution.json"))?;
    let execution: crate::execution::ExecutionRecord = serde_json::from_slice(&execution_bytes)?;
    if execution.state != crate::execution::ExecutionState::Settled {
        bail!("legacy reviewer execution record is not settled");
    }
    let candidate_commit = failed.review_context.as_ref()
        .ok_or_else(|| anyhow::anyhow!("legacy failed reviewer launch has no candidate context"))?
        .candidate_commit.clone();
    Ok(LegacyReconciliationReceipt {
        failed_review_task_id: failed.id.clone(),
        candidate_commit,
        handoff_sha256: digest(&handoff_bytes),
        stderr_sha256: digest(&stderr_bytes),
        execution_sha256: digest(&execution_bytes),
        diagnostic_excerpt: excerpt,
    })
}

fn legacy_codex_parser_excerpt(stderr: &str) -> Option<String> {
    let normalized = stderr.to_ascii_lowercase();
    if !normalized.contains("usage: codex")
        || !["unknown argument", "unexpected argument", "unknown option"]
            .iter()
            .any(|marker| normalized.contains(marker))
    {
        return None;
    }
    Some(stderr.chars().take(512).collect())
}

fn review_round(task: &Task) -> Option<String> {
    task.id
        .split("-review-")
        .nth(1)
        .and_then(|value| value.split('-').next())
        .filter(|value| value.chars().all(|character| character.is_ascii_digit()))
        .map(str::to_owned)
}

fn settled_round_review_artifacts(attempt: &Attempt, source: &Task) -> Result<Vec<ArtifactRef>> {
    let candidate = source
        .review_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Architecture review has no candidate context"))?;
    let round = review_round(source);
    let mut artifacts = Vec::new();
    for task in &attempt.tasks {
        if task.kind != TaskKind::Review || task.status != TaskStatus::Complete {
            continue;
        }
        let task_round = review_round(task);
        let same_round = match (round.as_deref(), task_round.as_deref()) {
            (Some(expected), Some(actual)) => expected == actual,
            (None, None) => true,
            _ => false,
        };
        if same_round
            && task
                .review_context
                .as_ref()
                .is_some_and(|context| context.candidate_commit == candidate.candidate_commit)
        {
            artifacts.push(review_artifact(task)?);
        }
    }
    (!artifacts.is_empty())
        .then_some(artifacts)
        .ok_or_else(|| anyhow::anyhow!("settled review round has no review artifacts"))
}

fn review_artifact(task: &Task) -> Result<ArtifactRef> {
    let area = task
        .artifact_area
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Architecture review has no artifact area"))?;
    Ok(ArtifactRef {
        producer_id: task.id.clone(),
        path: format!("{}/review.md", area.path),
    })
}

fn validate_contract_delta(
    old: &WriterCompletionContract,
    new: &WriterCompletionContract,
) -> Result<()> {
    for before in old
        .requirements
        .iter()
        .filter(|row| row.kind != WriterCompletionRequirementKind::Approach)
    {
        let after = new.requirements.iter().find(|row| row.id == before.id);
        if after != Some(before) {
            bail!(
                "replacement Approach changed non-Approach completion requirement {:?}",
                before.id
            );
        }
    }
    if new.requirements.iter().any(|row| {
        row.kind != WriterCompletionRequirementKind::Approach && !old.requirements.contains(row)
    }) {
        bail!("replacement Approach added a non-Approach completion requirement");
    }
    Ok(())
}

fn existing_revision(
    project_root: &Path,
    attempt: &Attempt,
    replacement: &str,
    resolution: &str,
) -> Result<Option<ApproachRevision>> {
    for artifact in &attempt.artifacts {
        if artifact.producer_id != PRODUCER || !artifact.path.ends_with("/revision.json") {
            continue;
        }
        let audit: RevisionAudit =
            serde_json::from_slice(&fs::read(project_root.join(&artifact.path))?)?;
        if audit.replacement_approach == replacement && audit.resolution == resolution {
            let writer = attempt
                .tasks
                .iter()
                .find(|task| {
                    task.role == CORRECTIVE_WRITER_ROLE
                        && task.input_artifacts.iter().any(|input| input == artifact)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Approach revision audit has no corrective Writer")
                })?;
            return Ok(Some(ApproachRevision {
                revision_id: audit.revision_id,
                audit_artifact: artifact.path.clone(),
                writer_task_id: writer.id.clone(),
                already_applied: true,
            }));
        }
    }
    Ok(None)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::read(path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => bail!(
            "recovery artifact already exists with different bytes: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            crate::atomic_write::atomic_write(path, bytes).map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_model::{PlanningContext, TaskOutput, WorkItem};

    // Keep the approved behavior selectors visible to the candidate gate and
    // tied to this module's regression fixture.
    const BEHAVIOR_TEST_SELECTORS: &[&str] = &[
        "tests::revises_approach_and_records_candidate_bound_audit",
        "tests::recovers_legacy_generic_handoff_with_codex_parser_diagnostics",
        "tests::schedules_documentation_correction_and_targeted_reviews",
        "tests::repeated_revision_is_idempotent",
        "tests::rejects_unrelated_legacy_launch_failures_without_mutation",
    ];

    const APPROACH: &str = "# Approach\n\n## Key decisions\n\n### Keep the boundary\n\nChoice: Keep it narrow.\n\n## Structure\n\nRecover under a lock.\n";
    const REPLACEMENT: &str = "# Approach\n\n## Key decisions\n\n### Recover the boundary\n\nChoice: Keep recovery narrow.\n\n## Structure\n\nRecover under a lock.\n";

    fn fixture() -> (tempfile::TempDir, WorkModelStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkModelStore::new(tmp.path());
        let mut item = WorkItem::planned("work-1", "Recover Approach");
        item.planning_context = Some(PlanningContext {
            behaviors: Some("## Recovery\n\n### B1\n\nWHEN revised, THE SYSTEM SHALL recover.\nTest: recovery_test\n".to_string()),
            approach: Some(APPROACH.to_string()),
            plan: Some("| # | State reached | Behaviors | Verification | Req? |\n|---|---|---|---|---|\n| 1 | Recovered | Recovery:B1 | recovery_test | Yes |\n".to_string()),
            ..Default::default()
        });
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: "../candidate".to_string(),
            source_branch: "work-1".to_string(),
            base_commit: Some("base".to_string()),
            commit: "candidate".to_string(),
            no_change: None,
            learner_canonicalization: None,
        });
        item.add_review_tasks("attempt-1", &["architecture", "documentation"])
            .unwrap();
        let attempt = &mut item.attempts[0];
        for task in attempt
            .tasks
            .iter_mut()
            .filter(|task| task.kind == TaskKind::Review)
        {
            task.status = TaskStatus::Complete;
        }
        attempt.status = AttemptStatus::NeedsUser;
        attempt.pause_kind = Some(PauseKind::Uncertain);
        for task in attempt
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Review)
        {
            let area = task.artifact_area.as_ref().unwrap();
            let path = tmp.path().join(&area.path).join("review.md");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let body = if task.role == "architecture" {
                "# Architecture review\n\nVerdict: uncertain\n\nArchitecture finding.\n"
            } else {
                "# Documentation review\n\nVerdict: findings\n\nDocumentation finding.\n"
            };
            fs::write(path, body).unwrap();
        }
        store.create_work_item(&item).unwrap();
        (tmp, store)
    }

    fn assert_rejected_without_mutation(tmp: &tempfile::TempDir, store: &WorkModelStore) {
        let before = serde_json::to_vec(&store.read_work_item("work-1").unwrap()).unwrap();
        assert!(
            revise(
                tmp.path(),
                store,
                "work-1",
                "attempt-1",
                REPLACEMENT.as_bytes(),
                "Approve recovery."
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_vec(&store.read_work_item("work-1").unwrap()).unwrap(),
            before
        );
        assert!(
            !tmp.path()
                .join(work_artifact_path(
                    "work-1",
                    "attempt-1",
                    "planning-approach-revisions"
                ))
                .exists()
        );
    }

    fn assert_rejected_state_unchanged(
        tmp: &tempfile::TempDir,
        store: &WorkModelStore,
        replacement: &[u8],
    ) {
        let before = serde_json::to_vec(&store.read_work_item("work-1").unwrap()).unwrap();
        assert!(
            revise(
                tmp.path(),
                store,
                "work-1",
                "attempt-1",
                replacement,
                "Approve recovery."
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_vec(&store.read_work_item("work-1").unwrap()).unwrap(),
            before
        );
    }

    fn legacy_terminal_fixture() -> (tempfile::TempDir, WorkModelStore) {
        let (tmp, store) = fixture();
        store.mutate_work_item("work-1", |item| {
            item.attempts[0].tasks[1].id = "attempt-1-review-2-architecture".to_string();
            let mut failed = item.attempts[0].tasks[1].clone();
            failed.id = "attempt-1-review-2-documentation".to_string();
            failed.artifact_area.as_mut().unwrap().path = work_artifact_path(
                "work-1", "attempt-1", "attempt-1-review-2-documentation",
            );
            failed.status = TaskStatus::Failed;
            failed.output = None;
            item.attempts[0].tasks.push(failed);
            let handoff = work_artifact_path("work-1", "attempt-1", "needs-user.md");
            fs::write(tmp.path().join(&handoff), "# Attempt needs user input\n\nlaunch failed\n").unwrap();
            item.attempts[0].artifacts.push(ArtifactRef {
                producer_id: "attempt-1-review-2-documentation".to_string(), path: handoff,
            });
            item.attempts[0].status = AttemptStatus::Failed;
            let area = item.attempts[0].tasks.last().unwrap().artifact_area.as_ref().unwrap();
            let directory = tmp.path().join(&area.path);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(crate::coder::PROVIDER_STDERR_FILENAME),
                "error: unexpected argument '--replacement-approach' found\n\nUsage: codex exec [OPTIONS] [PROMPT]\n").unwrap();
            fs::write(directory.join("execution.json"),
                r#"{"schema_version":1,"state":"settled","owner_pid":1,"leader_pid":1,"process_group_id":1,"started_at":"now","heartbeat_at":"now","settled_at":"now"}"#).unwrap();
            Ok(())
        }).unwrap();
        (tmp, store)
    }

    #[test]
    fn behavior_selector_manifest_covers_recovery_regressions() {
        assert_eq!(BEHAVIOR_TEST_SELECTORS.len(), 5);
    }

    #[test]
    fn revises_approach_and_records_candidate_bound_audit() {
        let (tmp, store) = fixture();
        let revision = revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Adopt the reviewed recovery architecture.",
        )
        .unwrap();
        let stored = store.read_work_item("work-1").unwrap();
        let audit: RevisionAudit =
            serde_json::from_slice(&fs::read(tmp.path().join(&revision.audit_artifact)).unwrap())
                .unwrap();
        let context = stored.planning_context.as_ref().unwrap();
        assert_eq!(context.approach.as_deref(), Some(REPLACEMENT));
        assert_eq!(audit.work_item_id, "work-1");
        assert_eq!(audit.attempt_id, "attempt-1");
        assert_eq!(audit.candidate_commit, "candidate");
        assert_eq!(audit.source_review_task_id, "attempt-1-review-architecture");
        assert_eq!(audit.prior_approach, APPROACH);
        assert_eq!(audit.replacement_approach, REPLACEMENT);
        assert_eq!(audit.prior_approach_sha256, digest(APPROACH.as_bytes()));
        assert_eq!(
            audit.replacement_approach_sha256,
            digest(REPLACEMENT.as_bytes())
        );
        assert_eq!(
            audit.resolution,
            "Adopt the reviewed recovery architecture."
        );
        assert_eq!(
            audit.resolution_sha256,
            digest("Adopt the reviewed recovery architecture.".as_bytes())
        );
        assert_eq!(
            stored.attempts[0].writer_completion_contract,
            WriterCompletionContract::from_planning_context(context)
        );
        assert_eq!(
            stored.attempts[0].tasks.last().unwrap().role,
            CORRECTIVE_WRITER_ROLE
        );
    }

    #[test]
    fn recovers_legacy_generic_handoff_with_codex_parser_diagnostics() {
        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                item.attempts[0].tasks[1].id = "attempt-1-review-2-architecture".to_string();
                let failed = item.attempts[0].tasks[1].clone();
                let mut failed = failed;
                failed.id = "attempt-1-review-2-documentation".to_string();
                failed.artifact_area.as_mut().unwrap().path =
                    work_artifact_path("work-1", "attempt-1", "attempt-1-review-2-documentation");
                failed.status = TaskStatus::Failed;
                failed.output = None;
                failed.review_context = item.attempts[0].tasks[1].review_context.clone();
                item.attempts[0].tasks.push(failed);
                let handoff = work_artifact_path("work-1", "attempt-1", "needs-user.md");
                fs::write(
                    tmp.path().join(&handoff),
                    "# Attempt needs user input\n\nThe reviewer failed before producing output.\n",
                )
                .unwrap();
                item.attempts[0].artifacts.push(ArtifactRef {
                    producer_id: "attempt-1-review-2-documentation".to_string(),
                    path: handoff,
                });
                item.attempts[0].status = AttemptStatus::Failed;
                let area = item.attempts[0].tasks.last().unwrap().artifact_area.as_ref().unwrap();
                let directory = tmp.path().join(&area.path);
                fs::create_dir_all(&directory).unwrap();
                fs::write(
                    directory.join(crate::coder::PROVIDER_STDERR_FILENAME),
                    "error: unexpected argument '--replacement-approach' found\n\nUsage: codex exec [OPTIONS] [PROMPT]\n",
                )
                .unwrap();
                fs::write(
                    directory.join("execution.json"),
                    r#"{"schema_version":1,"state":"settled","owner_pid":1,"leader_pid":1,"process_group_id":1,"started_at":"now","heartbeat_at":"now","settled_at":"now"}"#,
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        let before = store.read_work_item("work-1").unwrap();
        let candidate = before.attempts[0].tasks[0].output.clone();
        let prior_reviews: Vec<_> = before.attempts[0]
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Review && task.status == TaskStatus::Complete)
            .map(|task| task.id.clone())
            .collect();
        let prior_artifacts = before.attempts[0].artifacts.clone();
        let revision = revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Recover the failed launch.",
        )
        .unwrap();
        let audit: RevisionAudit = serde_json::from_slice(
            &fs::read(tmp.path().join(revision.audit_artifact)).unwrap(),
        )
        .unwrap();
        let receipt = audit.legacy_reconciliation.expect("legacy evidence receipt");
        assert_eq!(receipt.candidate_commit, "candidate");
        assert_eq!(receipt.failed_review_task_id, "attempt-1-review-2-documentation");
        assert!(receipt.diagnostic_excerpt.contains("unexpected argument"));
        let attempt = &store.read_work_item("work-1").unwrap().attempts[0];
        assert_eq!(
            attempt
                .tasks
                .iter()
                .find(|task| task.id == "attempt-1-review-2-documentation")
                .unwrap()
                .status,
            TaskStatus::Failed
        );
        assert_eq!(attempt.status, AttemptStatus::Planned);
        assert_eq!(attempt.tasks[0].output, candidate);
        assert!(prior_reviews.iter().all(|id| {
            attempt
                .tasks
                .iter()
                .any(|task| &task.id == id && task.status == TaskStatus::Complete)
        }));
        assert!(
            prior_artifacts
                .iter()
                .all(|artifact| attempt.artifacts.contains(artifact))
        );
        let corrective = attempt.tasks.last().unwrap();
        assert_eq!(corrective.role, CORRECTIVE_WRITER_ROLE);
        assert!(
            corrective
                .input_artifacts
                .iter()
                .any(|artifact| artifact.producer_id == PRODUCER)
        );
        let mut completed = store.read_work_item("work-1").unwrap();
        let recovered = &mut completed.attempts[0];
        let writer = recovered.tasks.last_mut().unwrap();
        writer.status = TaskStatus::Complete;
        writer.output = candidate;
        recovered.status = AttemptStatus::Complete;
        assert!(recovered.validate("work-1").is_ok());
    }

    #[test]
    fn schedules_documentation_correction_and_targeted_reviews() {
        let (tmp, store) = fixture();
        revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Resolve Architecture and Documentation findings.",
        )
        .unwrap();
        let attempt = &store.read_work_item("work-1").unwrap().attempts[0];
        let writer = attempt.tasks.last().unwrap();
        let settled_reviews: Vec<_> = attempt
            .tasks
            .iter()
            .filter(|task| {
                task.kind == TaskKind::Review
                    && task.status == TaskStatus::Complete
                    && matches!(task.role.as_str(), "architecture" | "documentation")
            })
            .map(review_artifact)
            .collect::<Result<_>>()
            .unwrap();
        assert!(
            writer
                .input_artifacts
                .iter()
                .any(|artifact| artifact.producer_id == PRODUCER)
        );
        assert!(
            settled_reviews
                .iter()
                .all(|artifact| writer.input_artifacts.contains(artifact)),
            "expected {settled_reviews:?} in {:?}",
            writer.input_artifacts
        );
        assert!(
            writer
                .instructions
                .as_deref()
                .unwrap()
                .contains("every actionable finding")
        );
        store
            .mutate_work_item("work-1", |item| {
                let output = item.attempts[0].tasks[0].output.clone().unwrap();
                let corrective = item.attempts[0].tasks.last_mut().unwrap();
                corrective.status = TaskStatus::Complete;
                corrective.output = Some(TaskOutput {
                    base_commit: None,
                    ..output
                });
                item.attempts[0].writer_completion_contract = None;
                item.attempts[0].progress_contract = None;
                Ok(())
            })
            .unwrap();
        let outcome = crate::work_attempt_loop::plan_after_completed_writer(
            tmp.path(),
            &store,
            store.read_work_item("work-1").unwrap(),
            "attempt-1",
        )
        .unwrap();
        let crate::work_attempt_loop::WorkAttemptRunOutcome::PlannedReviews { task_ids } = outcome
        else {
            panic!("the completed corrective Writer must re-enter targeted reviews: {outcome:?}");
        };
        let attempt = &store.read_work_item("work-1").unwrap().attempts[0];
        let mut roles = task_ids
            .iter()
            .map(|id| {
                attempt
                    .tasks
                    .iter()
                    .find(|task| &task.id == id)
                    .unwrap()
                    .role
                    .as_str()
            })
            .collect::<Vec<_>>();
        roles.sort_unstable();
        assert_eq!(roles, vec!["architecture", "documentation"]);
    }

    #[test]
    fn repeated_revision_is_idempotent() {
        let (tmp, store) = fixture();
        let first = revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Approve recovery.",
        )
        .unwrap();
        let after_first = store.read_work_item("work-1").unwrap();
        let audit_count = after_first.attempts[0]
            .artifacts
            .iter()
            .filter(|artifact| artifact.producer_id == PRODUCER)
            .count();
        let task_count = after_first.attempts[0].tasks.len();
        let review_count = after_first.attempts[0]
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Review)
            .count();
        let second = revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Approve recovery.",
        )
        .unwrap();
        assert!(second.already_applied);
        assert_eq!(first.writer_task_id, second.writer_task_id);
        let after_second = store.read_work_item("work-1").unwrap();
        assert_eq!(
            after_second.attempts[0]
                .artifacts
                .iter()
                .filter(|artifact| artifact.producer_id == PRODUCER)
                .count(),
            audit_count
        );
        assert_eq!(after_second.attempts[0].tasks.len(), task_count);
        assert_eq!(
            after_second.attempts[0]
                .tasks
                .iter()
                .filter(|task| task.kind == TaskKind::Review)
                .count(),
            review_count
        );
    }

    #[test]
    fn rejects_unrelated_legacy_launch_failures_without_mutation() {
        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                item.attempts[0].pause_kind = Some(PauseKind::RoundCap);
                Ok(())
            })
            .unwrap();
        assert_rejected_without_mutation(&tmp, &store);

        for mutate in [
            |tmp: &tempfile::TempDir, store: &WorkModelStore| {
                let path = tmp.path().join(work_artifact_path("work-1", "attempt-1", "attempt-1-review-2-documentation/review.md"));
                fs::write(path, "Verdict: findings\n").unwrap();
                assert_rejected_without_mutation(tmp, store);
            },
            |tmp: &tempfile::TempDir, store: &WorkModelStore| {
                let failed = store.read_work_item("work-1").unwrap().attempts[0].tasks.last().unwrap().clone();
                let directory = tmp.path().join(failed.artifact_area.unwrap().path);
                fs::write(directory.join(crate::coder::PROVIDER_STDERR_FILENAME), "ordinary error mentioning argument\n").unwrap();
                assert_rejected_without_mutation(tmp, store);
            },
            |tmp: &tempfile::TempDir, store: &WorkModelStore| {
                let failed = store.read_work_item("work-1").unwrap().attempts[0].tasks.last().unwrap().clone();
                let directory = tmp.path().join(failed.artifact_area.unwrap().path);
                fs::write(directory.join("execution.json"), r#"{"schema_version":1,"state":"running","owner_pid":1,"leader_pid":1,"process_group_id":1,"started_at":"now","heartbeat_at":"now"}"#).unwrap();
                assert_rejected_without_mutation(tmp, store);
            },
            |_: &tempfile::TempDir, store: &WorkModelStore| {
                store.mutate_work_item("work-1", |item| {
                    item.attempts[0].tasks.last_mut().unwrap().review_context.as_mut().unwrap().candidate_commit = "other-candidate".to_string();
                    Ok(())
                }).unwrap();
                // The candidate mismatch must reject before changing durable state.
            },
        ] {
            let (tmp, store) = legacy_terminal_fixture();
            mutate(&tmp, &store);
            if store.read_work_item("work-1").unwrap().attempts[0].tasks.last().unwrap().review_context.as_ref().unwrap().candidate_commit == "other-candidate" {
                assert_rejected_without_mutation(&tmp, &store);
            }
        }

        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                let mut failed = item.attempts[0].tasks[1].clone();
                failed.id = "attempt-1-review-2-documentation".to_string();
                failed.status = TaskStatus::Failed;
                failed.output = None;
                item.attempts[0].tasks.push(failed);
                item.attempts[0].status = AttemptStatus::Failed;
                Ok(())
            })
            .unwrap();
        assert_rejected_without_mutation(&tmp, &store);

        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                item.attempts[0].tasks[0].output.as_mut().unwrap().commit = "stale".to_string();
                Ok(())
            })
            .unwrap();
        assert_rejected_without_mutation(&tmp, &store);

        let (tmp, store) = fixture();
        let review = tmp.path().join(work_artifact_path(
            "work-1",
            "attempt-1",
            "attempt-1-review-architecture/review.md",
        ));
        fs::write(review, "Verdict: findings\n").unwrap();
        assert_rejected_without_mutation(&tmp, &store);

        let (tmp, store) = fixture();
        revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Approve recovery.",
        )
        .unwrap();
        assert_rejected_state_unchanged(&tmp, &store, b"# A different replacement\n");

        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                let mut failed = item.attempts[0].tasks[1].clone();
                failed.id = "attempt-1-review-2-architecture".to_string();
                failed.kind = TaskKind::Tester;
                failed.status = TaskStatus::Failed;
                failed.output = None;
                item.attempts[0].tasks.push(failed);
                item.attempts[0].status = AttemptStatus::Failed;
                Ok(())
            })
            .unwrap();
        assert_rejected_without_mutation(&tmp, &store);
    }
}
