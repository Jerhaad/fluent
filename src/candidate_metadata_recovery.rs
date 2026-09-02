//! Audited recovery for a rejected, tree-identical candidate metadata gate.
//!
//! A candidate-gate rejection is normally resumed by planning an ordinary
//! corrective Writer.  When the only remaining failure is commit metadata,
//! however, the Writer must be allowed to rewrite commit messages without
//! changing the candidate tree or protected ancestry.  This module records the
//! operator's decision and the exact gate it applies to, then schedules that
//! Writer through the normal candidate gate, Tester, review, and Learner path.
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
    ArtifactRef, Attempt, AttemptReviewState, AttemptStatus, CANDIDATE_METADATA_REPAIR_ROLE, Task,
    TaskKind, TaskStatus, WorkModelError, WorkModelStore, now_iso8601,
    resolve_managed_sibling_workspace_path, work_artifact_path,
};

pub const PRODUCER: &str = "candidate-metadata-recovery";
const SCHEMA_VERSION: u32 = 1;
const MAX_RESOLUTION_BYTES: usize = 4_096;
const MAX_PROJECT_CHECK_LOG_BYTES: u64 = 256 * 1024;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataFinding {
    commit: String,
    reason: String,
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
    if write_rounds
        >= attempt.effective_write_round_cap(crate::work_attempt_loop::max_total_write_rounds())
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

            let mut source_evidence = vec![gate_ref.clone()];
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
            writer.instructions = Some(format!(
                "Perform only the audited tree-identical commit-metadata rewrite described by {}. Preserve the candidate tree and protected base; Fluent will verify the resulting candidate.",
                authority_rel
            ));
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
    let Some(task) = attempt
        .tasks
        .iter()
        .find(|task| task.kind == TaskKind::Write && task.role == CANDIDATE_METADATA_REPAIR_ROLE)
    else {
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
    if attempt.status != AttemptStatus::NeedsUser || !attempt.is_candidate_gate_pause() {
        bail!("Attempt is not paused at a candidate-gate metadata frontier");
    }
    // This also rejects a concurrent Writer, failed Task, or unsupported pause
    // shape before we create an audited recovery task.
    attempt.ensure_resume_ready()?;
    inspect_candidate_frontier(project_root, attempt, None)
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
            task.status != TaskStatus::Complete && Some(task.id.as_str()) != ignored_task_id
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
        gate_ref,
        gate_path,
        project_check_ref,
    })
}

pub(crate) fn validate_derived_project_check(
    project_root: &Path,
    gate_path: &Path,
    metadata_check: &crate::candidate_gate::CandidateCheckEvidence,
    project_check: &crate::candidate_gate::CandidateCheckEvidence,
) -> Result<()> {
    let gate_findings =
        metadata_findings(&metadata_check.diagnostics, &[], false).ok_or_else(|| {
            anyhow::anyhow!(
                "project-check failure is not explained by structured commit-metadata diagnostics"
            )
        })?;
    if gate_findings.is_empty() {
        bail!("project-check failure is not explained by structured commit-metadata diagnostics");
    }
    let known_commits = gate_findings
        .iter()
        .map(|finding| finding.commit.clone())
        .collect::<Vec<_>>();

    let log_rel = project_check
        .log_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("derived project-check failure has no stored hook log"))?;
    let log_path =
        crate::work_task_executor::resolve_managed_artifact_area_path(project_root, log_rel)?;
    let expected_log_dir = gate_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("candidate gate artifact path has no parent"))?
        .join("hooks");
    if log_path.parent() != Some(expected_log_dir.as_path()) {
        bail!(
            "derived project-check hook log is not beside the candidate gate: {}",
            log_path.display()
        );
    }
    let metadata = fs::metadata(&log_path).with_context(|| {
        format!(
            "inspect derived project-check hook log {}",
            log_path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "derived project-check hook log is not a regular file: {}",
            log_path.display()
        );
    }
    if metadata.len() > MAX_PROJECT_CHECK_LOG_BYTES {
        bail!(
            "derived project-check hook log is {} bytes; maximum is {MAX_PROJECT_CHECK_LOG_BYTES} bytes",
            metadata.len()
        );
    }
    let log =
        String::from_utf8(fs::read(&log_path).with_context(|| {
            format!("read derived project-check hook log {}", log_path.display())
        })?)
        .map_err(|_| anyhow::anyhow!("derived project-check hook log is not valid UTF-8"))?;
    let mut project_findings = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    project_findings.extend(
        project_check
            .diagnostics
            .iter()
            .filter(|diagnostic| !is_project_check_exit_diagnostic(diagnostic))
            .cloned(),
    );
    if project_findings.is_empty() {
        bail!("derived project-check failure has no structured hook diagnostics");
    }
    let project_findings =
        metadata_findings(&project_findings, &known_commits, true).ok_or_else(|| {
            anyhow::anyhow!(
                "project-check failure contains a diagnostic unrelated to commit-metadata findings"
            )
        })?;
    let mut remaining = gate_findings;
    for finding in project_findings {
        let Some(index) = remaining.iter().position(|candidate| candidate == &finding) else {
            bail!(
                "project-check failure contains a diagnostic unrelated to commit-metadata findings"
            );
        };
        remaining.remove(index);
    }
    Ok(())
}

