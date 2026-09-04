//! Audited recovery for a rejected, tree-identical candidate metadata gate.
//!
//! A candidate-gate rejection is normally resumed by planning an ordinary
//! corrective Writer.  When commit metadata is the only failed check, or when
//! `project-check` is the one additional failure and its canonical stored hook
//! log contains only matching metadata diagnostics, the Writer may rewrite
//! commit messages without changing the candidate tree or protected ancestry.
//! This module records the operator's decision and the exact gate it applies
//! to, then schedules that Writer through the normal candidate gate, Tester,
//! review, and Learner path.
//! Git remains the authority for the resulting history; Fluent records why the
//! history rewrite was authorized and which Attempt owns it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::candidate_gate::{
    CandidateCheckStatus, CandidateGateDisposition, CandidateGatePhase,
    authorized_tree_identical_metadata_rewrite_base, read_prior_writer_gate,
    validate_prior_writer_gate,
};
use crate::follow_up::content_digest;
use crate::git;
use crate::work_model::{
    ArtifactRef, Attempt, AttemptReviewState, AttemptStatus,
    CANDIDATE_METADATA_DERIVED_PROJECT_CHECK_MARKER, CANDIDATE_METADATA_REPAIR_ROLE, Task,
    TaskKind, TaskStatus, WorkModelError, WorkModelStore,
    candidate_metadata_derived_project_check_path, now_iso8601,
    resolve_managed_sibling_workspace_path, work_artifact_path,
};

pub const PRODUCER: &str = "candidate-metadata-recovery";
const SCHEMA_VERSION: u32 = 1;
const MAX_RESOLUTION_BYTES: usize = 4_096;

/// Result of requesting the one audited metadata-rewrite Writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMetadataRepair {
    pub task_id: String,
    pub candidate_commit: String,
    pub protected_base_commit: String,
    pub audit_artifact: String,
    pub already_authorized: bool,
}

/// The immutable authority supplied to the corrective Writer.  The replacement
/// commit is intentionally not guessed here: the Writer produces it, and the
/// candidate gate proves that its tree is identical and its protected base is
/// preserved before any reviewer runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct MetadataRepairAuthority {
    schema_version: u32,
    work_item_id: String,
    attempt_id: String,
    source_writer_task_id: String,
    source_gate_artifact: String,
    source_gate_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_project_check_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_project_check_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_review_artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_review_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_finding_id: Option<String>,
    source_candidate_commit: String,
    protected_base_commit: String,
    candidate_tree: String,
    resolution: String,
    resolution_sha256: String,
    requested_at: String,
}

struct EligibleMetadataRepair {
    source_writer: Task,
    source_candidate_commit: String,
    protected_base_commit: String,
    candidate_tree: String,
    gate_ref: ArtifactRef,
    gate_path: String,
    project_check_ref: Option<ArtifactRef>,
    source_review_ref: Option<ArtifactRef>,
    source_finding_id: Option<String>,
}

/// Return the exact public command when the current Attempt has this recovery
/// shape.  Candidate-gate handoffs use this so a generated needs-user file
/// always names an eligible transition rather than a generic resume that may
/// leave the Writer's history policy ambiguous.
pub fn recovery_action(project_root: &Path, attempt: &Attempt) -> Option<String> {
    inspect_eligible(project_root, attempt).ok().map(|_| {
        format!(
            "fluent attempt repair-candidate-metadata {} {} --resolution \"<describe the approved metadata rewrite>\"",
            attempt.work_item_id, attempt.id
        )
    })
}

/// Schedule one audited corrective Writer for the exact current metadata gate.
/// Calling this command again with the same decision returns the existing
/// Writer; it never creates a second recovery task.
pub fn repair(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    resolution: &str,
) -> Result<CandidateMetadataRepair> {
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire candidate metadata recovery lock")?;
    let resolution = normalize_resolution(resolution)?;
    let item = store.read_work_item(work_item_id)?;
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;

    if let Some(existing) = existing_repair(project_root, attempt, &resolution)? {
        return Ok(existing);
    }

    if item
        .merge_candidates
        .iter()
        .any(|candidate| candidate.attempt_id == attempt_id)
    {
        bail!("candidate metadata recovery is not eligible after a Merge Candidate exists");
    }
    if item
        .attempts
        .last()
        .is_none_or(|current| current.id != attempt_id)
    {
        bail!("candidate metadata recovery requires the current Attempt");
    }
    let eligible = inspect_eligible(project_root, attempt)?;
    let write_rounds = attempt.materialized_write_rounds();
    // A Documentation review-derived metadata repair is an explicitly
    // authorized history operation, not another open-ended content attempt.
    // It may therefore consume one recovery Writer even when the ordinary
    // correction budget is exhausted. Candidate-gate-derived repairs retain
    // the normal cap and still require a separate round-cap approval.
    if write_rounds
        >= attempt.effective_write_round_cap(crate::work_attempt_loop::max_total_write_rounds())
        && eligible.source_review_ref.is_none()
    {
        bail!(
            "candidate metadata recovery has no remaining authorized Writer round; approve a round-cap extension first"
        );
    }

    let next_round = attempt
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Write)
        .count()
        + 1;
    let task_id = format!("{attempt_id}-write-{next_round}");
    let requested_at = now_iso8601();
    let authority_rel = work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("candidate-metadata-repairs/{task_id}/repair.json"),
    );
    let authority = MetadataRepairAuthority {
        schema_version: SCHEMA_VERSION,
        work_item_id: work_item_id.to_string(),
        attempt_id: attempt_id.to_string(),
        source_writer_task_id: eligible.source_writer.id.clone(),
        source_gate_artifact: eligible.gate_path.clone(),
        source_gate_sha256: digest_path(project_root, &eligible.gate_path)?,
        source_project_check_artifact: eligible
            .project_check_ref
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        source_project_check_sha256: eligible
            .project_check_ref
            .as_ref()
            .map(|artifact| digest_path(project_root, &artifact.path))
            .transpose()?,
        source_review_artifact: eligible
            .source_review_ref
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        source_review_sha256: eligible
            .source_review_ref
            .as_ref()
            .map(|artifact| digest_path(project_root, &artifact.path))
            .transpose()?,
        source_finding_id: eligible.source_finding_id.clone(),
        source_candidate_commit: eligible.source_candidate_commit.clone(),
        protected_base_commit: eligible.protected_base_commit.clone(),
        candidate_tree: eligible.candidate_tree.clone(),
        resolution: resolution.clone(),
        resolution_sha256: content_digest(resolution.as_bytes()),
        requested_at: requested_at.clone(),
    };
    let authority_path = project_root.join(&authority_rel);
    fs::create_dir_all(
        authority_path
            .parent()
            .expect("candidate metadata authority path has a parent"),
    )?;
    crate::atomic_write::atomic_write(&authority_path, &serde_json::to_vec_pretty(&authority)?)
        .with_context(|| {
            format!(
                "write candidate metadata recovery authority {}",
                authority_path.display()
            )
        })?;

    let authority_ref = ArtifactRef {
        producer_id: PRODUCER.to_string(),
        path: authority_rel.clone(),
    };
    let expected = item.clone();
    let source_writer_id = eligible.source_writer.id.clone();
    let source_candidate_commit = eligible.source_candidate_commit.clone();
    let protected_base_commit = eligible.protected_base_commit.clone();
    let gate_ref = eligible.gate_ref.clone();
    let project_check_ref = eligible.project_check_ref.clone();
    let audit_artifact = authority_rel.clone();
    let stored_task_id = store.mutate_work_item_from_external(
        work_item_id,
        format!("authorize candidate metadata rewrite for {attempt_id}"),
        Some(authority_rel.clone()),
        |fresh| {
            if fresh != &expected {
                return Err(WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: "the candidate-gate frontier changed while metadata recovery was being authorized".to_string(),
                });
            }
            let current_attempt = fresh
                .attempts
                .iter()
                .find(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound {
                    id: attempt_id.to_string(),
                })?;
            let current = inspect_eligible(project_root, current_attempt).map_err(|error| {
                WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: format!("candidate metadata frontier changed: {error:#}"),
                }
            })?;
            if current.source_writer.id != source_writer_id
                || current.source_candidate_commit != source_candidate_commit
                || current.protected_base_commit != protected_base_commit
                || current.gate_ref != gate_ref
                || current.project_check_ref != project_check_ref
            {
                return Err(WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: "the candidate-gate frontier changed before metadata recovery was recorded".to_string(),
                });
            }

            // The source Writer gate is always carried into the recovery
            // Writer. A Documentation-derived repair also carries the review
            // that authorized it; keeping both bindings lets the Work Model
            // validate the special Writer shape and lets the later candidate
            // gate re-check the exact source evidence.
            let mut source_evidence = vec![gate_ref.clone()];
            if let Some(review_ref) = eligible.source_review_ref.clone() {
                source_evidence.push(review_ref);
            }
            if let Some(project_check_ref) = project_check_ref.clone() {
                source_evidence.push(project_check_ref);
            }
            source_evidence.push(authority_ref.clone());
            let task_id = fresh.add_next_write_round(attempt_id, source_evidence)?;
            let attempt = fresh
                .attempts
                .iter_mut()
                .find(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound {
                    id: attempt_id.to_string(),
                })?;
            if !attempt.artifacts.contains(&authority_ref) {
                attempt.artifacts.push(authority_ref.clone());
            }
            let writer = attempt
                .tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .ok_or_else(|| WorkModelError::TaskNotFound {
                    id: task_id.clone(),
                })?;
            writer.role = CANDIDATE_METADATA_REPAIR_ROLE.to_string();
            let instructions = format!(
                "Perform only the audited tree-identical commit-metadata rewrite described by {}. Preserve the candidate tree and protected base; Fluent will verify the resulting candidate.",
                authority_rel
            );
            writer.instructions = Some(if let Some(project_check_ref) = &project_check_ref {
                format!(
                    "{instructions}{CANDIDATE_METADATA_DERIVED_PROJECT_CHECK_MARKER}{}",
                    project_check_ref.path
                )
            } else {
                instructions
            });
            attempt.status = AttemptStatus::Planned;
            attempt.pause_kind = None;
            attempt.completed_at = None;
            attempt.review_state = Some(AttemptReviewState::Failed);
            Ok(task_id)
        },
    )?;

    Ok(CandidateMetadataRepair {
        task_id: stored_task_id,
        candidate_commit: source_candidate_commit,
        protected_base_commit,
        audit_artifact,
        already_authorized: false,
    })
}

