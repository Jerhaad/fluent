//! Recover one exact uncertain Architecture review through an audited Approach revision.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::work_model::{
    ApprovedPlanningRevision, ArtifactRef, Attempt, AttemptStatus, DurableTransitionMetadata,
    DurableTransitionOrigin, PauseKind, Task, TaskKind, TaskStatus, WorkModelError, WorkModelStore,
    WriterCompletionContract, WriterCompletionRequirementKind, now_iso8601, work_artifact_path,
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
    created_at: String,
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
    let audited_frontier = frontier(attempt)?;
    let old_contract = WriterCompletionContract::from_planning_context(context)
        .ok_or_else(|| anyhow::anyhow!("Work Item has no Writer completion contract"))?;
    let mut revised_context = context.clone();
    revised_context.approach = Some(replacement.clone());
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
        created_at: now_iso8601(),
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
    let source_review_ref = review_artifact(audited_frontier.source_review)?;
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
        let current = frontier(&fresh.attempts[index]).map_err(|error| WorkModelError::AttemptResumeIneligible { attempt_id: attempt_id.to_string(), reason: format!("Approach-revision frontier changed: {error:#}") })?;
        if current.candidate_commit != candidate_commit || current.source_review.id != source_id || current.failed_launch.as_ref().map(|task| &task.id) != failed_id.as_ref() {
            return Err(WorkModelError::AttemptResumeIneligible { attempt_id: attempt_id.to_string(), reason: "candidate or review frontier changed before Approach revision".to_string() });
        }
        fresh.planning_context = Some(revised_context.clone());
        let task_id = fresh.add_next_write_round(attempt_id, vec![audit_ref.clone(), source_review_ref.clone()])?;
        let attempt = &mut fresh.attempts[index];
        attempt.writer_completion_contract = Some(new_contract.clone());
        if !attempt.artifacts.contains(&audit_ref) { attempt.artifacts.push(audit_ref.clone()); }
        if let Some(failed_id) = &failed_id {
            let failed = attempt.tasks.iter_mut().find(|task| task.id == *failed_id)
                .ok_or_else(|| WorkModelError::TaskNotFound { id: failed_id.clone() })?;
            failed.status = TaskStatus::Superseded;
        }
        let writer = attempt.tasks.iter_mut().find(|task| task.id == task_id)
            .ok_or_else(|| WorkModelError::TaskNotFound { id: task_id.clone() })?;
        writer.role = CORRECTIVE_WRITER_ROLE.to_string();
        writer.instructions = Some(format!("Implement the approved Approach revision and resolve the Architecture finding. Human resolution: {resolution}"));
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
}

fn frontier(attempt: &Attempt) -> Result<Frontier<'_>> {
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
    let failed_launch = if terminal_failure {
        if last.kind != TaskKind::Review
            || last.status != TaskStatus::Failed
            || last.output.is_some()
        {
            bail!("terminal Attempt is not the exact artifact-free reviewer launch failure");
        }
        Some(last)
    } else {
        None
    };
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
    Ok(Frontier {
        source_review,
        failed_launch,
        candidate_commit,
    })
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
        store.create_work_item(&item).unwrap();
        (tmp, store)
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
        assert_eq!(
            stored.planning_context.unwrap().approach.as_deref(),
            Some(REPLACEMENT)
        );
        assert!(tmp.path().join(&revision.audit_artifact).is_file());
        assert_eq!(
            stored.attempts[0].tasks.last().unwrap().role,
            CORRECTIVE_WRITER_ROLE
        );
    }

    #[test]
    fn recovers_exact_terminal_launch_failure_after_uncertain_architecture() {
        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                let failed = item.attempts[0].tasks[1].clone();
                let mut failed = failed;
                failed.id = "attempt-1-review-2-architecture".to_string();
                failed.status = TaskStatus::Failed;
                failed.output = None;
                item.attempts[0].tasks.push(failed);
                item.attempts[0].status = AttemptStatus::Failed;
                Ok(())
            })
            .unwrap();
        revise(
            tmp.path(),
            &store,
            "work-1",
            "attempt-1",
            REPLACEMENT.as_bytes(),
            "Recover the failed launch.",
        )
        .unwrap();
        let attempt = &store.read_work_item("work-1").unwrap().attempts[0];
        assert_eq!(
            attempt
                .tasks
                .iter()
                .find(|task| task.id == "attempt-1-review-2-architecture")
                .unwrap()
                .status,
            TaskStatus::Superseded
        );
        assert_eq!(attempt.status, AttemptStatus::Planned);
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
        assert!(
            writer
                .input_artifacts
                .iter()
                .any(|artifact| artifact.producer_id == PRODUCER)
        );
        assert!(
            writer
                .input_artifacts
                .iter()
                .any(|artifact| artifact.path.ends_with("/review.md"))
        );
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
    }

    #[test]
    fn rejects_stale_drifted_and_unrelated_recovery_requests() {
        let (tmp, store) = fixture();
        store
            .mutate_work_item("work-1", |item| {
                item.attempts[0].pause_kind = Some(PauseKind::RoundCap);
                Ok(())
            })
            .unwrap();
        assert!(
            revise(
                tmp.path(),
                &store,
                "work-1",
                "attempt-1",
                REPLACEMENT.as_bytes(),
                "Approve recovery."
            )
            .is_err()
        );
    }
}