fn classify_derived_project_check(
    project_root: &Path,
    source_writer_id: &str,
    gate_path: &Path,
    metadata_check: &crate::candidate_gate::CandidateCheckEvidence,
    project_check: &crate::candidate_gate::CandidateCheckEvidence,
) -> Result<ArtifactRef> {
    let log_rel = project_check
        .log_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("derived project-check failure has no stored hook log"))?;
    validate_derived_project_check(project_root, gate_path, metadata_check, project_check)?;

    Ok(ArtifactRef {
        producer_id: source_writer_id.to_string(),
        path: log_rel.to_string(),
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
    validate_derived_project_check(project_root, gate_path, metadata_check, project_check).ok()?;
    Some(evidence.base_commit)
}

fn metadata_findings(
    diagnostics: &[String],
    known_commits: &[String],
    allow_project_policy_wording: bool,
) -> Option<Vec<MetadataFinding>> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            let normalized = normalize_diagnostic_text(diagnostic);
            let rest = normalized.strip_prefix("commit ")?;
            let (raw_commit, reason) = rest.split_once(' ')?;
            let raw_commit = raw_commit.trim_end_matches(':');
            let commit = resolve_commit_id(raw_commit, known_commits)?;
            let reason = normalize_metadata_reason(reason, allow_project_policy_wording)?;
            Some(MetadataFinding { commit, reason })
        })
        .collect()
}