/// Descriptive alias for callers that treat this operation as an authorization
/// request, matching the other public recovery modules.
pub fn request(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    resolution: &str,
) -> Result<CandidateMetadataRepair> {
    repair(project_root, store, work_item_id, attempt_id, resolution)
}

/// Return whether an already-authorized metadata recovery Writer is safe for a
/// scheduler resume.  The recovery command leaves the Attempt in `planned`
/// state so the normal Attempt loop runs the planned Writer directly; queue
/// admission must therefore recognize this audited exception without reopening
/// the original candidate-gate pause (which would create a duplicate Writer).
pub fn is_resume_ready(
    project_root: &Path,
    item: &crate::work_model::WorkItem,
    attempt_id: &str,
) -> Result<bool> {
    let Some(attempt) = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
    else {
        return Ok(false);
    };
    if item
        .attempts
        .last()
        .is_none_or(|current| current.id != attempt_id)
    {
        return Ok(false);
    }
    let Some(task) =
        attempt.tasks.iter().rev().find(|task| {
            task.kind == TaskKind::Write && task.role == CANDIDATE_METADATA_REPAIR_ROLE
        })
    else {
        return Ok(false);
    };
    if attempt.status != AttemptStatus::Planned
        || attempt.pause_kind.is_some()
        || task.status != TaskStatus::Planned
        || task.output.is_some()
    {
        return Ok(false);
    }
    let authority_ref = task
        .input_artifacts
        .iter()
        .find(|artifact| artifact.producer_id == PRODUCER)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "metadata recovery Writer {:?} has no authority artifact",
                task.id
            )
        })?;
    let authority = read_authority(project_root, attempt, task, authority_ref)?;
    validate_planned_repair(project_root, attempt, task, &authority)?;
    Ok(true)
}

fn existing_repair(
    project_root: &Path,
    attempt: &Attempt,
    resolution: &str,
) -> Result<Option<CandidateMetadataRepair>> {
    let Some(task) = attempt.tasks.iter().rev().find(|task| {
        if task.kind != TaskKind::Write || task.role != CANDIDATE_METADATA_REPAIR_ROLE {
            return false;
        }
        let Some(authority_ref) = task
            .input_artifacts
            .iter()
            .find(|artifact| artifact.producer_id == PRODUCER)
        else {
            return false;
        };
        let Ok(authority) = read_authority(project_root, attempt, task, authority_ref) else {
            return false;
        };
        let Ok(current) = inspect_candidate_frontier(project_root, attempt, Some(task.id.as_str()))
            .or_else(|_| {
                inspect_documentation_review_frontier(project_root, attempt, Some(task.id.as_str()))
            })
        else {
            return false;
        };
        authority.source_writer_task_id == current.source_writer.id
            && authority.source_candidate_commit == current.source_candidate_commit
    }) else {
        // A previous metadata rewrite belongs to an older candidate frontier.
        // It remains immutable history, but must not prevent a later audited
        // repair for a newly rejected tree-identical candidate.
        return Ok(None);
    };
    let authority_ref = task
        .input_artifacts
        .iter()
        .find(|artifact| artifact.producer_id == PRODUCER)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "metadata recovery Writer {:?} has no authority artifact",
                task.id
            )
        })?;
    let authority = read_authority(project_root, attempt, task, authority_ref)?;
    if authority.resolution != resolution {
        bail!(
            "candidate metadata recovery was already authorized by Task {:?} with a different resolution",
            task.id
        );
    }
    if task.status == TaskStatus::Planned
        && attempt.status == AttemptStatus::Planned
        && attempt.pause_kind.is_none()
    {
        validate_planned_repair(project_root, attempt, task, &authority)?;
        return Ok(Some(CandidateMetadataRepair {
            task_id: task.id.clone(),
            candidate_commit: authority.source_candidate_commit,
            protected_base_commit: authority.protected_base_commit,
            audit_artifact: authority_ref.path.clone(),
            already_authorized: true,
        }));
    }
    bail!(
        "candidate metadata recovery Writer {:?} has already advanced to {}; follow the Attempt's current next action",
        task.id,
        task.status.as_str()
    );
}

fn inspect_eligible(project_root: &Path, attempt: &Attempt) -> Result<EligibleMetadataRepair> {
    if attempt.status != AttemptStatus::NeedsUser {
        bail!("Attempt is not paused at a recoverable metadata frontier");
    }
    // This also rejects a concurrent Writer, failed Task, or unsupported pause
    // shape before we create an audited recovery task.
    attempt.ensure_resume_ready()?;
    if attempt.is_candidate_gate_pause() {
        inspect_candidate_frontier(project_root, attempt, None)
    } else if attempt.pause_kind == Some(crate::work_model::PauseKind::WriterCompletion) {
        inspect_documentation_review_frontier(project_root, attempt, None)
    } else {
        bail!("Attempt is not paused at a candidate-gate or Documentation metadata frontier");
    }
}

/// Recognize a completed Documentation review that found one historical
/// commit-body line-length defect after a Writer deliberately made no content
/// change. The review is the authority for why a metadata rewrite is needed;
/// Git supplies the protected parent of the named commit and verifies that the
/// candidate is otherwise unchanged.
fn inspect_documentation_review_frontier(
    project_root: &Path,
    attempt: &Attempt,
    ignored_task_id: Option<&str>,
) -> Result<EligibleMetadataRepair> {
    let (writer_index, source_writer) = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| {
            anyhow::anyhow!("Documentation metadata frontier has no completed Writer")
        })?;
    let source_writer = source_writer.clone();
    let output = source_writer
        .output
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Documentation metadata Writer has no candidate output"))?;
    let no_change = output.no_change.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Documentation metadata recovery requires the latest Writer to declare no change"
        )
    })?;
    if no_change.declaration_path.trim().is_empty() {
        bail!("Documentation metadata Writer has an empty no-change declaration path");
    }

    let gate_ref = attempt
        .artifacts
        .iter()
        .rev()
        .find(|artifact| {
            artifact.producer_id == source_writer.id
                && artifact
                    .path
                    .ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Documentation metadata frontier has no Writer gate"))?;
    let gate_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &gate_ref.path,
    )?;
    let gate = read_prior_writer_gate(&gate_path).map_err(anyhow::Error::msg)?;
    if gate.schema_version == 0
        || gate.schema_version > crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION
        || gate.phase != CandidateGatePhase::Writer
        || gate.disposition != CandidateGateDisposition::Passed
        || gate.candidate_commit != output.commit
        || gate.checks.iter().any(|check| {
            matches!(
                check.status,
                CandidateCheckStatus::Failed | CandidateCheckStatus::Error
            )
        })
    {
        bail!("Documentation metadata frontier does not have a passing current Writer gate");
    }

    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "Documentation metadata recovery workspace",
    )?;
    let head = git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "resolve Documentation metadata recovery HEAD",
    )?;
    if head != output.commit {
        bail!(
            "candidate HEAD changed from {} to {head}; ordinary candidate drift remains rejected",
            output.commit
        );
    }
    let status = git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
        "check Documentation metadata recovery cleanliness",
    )?;
    if !status.is_empty() {
        bail!("Documentation metadata recovery requires a clean candidate worktree:\n{status}");
    }
    let candidate_tree = git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", output.commit)],
        "resolve Documentation metadata candidate tree",
    )?;

    let matrix_path = source_writer
        .artifact_area
        .as_ref()
        .map(|area| {
            crate::work_task_executor::resolve_managed_artifact_area_path(project_root, &area.path)
                .map(|path| path.join(crate::work_task_executor::WRITER_COMPLETION_MATRIX_FILE))
        })
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("Documentation metadata Writer has no completion matrix"))?;
    let matrix: crate::work_model::WriterCompletionMatrix =
        serde_json::from_slice(&fs::read(&matrix_path).with_context(|| {
            format!(
                "read Documentation metadata Writer completion matrix {}",
                matrix_path.display()
            )
        })?)
        .with_context(|| {
            format!(
                "parse Documentation metadata Writer completion matrix {}",
                matrix_path.display()
            )
        })?;
    if matrix.task_id != source_writer.id {
        bail!(
            "Documentation metadata completion matrix belongs to {}, expected {}",
            matrix.task_id,
            source_writer.id
        );
    }
    let pending = matrix
        .rows
        .iter()
        .filter(|row| {
            row.kind == crate::work_model::WriterCompletionRequirementKind::Finding
                && row.state == crate::work_model::WriterCompletionState::Pending
        })
        .collect::<Vec<_>>();
    if pending.len() != 1 {
        bail!(
            "Documentation metadata recovery requires exactly one pending review finding; found {}",
            pending.len()
        );
    }
    let finding = pending[0];
    let finding_id = finding.id.clone();
    let Some(role) = finding
        .id
        .strip_prefix("finding:")
        .and_then(|value| value.split(':').next())
    else {
        bail!("Documentation metadata finding has an invalid identity");
    };
    if role != "documentation" {
        bail!(
            "metadata recovery from this pause only supports a Documentation finding, not {role}"
        );
    }
    let review_task = attempt
        .tasks
        .iter()
        .find(|task| {
            task.kind == TaskKind::Review
                && task.role == role
                && task.status == TaskStatus::Complete
                && task
                    .artifact_area
                    .as_ref()
                    .is_some_and(|area| format!("{}/review.md", area.path) == finding.source)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Documentation metadata finding has no matching review Task")
        })?;
    let review_ref = ArtifactRef {
        producer_id: review_task.id.clone(),
        path: finding.source.clone(),
    };
    let review_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &review_ref.path,
    )?;
    let review_source = fs::read_to_string(&review_path).with_context(|| {
        format!(
            "read Documentation metadata finding review {}",
            review_path.display()
        )
    })?;
    let finding_title = finding
        .requirement
        .strip_prefix("Address documentation review finding: ")
        .unwrap_or(&finding.requirement);
    if crate::review::extract_verdict(&review_source) != crate::review::Verdict::Fail
        || !crate::review::open_finding_titles(&review_source)
            .iter()
            .any(|title| title == finding_title)
    {
        bail!("Documentation metadata finding review is no longer an unchanged failing report");
    }
    if review_task
        .review_context
        .as_ref()
        .is_none_or(|context| context.candidate_commit != output.commit)
    {
        bail!("Documentation metadata finding is not bound to the current candidate");
    }
    let protected_base_commit = parse_commit_body_length_finding(
        &workspace,
        &review_source,
        &finding.requirement,
        &output.commit,
    )?;
    if writer_index + 1 < attempt.tasks.len()
        && attempt.tasks[writer_index + 1..].iter().any(|task| {
            task.status != TaskStatus::Complete
                && task.status != TaskStatus::Superseded
                && Some(task.id.as_str()) != ignored_task_id
        })
    {
        bail!("Documentation metadata frontier has unsettled Tasks after the current Writer");
    }
    Ok(EligibleMetadataRepair {
        source_writer,
        source_candidate_commit: output.commit,
        protected_base_commit,
        candidate_tree,
        gate_ref: gate_ref.clone(),
        gate_path: gate_ref.path.clone(),
        project_check_ref: None,
        source_review_ref: Some(review_ref),
        source_finding_id: Some(finding_id),
    })
}

fn parse_commit_body_length_finding(
    workspace: &Path,
    review_source: &str,
    requirement: &str,
    candidate: &str,
) -> Result<String> {
    let requirement_lower = requirement.to_ascii_lowercase();
    if !requirement_lower.contains("commit body")
        || !requirement_lower.contains("line-length")
        || !requirement_lower.contains("blocking")
    {
        bail!("Documentation finding is not the supported commit-body line-length finding");
    }
    let commits = review_source
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("commit"))
        .flat_map(extract_full_commit_ids)
        .collect::<Vec<_>>();
    let mut commits = commits;
    commits.sort();
    commits.dedup();
    let [target] = commits.as_slice() else {
        bail!(
            "Documentation metadata finding must name exactly one commit; found {}",
            commits.len()
        );
    };
    if target == candidate {
        bail!("Documentation metadata finding names the candidate tip, not a historical commit");
    }
    let parent_line = git::run_stdout(
        workspace,
        &["rev-list", "--parents", "-n", "1", target],
        "resolve Documentation metadata finding commit parent",
    )?;
    let parents = parent_line.split_whitespace().collect::<Vec<_>>();
    if parents.len() != 2 || parents[0] != target {
        bail!("Documentation metadata finding names a merge or malformed commit");
    }
    if !git::run_raw(
        workspace,
        &["merge-base", "--is-ancestor", target, candidate],
    )?
    .status
    .success()
    {
        bail!("Documentation metadata finding names a commit outside the current candidate");
    }
    let message = git::run_stdout(
        workspace,
        &["show", "-s", "--format=%B", target],
        "read Documentation metadata finding commit message",
    )?;
    if message
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        <= 72
    {
        bail!("Documentation metadata finding no longer matches the named commit");
    }
    Ok(parents[1].to_string())
}

fn extract_full_commit_ids(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut ids = Vec::new();
    for start in 0..bytes.len().saturating_sub(39) {
        let end = start + 40;
        if end > bytes.len()
            || !bytes[start..end]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit())
            || (start > 0 && bytes[start - 1].is_ascii_hexdigit())
            || (end < bytes.len() && bytes[end].is_ascii_hexdigit())
        {
            continue;
        }
        // The review is normally ASCII, but do not slice a UTF-8 string at a
        // byte offset that falls inside a non-ASCII character.
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        ids.push(line[start..end].to_ascii_lowercase());
    }
    ids
}