fn normalize_diagnostic_text(diagnostic: &str) -> String {
    diagnostic.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_commit_id(raw_commit: &str, known_commits: &[String]) -> Option<String> {
    if !(4..=64).contains(&raw_commit.len())
        || !raw_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let raw_commit = raw_commit.to_ascii_lowercase();
    if known_commits.is_empty() {
        return Some(raw_commit);
    }
    let matches = known_commits
        .iter()
        .filter(|commit| commit.as_str() == raw_commit || commit.starts_with(&raw_commit))
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn normalize_metadata_reason(reason: &str, allow_project_policy_wording: bool) -> Option<String> {
    let reason = reason.to_ascii_lowercase();
    if allow_project_policy_wording && is_body_line_length_reason(&reason) {
        return Some("literal-line-breaks".to_string());
    }
    match reason.as_str() {
        "is a merge commit" => Some("merge-commit".to_string()),
        "has an empty subject" => Some("empty-subject".to_string()),
        "contains literal \\n text instead of line breaks" => {
            Some("literal-line-breaks".to_string())
        }
        "has no body separated from its subject by a blank line"
            if allow_project_policy_wording =>
        {
            Some("literal-line-breaks".to_string())
        }
        _ => reason.strip_prefix("has invalid ").and_then(|label| {
            let label = label.split(':').next()?.trim();
            matches!(
                label,
                "author name" | "author email" | "committer name" | "committer email"
            )
            .then(|| format!("invalid-{}", label.replace(' ', "-")))
        }),
    }
}

fn is_body_line_length_reason(reason: &str) -> bool {
    let mut fields = reason.split_whitespace();
    if fields.next() != Some("body") || fields.next() != Some("line") {
        return false;
    }
    let Some(line) = fields.next() else {
        return false;
    };
    let Ok(line_number) = line.parse::<u64>() else {
        return false;
    };
    if line.is_empty() || !line.bytes().all(|byte| byte.is_ascii_digit()) || line_number == 0 {
        return false;
    }
    matches!(
        (fields.next(), fields.next(), fields.next(), fields.next()),
        (Some("exceeds"), Some("72"), Some("characters"), None)
    )
}

fn is_project_check_exit_diagnostic(diagnostic: &str) -> bool {
    let normalized = normalize_diagnostic_text(diagnostic);
    normalized
        .strip_prefix("check-pre-merge exited ")
        .and_then(|code| code.parse::<i32>().ok())
        .is_some()
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
    let eligible = inspect_candidate_frontier(project_root, attempt, Some(task.id.as_str()))?;
    if authority.source_writer_task_id != eligible.source_writer.id
        || authority.source_candidate_commit != eligible.source_candidate_commit
        || authority.protected_base_commit != eligible.protected_base_commit
        || authority.candidate_tree != eligible.candidate_tree
        || authority.source_gate_artifact != eligible.gate_path
        || !task.input_artifacts.contains(&eligible.gate_ref)
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
    if authority.resolution_sha256 != content_digest(authority.resolution.as_bytes()) {
        bail!(
            "metadata recovery Writer {:?} has a mismatched authorization digest",
            task.id
        );
    }
    Ok(())
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
    use crate::work_model::{PauseKind, TaskOutput, WorkItem};
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        project: std::path::PathBuf,
        workspace: std::path::PathBuf,
        store: WorkModelStore,
        candidate: String,
        base: String,
        gate: String,
    }

    fn git_ok(path: &Path, args: &[&str]) {
        git::run(path, args, "prepare candidate metadata recovery fixture").unwrap();
    }

    fn fixture() -> Fixture {
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
        git_ok(&workspace, &["commit", "-q", "-m", "Candidate"]);
        let candidate =
            git::run_stdout(&workspace, &["rev-parse", "HEAD"], "fixture candidate").unwrap();

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
            commit: candidate.clone(),
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
                candidate_commit: candidate.clone(),
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
            candidate,
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
    }

    fn metadata_diagnostic(candidate: &str) -> String {
        format!("commit {candidate} contains literal \\n text instead of line breaks")
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
    fn metadata_recovery_accepts_derived_project_check() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} has no body separated from its subject by a blank line\n",
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

        assert!(!repair.already_authorized);
        let item = fixture.store.read_work_item("work-1").unwrap();
        let writer = item.attempts[0].tasks.last().unwrap();
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: fixture.gate.clone(),
        }));
        assert!(writer.input_artifacts.iter().any(|artifact| {
            artifact.producer_id == "attempt-1-write-1"
                && artifact.path.ends_with("/hooks/check-pre-merge.log")
        }));
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
        assert!(writer.input_artifacts.contains(&ArtifactRef {
            producer_id: "attempt-1-write-1".to_string(),
            path: project_check_path.clone(),
        }));
        assert_eq!(
            authority.source_project_check_artifact.as_deref(),
            Some(project_check_path.as_str())
        );
        let project_check_sha256 = digest_path(&fixture.project, &project_check_path).unwrap();
        assert_eq!(
            authority.source_project_check_sha256.as_deref(),
            Some(project_check_sha256.as_str())
        );
    }

    #[test]
    fn metadata_recovery_accepts_hook_line_length_wording() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\n",
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
            Some(digest_path(&fixture.project, &project_check_path).unwrap().as_str())
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
    fn derived_project_check_authorizes_followup_candidate_gate() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} has no body separated from its subject by a blank line\n",
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
            "commit {} has no body separated from its subject by a blank line\n",
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
    fn metadata_recovery_hook_wording_is_idempotent() {
        let fixture = fixture();
        let metadata = metadata_diagnostic(&fixture.candidate);
        let hook = format!(
            "commit {} body line 1 exceeds 72 characters\n",
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
            "commit {} has no body separated from its subject by a blank line\n",
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