fn inspect_candidate_frontier(
    project_root: &Path,
    attempt: &Attempt,
    ignored_task_id: Option<&str>,
) -> Result<EligibleMetadataRepair> {
    let (writer_index, source_writer) = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| {
            anyhow::anyhow!("candidate-gate metadata frontier has no completed Writer")
        })?;
    let source_writer = source_writer.clone();
    let output = source_writer
        .output
        .clone()
        .ok_or_else(|| anyhow::anyhow!("candidate-gate metadata Writer has no candidate output"))?;
    let gate_ref = attempt
        .artifacts
        .iter()
        .rev()
        .find(|artifact| {
            artifact.producer_id == source_writer.id
                && artifact
                    .path
                    .ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("candidate-gate metadata frontier has no Writer gate"))?;
    let gate_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &gate_ref.path,
    )?;
    let evidence = read_prior_writer_gate(&gate_path).map_err(|reason| anyhow::anyhow!(reason))?;
    if evidence.phase != CandidateGatePhase::Writer
        || evidence.disposition != CandidateGateDisposition::Rejected
        || evidence.candidate_commit != output.commit
    {
        bail!("candidate-gate evidence does not name the current rejected Writer candidate");
    }
    let failed = evidence
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .collect::<Vec<_>>();
    if evidence
        .checks
        .iter()
        .any(|check| check.status == CandidateCheckStatus::Error)
        || !matches!(
            failed.as_slice(),
            ["commit-metadata"] | ["commit-metadata", "project-check"]
        )
    {
        bail!(
            "candidate-gate metadata recovery requires commit-metadata and, at most, its derived project-check failure"
        );
    }
    let metadata_check = evidence
        .checks
        .iter()
        .find(|check| check.name == "commit-metadata")
        .ok_or_else(|| anyhow::anyhow!("candidate-gate has no commit-metadata check"))?;
    let project_check_ref = evidence
        .checks
        .iter()
        .find(|check| check.name == "project-check" && check.status == CandidateCheckStatus::Failed)
        .map(|check| {
            classify_derived_project_check(
                project_root,
                &source_writer.id,
                &gate_path,
                metadata_check,
                check,
            )
        })
        .transpose()?;
    let protected_base_commit = if project_check_ref.is_some() {
        validate_prior_writer_gate(&evidence, &output.commit).map_err(anyhow::Error::msg)?;
        evidence.base_commit.clone()
    } else {
        authorized_tree_identical_metadata_rewrite_base(&gate_path, &output.commit).ok_or_else(
            || {
                anyhow::anyhow!(
                    "candidate-gate does not authorize a tree-identical metadata rewrite"
                )
            },
        )?
    };
    if protected_base_commit != evidence.base_commit {
        bail!("candidate-gate protected base is inconsistent with its metadata authorization");
    }
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "candidate metadata recovery workspace",
    )?;
    let head = git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "resolve candidate metadata recovery HEAD",
    )?;
    if head != output.commit {
        bail!(
            "candidate HEAD changed from {} to {head}; ordinary candidate drift remains rejected",
            output.commit
        );
    }
    let status = git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
        "check candidate metadata recovery cleanliness",
    )?;
    if !status.is_empty() {
        bail!("candidate metadata recovery requires a clean candidate worktree:\n{status}");
    }
    if !git::run_raw(
        &workspace,
        &[
            "merge-base",
            "--is-ancestor",
            &protected_base_commit,
            &output.commit,
        ],
    )?
    .status
    .success()
    {
        bail!(
            "protected base {protected_base_commit} is not an ancestor of candidate {}",
            output.commit
        );
    }
    let candidate_tree = git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", output.commit)],
        "resolve candidate metadata recovery tree",
    )?;
    if writer_index + 1 < attempt.tasks.len()
        && attempt.tasks[writer_index + 1..].iter().any(|task| {
            task.status != TaskStatus::Complete
                && task.status != TaskStatus::Superseded
                && Some(task.id.as_str()) != ignored_task_id
        })
    {
        bail!("candidate-gate metadata frontier has unsettled Tasks after the current Writer");
    }
    let gate_path = gate_ref.path.clone();
    Ok(EligibleMetadataRepair {
        source_writer,
        source_candidate_commit: output.commit.clone(),
        protected_base_commit,
        candidate_tree,
        gate_ref: gate_ref.clone(),
        gate_path,
        project_check_ref,
        source_review_ref: None,
        source_finding_id: None,
    })
}

fn classify_derived_project_check(
    project_root: &Path,
    source_writer_id: &str,
    gate_path: &Path,
    metadata_check: &crate::candidate_gate::CandidateCheckEvidence,
    project_check: &crate::candidate_gate::CandidateCheckEvidence,
) -> Result<ArtifactRef> {
    let log_rel = crate::candidate_gate::validate_derived_project_check(
        project_root,
        gate_path,
        metadata_check,
        project_check,
    )?;

    Ok(ArtifactRef {
        producer_id: source_writer_id.to_string(),
        path: log_rel,
    })
}

pub(crate) fn authorized_derived_project_check_metadata_rewrite_base(
    project_root: &Path,
    gate_path: &Path,
    expected_candidate: &str,
) -> Option<String> {
    let evidence = read_prior_writer_gate(gate_path).ok()?;
    validate_prior_writer_gate(&evidence, expected_candidate).ok()?;
    let failed = evidence
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .collect::<Vec<_>>();
    if failed != ["commit-metadata", "project-check"] {
        return None;
    }
    let metadata_check = evidence
        .checks
        .iter()
        .find(|check| check.name == "commit-metadata")?;
    let project_check = evidence
        .checks
        .iter()
        .find(|check| check.name == "project-check")?;
    crate::candidate_gate::validate_derived_project_check(
        project_root,
        gate_path,
        metadata_check,
        project_check,
    )
    .ok()?;
    Some(evidence.base_commit)
}

fn read_authority(
    project_root: &Path,
    attempt: &Attempt,
    task: &Task,
    authority_ref: &ArtifactRef,
) -> Result<MetadataRepairAuthority> {
    let authority_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &authority_ref.path,
    )?;
    let authority: MetadataRepairAuthority =
        serde_json::from_slice(&fs::read(&authority_path).with_context(|| {
            format!(
                "read metadata recovery authority {}",
                authority_path.display()
            )
        })?)
        .with_context(|| {
            format!(
                "parse metadata recovery authority {}",
                authority_path.display()
            )
        })?;
    if authority.schema_version != SCHEMA_VERSION
        || authority.work_item_id != attempt.work_item_id
        || authority.attempt_id != attempt.id
        || authority.source_writer_task_id.is_empty()
        || authority.source_candidate_commit.is_empty()
        || authority.protected_base_commit.is_empty()
        || authority.candidate_tree.is_empty()
        || authority.source_gate_artifact.is_empty()
        || authority.source_gate_sha256.is_empty()
        || authority.source_project_check_artifact.is_some()
            != authority.source_project_check_sha256.is_some()
        || authority.source_review_artifact.is_some() != authority.source_review_sha256.is_some()
        || authority.source_review_artifact.is_some() != authority.source_finding_id.is_some()
        || authority.resolution.trim().is_empty()
        || authority.resolution_sha256 != content_digest(authority.resolution.as_bytes())
        || authority.requested_at.trim().is_empty()
        || authority_ref.path
            != work_artifact_path(
                &attempt.work_item_id,
                &attempt.id,
                &format!("candidate-metadata-repairs/{}/repair.json", task.id),
            )
    {
        bail!(
            "metadata recovery authority for {:?} is not valid for this Attempt",
            task.id
        );
    }
    Ok(authority)
}

fn validate_planned_repair(
    project_root: &Path,
    attempt: &Attempt,
    task: &Task,
    authority: &MetadataRepairAuthority,
) -> Result<()> {
    let eligible = if authority.source_review_artifact.is_some() {
        inspect_documentation_review_frontier(project_root, attempt, Some(task.id.as_str()))?
    } else {
        inspect_candidate_frontier(project_root, attempt, Some(task.id.as_str()))?
    };
    if authority.source_writer_task_id != eligible.source_writer.id
        || authority.source_candidate_commit != eligible.source_candidate_commit
        || authority.protected_base_commit != eligible.protected_base_commit
        || authority.candidate_tree != eligible.candidate_tree
        || authority.source_gate_artifact != eligible.gate_path
        || authority.source_review_artifact.as_deref()
            != eligible
                .source_review_ref
                .as_ref()
                .map(|artifact| artifact.path.as_str())
        || authority.source_finding_id.as_deref() != eligible.source_finding_id.as_deref()
        || !task_binds_frontier_evidence(task, &eligible)
        || !task.input_artifacts.contains(&eligible.gate_ref)
        || candidate_metadata_derived_project_check_path(task)
            != eligible
                .project_check_ref
                .as_ref()
                .map(|artifact| artifact.path.as_str())
        || eligible
            .project_check_ref
            .as_ref()
            .is_some_and(|artifact| !task.input_artifacts.contains(artifact))
    {
        bail!(
            "metadata recovery Writer {:?} no longer matches its authorized candidate-gate frontier",
            task.id
        );
    }
    let current_gate_sha256 = digest_path(project_root, &eligible.gate_path)?;
    if authority.source_gate_sha256 != current_gate_sha256 {
        bail!(
            "metadata recovery Writer {:?} source gate changed after authorization",
            task.id
        );
    }
    let current_project_check_sha256 = eligible
        .project_check_ref
        .as_ref()
        .map(|artifact| digest_path(project_root, &artifact.path))
        .transpose()?;
    let project_check_matches = authority.source_project_check_artifact.as_deref()
        == eligible
            .project_check_ref
            .as_ref()
            .map(|artifact| artifact.path.as_str())
        && authority.source_project_check_sha256.as_deref()
            == current_project_check_sha256.as_deref();
    if !project_check_matches {
        bail!(
            "metadata recovery Writer {:?} no longer matches its authorized project-check evidence",
            task.id
        );
    }
    let current_review_digest = eligible
        .source_review_ref
        .as_ref()
        .map(|artifact| digest_path(project_root, &artifact.path))
        .transpose()?;
    let review_matches = authority.source_review_artifact.as_deref()
        == eligible
            .source_review_ref
            .as_ref()
            .map(|artifact| artifact.path.as_str())
        && authority.source_review_sha256.as_deref() == current_review_digest.as_deref();
    if !review_matches {
        bail!(
            "metadata recovery Writer {:?} no longer matches its Documentation review evidence",
            task.id
        );
    }
    if authority.resolution_sha256 != content_digest(authority.resolution.as_bytes()) {
        bail!(
            "metadata recovery Writer {:?} has a mismatched authorization digest",
            task.id
        );
    }
    Ok(())
}

fn task_binds_frontier_evidence(task: &Task, eligible: &EligibleMetadataRepair) -> bool {
    task.input_artifacts.contains(&eligible.gate_ref)
        && eligible
            .source_review_ref
            .as_ref()
            .is_none_or(|review_ref| task.input_artifacts.contains(review_ref))
        && eligible
            .project_check_ref
            .as_ref()
            .is_none_or(|project_check_ref| task.input_artifacts.contains(project_check_ref))
}

/// Return the host-validated context needed by the candidate gate for a
/// review-derived metadata rewrite. Candidate-gate recovery already carries a
/// rejected gate artifact and follows its older path; review-derived recovery
/// carries the review and this authority instead, so the gate must verify the
/// authority's source evidence before accepting a rewritten candidate.
pub(crate) fn candidate_gate_authorization_context(
    project_root: &Path,
    attempt: &Attempt,
    task: &Task,
) -> Result<Option<crate::candidate_gate::AuthorizedMetadataRewriteContext>> {
    let Some(authority_ref) = task.input_artifacts.iter().find(|artifact| {
        artifact.producer_id == PRODUCER && artifact.path.ends_with("/repair.json")
    }) else {
        return Ok(None);
    };
    let authority = read_authority(project_root, attempt, task, authority_ref)?;
    let Some(review_path) = authority.source_review_artifact.as_deref() else {
        return Ok(None);
    };
    let review_digest = digest_path(project_root, review_path)?;
    if authority.source_review_sha256.as_deref() != Some(review_digest.as_str())
        || !task
            .input_artifacts
            .iter()
            .any(|artifact| artifact.producer_id != PRODUCER && artifact.path == review_path)
    {
        bail!(
            "metadata recovery Writer {:?} no longer carries its authorized Documentation review",
            task.id
        );
    }
    let gate_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &authority.source_gate_artifact,
    )?;
    if !task.input_artifacts.iter().any(|artifact| {
        artifact.producer_id == authority.source_writer_task_id
            && artifact.path == authority.source_gate_artifact
    }) {
        bail!(
            "metadata recovery Writer {:?} no longer carries its authorized source Writer gate",
            task.id
        );
    }
    let gate_digest = digest_path(project_root, &authority.source_gate_artifact)?;
    if authority.source_gate_sha256 != gate_digest {
        bail!(
            "metadata recovery Writer {:?} source Writer gate changed after authorization",
            task.id
        );
    }
    let gate = read_prior_writer_gate(&gate_path).map_err(anyhow::Error::msg)?;
    if gate.schema_version == 0
        || gate.schema_version > crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION
        || gate.phase != CandidateGatePhase::Writer
        || gate.disposition != CandidateGateDisposition::Passed
        || gate.candidate_commit != authority.source_candidate_commit
        || gate.checks.iter().any(|check| {
            matches!(
                check.status,
                CandidateCheckStatus::Failed | CandidateCheckStatus::Error
            )
        })
    {
        bail!(
            "metadata recovery Writer {:?} source Writer gate is no longer a passing gate for its candidate",
            task.id
        );
    }
    Ok(Some(
        crate::candidate_gate::AuthorizedMetadataRewriteContext {
            source_candidate_commit: authority.source_candidate_commit,
            protected_base_commit: authority.protected_base_commit,
            candidate_tree: authority.candidate_tree,
        },
    ))
}

/// Resolve a metadata recovery authority in a Writer correction context. This
/// lets the Writer receive the same explicit rewrite instructions that the
/// candidate gate will later enforce, even when no rejected candidate-gate
/// artifact is part of the correction input.
pub(crate) fn authorized_metadata_rewrite_base(
    project_root: &Path,
    authority_path: &Path,
    expected_candidate: &str,
) -> Option<String> {
    let authority: MetadataRepairAuthority =
        serde_json::from_slice(&fs::read(authority_path).ok()?).ok()?;
    if authority.schema_version != SCHEMA_VERSION
        || authority.source_candidate_commit != expected_candidate
        || authority.protected_base_commit.is_empty()
        || authority.candidate_tree.is_empty()
        || authority.source_gate_artifact.is_empty()
        || authority.source_gate_sha256.is_empty()
        || authority.source_review_artifact.is_some() != authority.source_review_sha256.is_some()
        || authority.source_review_artifact.is_some() != authority.source_finding_id.is_some()
    {
        return None;
    }
    let gate_digest = digest_path(project_root, &authority.source_gate_artifact).ok()?;
    (gate_digest == authority.source_gate_sha256).then_some(authority.protected_base_commit)
}

fn digest_path(project_root: &Path, relative: &str) -> Result<String> {
    let path =
        crate::work_task_executor::resolve_managed_artifact_area_path(project_root, relative)?;
    Ok(content_digest(&fs::read(&path).with_context(|| {
        format!(
            "read candidate gate for metadata recovery digest {}",
            path.display()
        )
    })?))
}

fn normalize_resolution(resolution: &str) -> Result<String> {
    let normalized = resolution.trim();
    if normalized.is_empty() {
        bail!("provide a non-empty --resolution for candidate metadata recovery");
    }
    if normalized.len() > MAX_RESOLUTION_BYTES {
        bail!(
            "the metadata recovery resolution is {} bytes; limit is {MAX_RESOLUTION_BYTES}",
            normalized.len()
        );
    }
    Ok(normalized.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate_gate::{CandidateCheckEvidence, CandidateGateEvidence};
    use crate::work_model::{PauseKind, TaskOutput, WorkItem, WorkModelError};
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        project: std::path::PathBuf,
        workspace: std::path::PathBuf,
        store: WorkModelStore,
        first_candidate: String,
        candidate: String,
        base: String,
        gate: String,
    }

    fn git_ok(path: &Path, args: &[&str]) {
        git::run(path, args, "prepare candidate metadata recovery fixture").unwrap();
    }

    fn fixture() -> Fixture {
        fixture_with_first_candidate_body(false)
    }

    fn fixture_with_malformed_first_candidate() -> Fixture {
        fixture_with_first_candidate_body(true)
    }

    fn fixture_with_first_candidate_body(malformed_first_candidate: bool) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("main");
        let workspace = tmp.path().join("work-candidate-metadata");
        fs::create_dir_all(&project).unwrap();
        git_ok(&project, &["init", "-q"]);
        git_ok(&project, &["config", "user.name", "Test Author"]);
        git_ok(&project, &["config", "user.email", "test@example.com"]);
        fs::write(project.join("README.md"), "base\n").unwrap();
        git_ok(&project, &["add", "."]);
        git_ok(&project, &["commit", "-q", "-m", "Create base"]);
        let base = git::run_stdout(&project, &["rev-parse", "HEAD"], "fixture base").unwrap();
        git_ok(
            &project,
            &[
                "worktree",
                "add",
                "-b",
                "work/candidate-metadata",
                workspace.to_str().unwrap(),
            ],
        );
        fs::write(workspace.join("candidate.txt"), "candidate\n").unwrap();
        git_ok(&workspace, &["add", "."]);
        let first_message = if malformed_first_candidate {
            "Candidate\n\nThis malformed body contains literal \\n text and is deliberately longer than seventy-two characters."
        } else {
            "Candidate"
        };
        git_ok(&workspace, &["commit", "-q", "-m", first_message]);
        let candidate =
            git::run_stdout(&workspace, &["rev-parse", "HEAD"], "fixture candidate").unwrap();
        fs::write(workspace.join("follow-up.txt"), "follow-up\n").unwrap();
        git_ok(&workspace, &["add", "."]);
        git_ok(
            &workspace,
            &[
                "commit",
                "-q",
                "-m",
                "Candidate follow-up\n\nThis malformed body contains literal \\n text and is deliberately longer than seventy-two characters.\nThis ordinary overlong body line has no metadata marker and exceeds the project limit.",
            ],
        );
        let latest_candidate = git::run_stdout(
            &workspace,
            &["rev-parse", "HEAD"],
            "fixture latest candidate",
        )
        .unwrap();

        let store = WorkModelStore::new(&project);
        let mut item = WorkItem::planned("work-1", "Repair metadata");
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: "../work-candidate-metadata".to_string(),
            source_branch: "work/candidate-metadata".to_string(),
            base_commit: Some(base.clone()),
            commit: latest_candidate.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        let gate = work_artifact_path(
            "work-1",
            "attempt-1",
            "candidate-gates/attempt-1-write-1/candidate-gate.json",
        );
        let gate_path = project.join(&gate);
        fs::create_dir_all(gate_path.parent().unwrap()).unwrap();
        let checks = [
            ("candidate-identity", CandidateCheckStatus::Passed),
            ("whitespace", CandidateCheckStatus::Passed),
            ("commit-metadata", CandidateCheckStatus::Failed),
            ("behavior-references", CandidateCheckStatus::Passed),
            ("tester-reference-evidence", CandidateCheckStatus::Skipped),
            ("project-check", CandidateCheckStatus::Skipped),
        ]
        .into_iter()
        .map(|(name, status)| CandidateCheckEvidence {
            name: name.to_string(),
            status,
            diagnostics: Vec::new(),
            log_path: None,
        })
        .collect();
        fs::write(
            &gate_path,
            serde_json::to_vec_pretty(&CandidateGateEvidence {
                schema_version: crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION,
                phase: CandidateGatePhase::Writer,
                disposition: CandidateGateDisposition::Rejected,
                base_commit: base.clone(),
                candidate_commit: latest_candidate.clone(),
                tester_results_path: None,
                metadata_rewrite: None,
                checks,
            })
            .unwrap(),
        )
        .unwrap();
        let attempt = &mut item.attempts[0];
        attempt.artifacts.push(ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: gate.clone(),
        });
        attempt.status = AttemptStatus::NeedsUser;
        attempt.pause_kind = Some(PauseKind::CandidateGate);
        attempt.review_state = Some(AttemptReviewState::NotReviewed);
        store.create_work_item(&item).unwrap();
        Fixture {
            _tmp: tmp,
            project,
            workspace,
            store,
            first_candidate: candidate,
            candidate: latest_candidate,
            base,
            gate,
        }
    }

    impl Fixture {
        fn configure_project_check(
            &self,
            metadata_diagnostics: &[&str],
            hook_log: &str,
            project_diagnostics: &[&str],
        ) {
            let gate_path = self.project.join(&self.gate);
            let mut evidence: CandidateGateEvidence =
                serde_json::from_slice(&fs::read(&gate_path).unwrap()).unwrap();
            let metadata = evidence
                .checks
                .iter_mut()
                .find(|check| check.name == "commit-metadata")
                .unwrap();
            metadata.diagnostics = metadata_diagnostics
                .iter()
                .map(|diagnostic| (*diagnostic).to_string())
                .collect();
            let hook_path = gate_path
                .parent()
                .unwrap()
                .join("hooks/check-pre-merge.log");
            fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
            fs::write(&hook_path, hook_log).unwrap();
            let project = evidence
                .checks
                .iter_mut()
                .find(|check| check.name == "project-check")
                .unwrap();
            project.status = CandidateCheckStatus::Failed;
            project.diagnostics = std::iter::once("check-pre-merge exited 1".to_string())
                .chain(
                    project_diagnostics
                        .iter()
                        .map(|diagnostic| (*diagnostic).to_string()),
                )
                .collect();
            project.log_path = Some(
                hook_path
                    .strip_prefix(&self.project)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
            evidence.disposition = CandidateGateDisposition::Rejected;
            fs::write(&gate_path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
        }

        fn update_project_check(&self, update: impl FnOnce(&mut CandidateCheckEvidence)) {
            let gate_path = self.project.join(&self.gate);
            let mut evidence: CandidateGateEvidence =
                serde_json::from_slice(&fs::read(&gate_path).unwrap()).unwrap();
            let project = evidence
                .checks
                .iter_mut()
                .find(|check| check.name == "project-check")
                .unwrap();
            update(project);
            fs::write(&gate_path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
        }
    }

    fn metadata_diagnostic(candidate: &str) -> String {
        format!("commit {candidate} contains literal \\n text instead of line breaks")
    }

    fn candidate_head(fixture: &Fixture) -> String {
        git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "snapshot candidate HEAD",
        )
        .unwrap()
    }

    fn snapshot_artifacts(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
        fn visit(root: &Path, current: &Path, entries: &mut Vec<(String, Option<Vec<u8>>)>) {
            let mut children = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                let relative = child
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let metadata = fs::symlink_metadata(&child).unwrap();
                if metadata.is_dir() {
                    entries.push((relative, None));
                    visit(root, &child, entries);
                } else {
                    entries.push((relative, Some(fs::read(&child).unwrap())));
                }
            }
        }

        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn metadata_recovery_accepts_hook_line_length_wording() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let repair = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();

        let authority: MetadataRepairAuthority = serde_json::from_slice(
            &fs::read(fixture.project.join(&repair.audit_artifact)).unwrap(),
        )
        .unwrap();
        let project_check_path = Path::new(&fixture.gate)
            .parent()
            .unwrap()
            .join("hooks/check-pre-merge.log")
            .to_string_lossy()
            .to_string();
        let item = fixture.store.read_work_item("work-1").unwrap();
        let writer = item.attempts[0].tasks.last().unwrap();

        assert_eq!(authority.source_candidate_commit, fixture.candidate);
        assert_eq!(authority.protected_base_commit, fixture.base);
        assert_eq!(authority.source_gate_artifact, fixture.gate);
        assert_eq!(
            authority.source_project_check_artifact.as_deref(),
            Some(project_check_path.as_str())
        );
        assert_eq!(
            authority.source_project_check_sha256.as_deref(),
            Some(
                digest_path(&fixture.project, &project_check_path)
                    .unwrap()
                    .as_str()
            )
        );
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: fixture.gate.clone(),
        }));
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: project_check_path,
        }));
    }

    #[test]
    fn metadata_recovery_accepts_hook_line_length_with_duplicate_gate_finding() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let invalid_author = format!(
            "commit {} has invalid author name: missing",
            fixture.candidate
        );
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata, &invalid_author], &hook, &[]);

        let repair = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();

        assert!(!repair.already_authorized);
        assert_eq!(repair.candidate_commit, fixture.candidate);
    }

    #[test]
    fn metadata_recovery_rejects_hook_wording_only_in_project_check_diagnostic() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], "", &[&hook]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_noncanonical_project_check_log_path() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let gate_path = fixture.project.join(&fixture.gate);
        let mut evidence: CandidateGateEvidence =
            serde_json::from_slice(&fs::read(&gate_path).unwrap()).unwrap();
        let project = evidence
            .checks
            .iter_mut()
            .find(|check| check.name == "project-check")
            .unwrap();
        let canonical_log = gate_path
            .parent()
            .unwrap()
            .join("hooks/check-pre-merge.log");
        let alternate_log = canonical_log.parent().unwrap().join("other.log");
        fs::write(&alternate_log, hook).unwrap();
        project.log_path = Some(
            alternate_log
                .strip_prefix(&fixture.project)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
        fs::write(&gate_path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_legacy_project_policy_wording() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} has no body separated from its subject by a blank line\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_accepts_real_hook_diagnostics_for_multiple_commits() {
        let fixture = fixture_with_malformed_first_candidate();
        let first_metadata = metadata_diagnostic(&fixture.first_candidate);
        let latest_metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\ncommit {} body line 3 exceeds 72 characters\n",
            fixture.first_candidate, fixture.candidate
        );
        fixture.configure_project_check(&[&first_metadata, &latest_metadata], &hook, &[]);

        let repair = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();

        assert!(!repair.already_authorized);
    }

    #[test]
    fn metadata_recovery_accepts_duplicate_metadata_findings_for_one_commit() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let invalid_author = format!(
            "commit {} has invalid author email: malformed@example",
            fixture.candidate
        );
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata, &invalid_author], &hook, &[]);

        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();
    }

    #[test]
    fn metadata_recovery_rejects_same_commit_diagnostic_with_different_reason() {
        let fixture = fixture();
        let invalid_author = format!(
            "commit {} has invalid author email: malformed@example",
            fixture.candidate
        );
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&invalid_author], &hook, &[]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_policy_only_hook_diagnostics() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        assert!(
            repair(
                &fixture.project,
                &fixture.store,
                "work-1",
                "attempt-1",
                "Rewrite only malformed commit messages after the protected base.",
            )
            .is_err()
        );
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
    }

    #[test]
    fn metadata_recovery_rejects_matching_diagnostic_only_in_project_check() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let project_diagnostic = format!(
            "commit {} body line 3 exceeds 72 characters",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], "", &[&project_diagnostic]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        assert!(
            repair(
                &fixture.project,
                &fixture.store,
                "work-1",
                "attempt-1",
                "Rewrite only malformed commit messages after the protected base.",
            )
            .is_err()
        );
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
    }

    #[test]
    fn metadata_recovery_rejects_unrelated_hook_output() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\nunrelated hook output\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        assert!(
            repair(
                &fixture.project,
                &fixture.store,
                "work-1",
                "attempt-1",
                "Rewrite only malformed commit messages after the protected base.",
            )
            .is_err()
        );
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
    }

    #[test]
    fn metadata_recovery_rejects_parseable_hook_diagnostic_for_uncovered_commit() {
        let fixture = fixture_with_malformed_first_candidate();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\ncommit {} body line 3 exceeds 72 characters\n",
            fixture.candidate, fixture.first_candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_generic_overlong_line_without_literal_breaks() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 4 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_noncanonical_hook_log_path() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let canonical_path = fixture.project.join(
            Path::new(&fixture.gate)
                .parent()
                .unwrap()
                .join("hooks/check-pre-merge.log"),
        );
        let alternate_path = canonical_path.parent().unwrap().join("other.log");
        fs::copy(&canonical_path, &alternate_path).unwrap();
        let alternate_rel = alternate_path
            .strip_prefix(&fixture.project)
            .unwrap()
            .to_string_lossy()
            .to_string();
        fixture.update_project_check(|project| project.log_path = Some(alternate_rel));
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        assert!(
            repair(
                &fixture.project,
                &fixture.store,
                "work-1",
                "attempt-1",
                "Rewrite only malformed commit messages after the protected base.",
            )
            .is_err()
        );
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_recovery_rejects_hook_log_symlink_outside_project() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let canonical_path = fixture.project.join(
            Path::new(&fixture.gate)
                .parent()
                .unwrap()
                .join("hooks/check-pre-merge.log"),
        );
        let outside_path = fixture.project.parent().unwrap().join("outside-hook.log");
        fs::write(&outside_path, &hook).unwrap();
        fs::remove_file(&canonical_path).unwrap();
        std::os::unix::fs::symlink(&outside_path, &canonical_path).unwrap();
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        assert!(
            repair(
                &fixture.project,
                &fixture.store,
                "work-1",
                "attempt-1",
                "Rewrite only malformed commit messages after the protected base.",
            )
            .is_err()
        );
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
    }

    #[test]
    fn metadata_recovery_rejects_inconsistent_project_check_exit_evidence() {
        for diagnostics in [
            vec!["check-pre-merge exited 0"],
            vec!["check-pre-merge exited 1", "check-pre-merge exited 2"],
        ] {
            let fixture = fixture();
            let metadata = metadata_diagnostic(&fixture.candidate);
            let hook = format!(
                "commit {} body line 3 exceeds 72 characters\n",
                fixture.candidate
            );
            fixture.configure_project_check(&[&metadata], &hook, &[]);
            fixture.update_project_check(|project| {
                project.diagnostics = diagnostics
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect();
            });
            let before_head = candidate_head(&fixture);

            assert!(
                repair(
                    &fixture.project,
                    &fixture.store,
                    "work-1",
                    "attempt-1",
                    "Rewrite only malformed commit messages after the protected base.",
                )
                .is_err()
            );
            assert_eq!(candidate_head(&fixture), before_head);
        }
    }

    #[test]
    fn metadata_recovery_task_requires_derived_hook_artifact() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();

        let mut item = fixture.store.read_work_item("work-1").unwrap();
        let task = item.attempts[0].tasks.last_mut().unwrap();
        task.input_artifacts
            .retain(|artifact| !artifact.path.ends_with("/hooks/check-pre-merge.log"));
        assert!(matches!(
            item.attempts[0].validate("work-1"),
            Err(WorkModelError::InvalidCandidateMetadataRepairTask { .. })
        ));
        assert!(is_resume_ready(&fixture.project, &item, "attempt-1").is_err());
    }

    #[test]
    fn derived_project_check_authorizes_followup_candidate_gate() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        git_ok(
            &fixture.workspace,
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                "Candidate\n\n- Preserve the candidate tree",
            ],
        );
        let replacement = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "resolve rewritten fixture candidate",
        )
        .unwrap();
        let prior_gate_path = fixture.project.join(&fixture.gate);
        let artifact_path = fixture.project.join(work_artifact_path(
            "work-1",
            "attempt-1",
            "candidate-gates/attempt-1-write-2/candidate-gate.json",
        ));
        let evidence = crate::candidate_gate::run_candidate_gate_with_prior(
            crate::candidate_gate::CandidateGateRequest {
                project_root: &fixture.project,
                workspace: &fixture.workspace,
                work_item_id: "work-1",
                attempt_id: "attempt-1",
                task_id: Some("attempt-1-write-2"),
                phase: CandidateGatePhase::Writer,
                base_commit: &fixture.candidate,
                candidate_commit: &replacement,
                completion_contract: None,
                allow_tree_identical_metadata_rewrite: false,
                metadata_rewrite_authorization: None,
                tester_results_path: None,
                accepted_tester_commands: &[],
                artifact_path: &artifact_path,
            },
            Some(&prior_gate_path),
        )
        .unwrap();

        assert_eq!(
            evidence.disposition,
            CandidateGateDisposition::Passed,
            "follow-up candidate gate should admit the derived project-check repair"
        );
        let rewrite = evidence.metadata_rewrite.unwrap();
        assert_eq!(rewrite.original_base_commit, fixture.base);
        assert_eq!(rewrite.rejected_candidate_commit, fixture.candidate);
        assert_eq!(rewrite.replacement_candidate_commit, replacement);
    }

    #[test]
    fn metadata_recovery_rejects_unrelated_project_check() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(
            &[&metadata],
            &hook,
            &["dependency cycle: app -> render -> app"],
        );
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = candidate_head(&fixture);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(candidate_head(&fixture), before_head);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_uncovered_hook_line_length() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\n",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "resolve fixture candidate HEAD before rejection",
        )
        .unwrap();

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(
            git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "resolve fixture candidate HEAD after rejection",
            )
            .unwrap(),
            before_head
        );
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_hook_line_length_for_non_literal_gate_finding() {
        let fixture = fixture();
        let metadata = format!(
            "commit {} has invalid author name: missing",
            fixture.candidate
        );
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "resolve fixture candidate HEAD before non-literal rejection",
        )
        .unwrap();

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(
            git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "resolve fixture candidate HEAD after non-literal rejection",
            )
            .unwrap(),
            before_head
        );
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_hook_line_length_with_wrong_threshold() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 73 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);
        let before_head = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "resolve fixture candidate HEAD before threshold rejection",
        )
        .unwrap();

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert_eq!(
            git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "resolve fixture candidate HEAD after threshold rejection",
            )
            .unwrap(),
            before_head
        );
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_unrelated_hook_diagnostic() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\ndependency cycle: app -> render -> app\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn metadata_recovery_rejects_non_positive_hook_line() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 0 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);

        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_item = fixture.store.read_work_item("work-1").unwrap();
        let before_artifacts = snapshot_artifacts(&artifact_root);

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap_err();

        assert!(error.to_string().contains("project-check"));
        assert_eq!(fixture.store.read_work_item("work-1").unwrap(), before_item);
        assert_eq!(snapshot_artifacts(&artifact_root), before_artifacts);
        assert!(!artifact_root.join("candidate-metadata-repairs").exists());
    }

    #[test]
    fn request_records_authority_and_is_idempotent() {
        let fixture = fixture();
        let first = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();
        assert!(!first.already_authorized);
        assert_eq!(first.candidate_commit, fixture.candidate);
        assert_eq!(first.protected_base_commit, fixture.base);
        let authority: MetadataRepairAuthority =
            serde_json::from_slice(&fs::read(fixture.project.join(&first.audit_artifact)).unwrap())
                .unwrap();
        assert_eq!(authority.source_gate_artifact, fixture.gate);
        assert_eq!(authority.source_candidate_commit, fixture.candidate);
        assert_eq!(authority.protected_base_commit, fixture.base);
        assert_eq!(
            authority.resolution_sha256,
            content_digest(authority.resolution.as_bytes())
        );

        let item = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &item.attempts[0];
        assert_eq!(attempt.status, AttemptStatus::Planned);
        assert!(attempt.pause_kind.is_none());
        assert!(is_resume_ready(&fixture.project, &item, "attempt-1").unwrap());
        let writer = attempt.tasks.last().unwrap();
        assert_eq!(writer.role, CANDIDATE_METADATA_REPAIR_ROLE);
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: fixture.gate.clone(),
        }));
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: PRODUCER.to_string(),
            path: first.audit_artifact.clone(),
        }));
        assert!(attempt.artifacts.contains(&ArtifactRef {
            producer_id: PRODUCER.to_string(),
            path: first.audit_artifact.clone(),
        }));

        let before_second_item = fixture.store.read_work_item("work-1").unwrap();
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_second_artifacts = snapshot_artifacts(&artifact_root);
        let before_second_head = candidate_head(&fixture);
        let second = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();
        assert!(second.already_authorized);
        assert_eq!(second.task_id, first.task_id);
        assert_eq!(second.audit_artifact, first.audit_artifact);
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .iter()
                .filter(|task| task.role == CANDIDATE_METADATA_REPAIR_ROLE)
                .count(),
            1
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap(),
            before_second_item
        );
        assert_eq!(snapshot_artifacts(&artifact_root), before_second_artifacts);
        assert_eq!(candidate_head(&fixture), before_second_head);

        let queued = crate::queue::resume(&fixture.project, "work-1", "attempt-1", None).unwrap();
        assert_eq!(queued, crate::queue::ResumeDispatchOutcome::Created);
        let queued_again =
            crate::queue::resume(&fixture.project, "work-1", "attempt-1", None).unwrap();
        assert_eq!(
            queued_again,
            crate::queue::ResumeDispatchOutcome::ExistingMatching
        );
    }

    #[test]
    fn metadata_recovery_derived_check_is_idempotent() {
        metadata_recovery_hook_wording_is_idempotent();
    }

    #[test]
    fn metadata_recovery_hook_wording_is_idempotent() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        let first = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();
        assert!(!first.already_authorized);
        assert_eq!(first.candidate_commit, fixture.candidate);
        assert_eq!(first.protected_base_commit, fixture.base);
        let authority: MetadataRepairAuthority =
            serde_json::from_slice(&fs::read(fixture.project.join(&first.audit_artifact)).unwrap())
                .unwrap();
        assert_eq!(authority.source_gate_artifact, fixture.gate);
        assert_eq!(authority.source_candidate_commit, fixture.candidate);
        assert_eq!(authority.protected_base_commit, fixture.base);
        assert_eq!(
            authority.source_project_check_artifact.as_deref(),
            Some(
                ".fluent/work/artifacts/work-1/attempt-1/candidate-gates/attempt-1-write-1/hooks/check-pre-merge.log"
            )
        );
        let project_check_sha256 = digest_path(
            &fixture.project,
            authority.source_project_check_artifact.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            authority.source_project_check_sha256.as_deref(),
            Some(project_check_sha256.as_str())
        );
        assert_eq!(
            authority.resolution_sha256,
            content_digest(authority.resolution.as_bytes())
        );
        let candidate_tree = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", &format!("{}^{{tree}}", fixture.candidate)],
            "resolve fixture candidate tree",
        )
        .unwrap();
        assert_eq!(authority.candidate_tree, candidate_tree);

        let item = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &item.attempts[0];
        assert_eq!(attempt.status, AttemptStatus::Planned);
        assert!(attempt.pause_kind.is_none());
        assert!(is_resume_ready(&fixture.project, &item, "attempt-1").unwrap());
        let writer = attempt.tasks.last().unwrap();
        assert_eq!(writer.role, CANDIDATE_METADATA_REPAIR_ROLE);
        assert!(
            writer
                .input_artifacts
                .iter()
                .any(|artifact| artifact.producer_id == PRODUCER)
        );

        let before_second_item = fixture.store.read_work_item("work-1").unwrap();
        let artifact_root = fixture
            .project
            .join(".fluent/work/artifacts/work-1/attempt-1");
        let before_second_artifacts = snapshot_artifacts(&artifact_root);
        let before_second_head = git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "resolve fixture candidate HEAD",
        )
        .unwrap();
        let second = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Rewrite only malformed commit messages after the protected base.",
        )
        .unwrap();
        assert!(second.already_authorized);
        assert_eq!(second.task_id, first.task_id);
        assert_eq!(second.audit_artifact, first.audit_artifact);
        assert_eq!(second.candidate_commit, first.candidate_commit);
        assert_eq!(second.protected_base_commit, first.protected_base_commit);
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .iter()
                .filter(|task| task.role == CANDIDATE_METADATA_REPAIR_ROLE)
                .count(),
            1
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap(),
            before_second_item
        );
        let replayed_writer = fixture.store.read_work_item("work-1").unwrap().attempts[0]
            .tasks
            .last()
            .unwrap()
            .clone();
        assert!(replayed_writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: fixture.gate.clone(),
        }));
        assert!(replayed_writer.input_artifacts.iter().any(|artifact| {
            artifact.producer_id == "attempt-1-write-1"
                && artifact.path.ends_with("/hooks/check-pre-merge.log")
        }));
        assert_eq!(snapshot_artifacts(&artifact_root), before_second_artifacts);
        assert_eq!(
            git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "resolve fixture candidate HEAD after replay",
            )
            .unwrap(),
            before_second_head
        );

        let queued = crate::queue::resume(&fixture.project, "work-1", "attempt-1", None).unwrap();
        assert_eq!(queued, crate::queue::ResumeDispatchOutcome::Created);
        let queued_again =
            crate::queue::resume(&fixture.project, "work-1", "attempt-1", None).unwrap();
        assert_eq!(
            queued_again,
            crate::queue::ResumeDispatchOutcome::ExistingMatching
        );
    }

    #[test]
    fn authorized_recovery_rejects_missing_project_check_task_input() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Authorize the metadata rewrite.",
        )
        .unwrap();

        let mut item = fixture.store.read_work_item("work-1").unwrap();
        let writer = item.attempts[0].tasks.last_mut().unwrap();
        writer
            .input_artifacts
            .retain(|artifact| !artifact.path.ends_with("/hooks/check-pre-merge.log"));
        let storage_error = fixture.store.write_work_item(&item).unwrap_err();
        assert!(format!("{storage_error:?}").contains("InvalidCandidateMetadataRepairTask"));

        let error = is_resume_ready(&fixture.project, &item, "attempt-1").unwrap_err();
        assert!(error.to_string().contains("candidate-gate frontier"));
    }

    #[test]
    fn ordinary_candidate_drift_is_rejected() {
        let fixture = fixture();
        fs::write(fixture.workspace.join("drift.txt"), "untracked\n").unwrap();
        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Authorize the metadata rewrite.",
        )
        .unwrap_err();
        assert!(error.to_string().contains("clean candidate worktree"));
    }

    #[test]
    fn authorized_recovery_rejects_drift_before_resume() {
        let fixture = fixture();
        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Authorize the metadata rewrite.",
        )
        .unwrap();
        fs::write(fixture.workspace.join("drift.txt"), "untracked\n").unwrap();
        let item = fixture.store.read_work_item("work-1").unwrap();
        let error = is_resume_ready(&fixture.project, &item, "attempt-1").unwrap_err();
        assert!(error.to_string().contains("clean candidate worktree"));
    }

    #[test]
    fn authorized_recovery_rejects_changed_project_check_before_resume() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 3 exceeds 72 characters\n",
            fixture.candidate
        );
        fixture.configure_project_check(&[&metadata], &hook, &[]);
        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Authorize the metadata rewrite.",
        )
        .unwrap();

        let hook_path = fixture.project.join(
            Path::new(&fixture.gate)
                .parent()
                .unwrap()
                .join("hooks/check-pre-merge.log"),
        );
        fs::write(&hook_path, format!("  {hook}")).unwrap();
        let item = fixture.store.read_work_item("work-1").unwrap();
        let error = is_resume_ready(&fixture.project, &item, "attempt-1").unwrap_err();
        assert!(error.to_string().contains("project-check evidence"));
    }

    #[test]
    fn recovery_action_names_the_audited_public_transition() {
        let fixture = fixture();
        let item = fixture.store.read_work_item("work-1").unwrap();
        let action = recovery_action(&fixture.project, &item.attempts[0]).unwrap();
        assert!(action.contains("repair-candidate-metadata work-1 attempt-1"));
        assert!(action.contains("--resolution"));
    }
}
