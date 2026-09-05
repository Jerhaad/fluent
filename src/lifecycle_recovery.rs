//! Read-only recognition of narrowly supported lifecycle recovery frontiers.
//!
//! Queue admission and Attempt execution share these predicates so an explicit
//! resume cannot authorize a broader state than execution knows how to repair.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::work_model::{
    ArtifactRef, Attempt, AttemptReviewState, AttemptStatus, LearningFailureKind,
    MergeCandidateMergeStatus, MergeReviewState, PauseKind, Task, TaskKind, TaskStatus, WorkItem,
    resolve_managed_sibling_workspace_path, work_artifact_path,
};

const LEGACY_LEARNER_GATE_SCHEMA_VERSION: u32 = 2;
const RETIRED_COMMIT_STYLE_RECLASSIFICATION_PRODUCER: &str = "commit-style-policy";
const RETIRED_COMMIT_STYLE_RECLASSIFICATION_FILE: &str = "reclassification.json";
const RETIRED_COMMIT_STYLE_RECLASSIFICATION_SCHEMA_VERSION: u32 = 1;
const RETIRED_COMMIT_STYLE_RECLASSIFICATION_MAX_BYTES: u64 = 256 * 1024;

/// The immutable paused frontier that may reuse a candidate after Factory
/// retires only the stored commit-message presentation findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetiredCommitStyleRecovery {
    pub source_writer_task_id: String,
    pub correction_writer_task_id: String,
    pub base_commit: String,
    pub candidate_commit: String,
    pub candidate_tree: String,
    pub workspace_path: String,
    pub gate_artifact: ArtifactRef,
    pub gate_digest: String,
    pub hook_log_path: String,
    pub hook_log_digest: String,
    pub findings: Vec<String>,
    pub policy_commit: String,
}

/// Separate evidence that preserves the rejected gate while recording why the
/// same candidate may continue through the ordinary review lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct RetiredCommitStyleReclassification {
    pub schema_version: u32,
    pub work_item_id: String,
    pub attempt_id: String,
    pub source_writer_task_id: String,
    pub superseded_writer_task_id: String,
    pub base_commit: String,
    pub candidate_commit: String,
    pub candidate_tree: String,
    pub workspace_path: String,
    pub gate_path: String,
    pub gate_digest: String,
    pub hook_log_path: String,
    pub hook_log_digest: String,
    pub findings: Vec<String>,
    pub policy_commit: String,
    pub recorded_at: String,
}

impl RetiredCommitStyleReclassification {
    pub(crate) fn from_recovery(
        item: &WorkItem,
        attempt_id: &str,
        recovery: &RetiredCommitStyleRecovery,
    ) -> Self {
        Self {
            schema_version: RETIRED_COMMIT_STYLE_RECLASSIFICATION_SCHEMA_VERSION,
            work_item_id: item.id.clone(),
            attempt_id: attempt_id.to_string(),
            source_writer_task_id: recovery.source_writer_task_id.clone(),
            superseded_writer_task_id: recovery.correction_writer_task_id.clone(),
            base_commit: recovery.base_commit.clone(),
            candidate_commit: recovery.candidate_commit.clone(),
            candidate_tree: recovery.candidate_tree.clone(),
            workspace_path: recovery.workspace_path.clone(),
            gate_path: recovery.gate_artifact.path.clone(),
            gate_digest: recovery.gate_digest.clone(),
            hook_log_path: recovery.hook_log_path.clone(),
            hook_log_digest: recovery.hook_log_digest.clone(),
            findings: recovery.findings.clone(),
            policy_commit: recovery.policy_commit.clone(),
            recorded_at: crate::work_model::now_iso8601(),
        }
    }
}

pub(crate) fn retired_commit_style_reclassification_artifact(
    work_item_id: &str,
    attempt_id: &str,
    source_writer_task_id: &str,
) -> ArtifactRef {
    ArtifactRef {
        producer_id: RETIRED_COMMIT_STYLE_RECLASSIFICATION_PRODUCER.to_string(),
        path: work_artifact_path(
            work_item_id,
            attempt_id,
            &format!(
                "retired-commit-style-recovery/{source_writer_task_id}/{RETIRED_COMMIT_STYLE_RECLASSIFICATION_FILE}"
            ),
        ),
    }
}

/// Recognize a stopped output-free Writer whose only candidate-gate failure is
/// exact, candidate-bound output from Factory's retired presentation policy.
/// `Ok(None)` is reserved for unrelated Writer-completion pauses; once a pause
/// names a rejected candidate gate, every mismatch fails closed.
pub(crate) fn inspect_retired_commit_style_recovery(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
) -> Result<Option<RetiredCommitStyleRecovery>> {
    let Some(attempt) = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
    else {
        anyhow::bail!("Attempt {attempt_id:?} not found");
    };
    if attempt.status != AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(PauseKind::WriterCompletion)
    {
        return Ok(None);
    }
    let Some((correction_index, correction)) = attempt.tasks.iter().enumerate().next_back() else {
        return Ok(None);
    };
    if correction.kind != TaskKind::Write {
        return Ok(None);
    }
    let gate_inputs = correction
        .input_artifacts
        .iter()
        .filter(|artifact| {
            artifact
                .path
                .ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
        })
        .cloned()
        .collect::<Vec<_>>();
    if gate_inputs.is_empty() {
        return Ok(None);
    }
    if gate_inputs.len() != 1 {
        anyhow::bail!("retired commit-style Writer names multiple candidate gates");
    }
    if correction.status != TaskStatus::NeedsUser || correction.output.is_some() {
        anyhow::bail!("retired commit-style Writer is live or produced output");
    }
    if crate::lease::is_leased(&crate::lease::task_lock_path(
        project_root,
        &item.id,
        &correction.id,
    )) || task_execution_is_live(project_root, correction)?
    {
        anyhow::bail!("retired commit-style Writer still has a live execution");
    }
    if item.attempts.last().map(|attempt| attempt.id.as_str()) != Some(attempt_id) {
        anyhow::bail!("retired commit-style Attempt is no longer current");
    }
    if item
        .merge_candidates
        .iter()
        .any(|candidate| candidate.attempt_id == attempt_id)
    {
        anyhow::bail!("a Merge Candidate already owns the retired commit-style candidate");
    }
    if correction_index == 0 {
        anyhow::bail!("retired commit-style Writer has no preceding candidate Writer");
    }
    let source = &attempt.tasks[correction_index - 1];
    if source.kind != TaskKind::Write || source.status != TaskStatus::Complete {
        anyhow::bail!(
            "retired commit-style correction does not immediately follow a completed Writer"
        );
    }
    if attempt.tasks[..correction_index - 1]
        .iter()
        .any(|task| !matches!(task.status, TaskStatus::Complete | TaskStatus::Superseded))
    {
        anyhow::bail!("retired commit-style Attempt contains another unresolved Task");
    }
    let output = source
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("retired commit-style source Writer has no candidate"))?;
    let base_commit = output
        .base_commit
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("retired commit-style source Writer has no base commit"))?;
    let gate_artifact = gate_inputs[0].clone();
    let expected_gate = ArtifactRef {
        producer_id: source.id.clone(),
        path: format!(
            "{}/{}/{}",
            work_artifact_path(&item.id, attempt_id, "candidate-gates"),
            source.id,
            crate::candidate_gate::CANDIDATE_GATE_FILE
        ),
    };
    if gate_artifact != expected_gate
        || attempt
            .artifacts
            .iter()
            .filter(|artifact| **artifact == expected_gate)
            .count()
            != 1
    {
        anyhow::bail!("retired commit-style gate is not bound to the source Writer");
    }
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "retired commit-style candidate workspace",
    )?;
    if !crate::recovery_contracts::candidate_workspace_is_clean_at_managed(
        project_root,
        output,
        &output.commit,
    ) {
        anyhow::bail!("retired commit-style candidate workspace is not clean and unchanged");
    }
    let gate_path =
        crate::recovery_contracts::resolve_input_artifact_path(project_root, &gate_artifact.path)?;
    let classified = crate::candidate_gate::classify_retired_commit_style_gate(
        project_root,
        &workspace,
        &gate_path,
        base_commit,
        &output.commit,
    )?;
    Ok(Some(RetiredCommitStyleRecovery {
        source_writer_task_id: source.id.clone(),
        correction_writer_task_id: correction.id.clone(),
        base_commit: base_commit.clone(),
        candidate_commit: output.commit.clone(),
        candidate_tree: classified.candidate_tree,
        workspace_path: output.workspace_path.clone(),
        gate_artifact,
        gate_digest: classified.gate_digest,
        hook_log_path: classified.hook_log_path,
        hook_log_digest: classified.hook_log_digest,
        findings: classified.findings,
        policy_commit: classified.policy_commit,
    }))
}

/// Validate the recorded admission before the Attempt loop skips the rejected
/// gate. The original gate and hook bytes remain the authority, so later drift
/// blocks instead of silently trusting the receipt.
pub(crate) fn inspect_retired_commit_style_admission(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    source_writer: &Task,
) -> Result<Option<ArtifactRef>> {
    let artifact =
        retired_commit_style_reclassification_artifact(&item.id, attempt_id, &source_writer.id);
    let matches = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .into_iter()
        .flat_map(|attempt| attempt.artifacts.iter())
        .filter(|candidate| **candidate == artifact)
        .count();
    if matches == 0 {
        return Ok(None);
    }
    if matches != 1 {
        anyhow::bail!("retired commit-style reclassification artifact is duplicated");
    }
    let path =
        crate::recovery_contracts::resolve_input_artifact_path(project_root, &artifact.path)?;
    let receipt = read_retired_commit_style_reclassification(&path)?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    let correction = attempt
        .tasks
        .iter()
        .find(|task| task.id == receipt.superseded_writer_task_id)
        .ok_or_else(|| anyhow::anyhow!("retired commit-style superseded Writer is missing"))?;
    if correction.status != TaskStatus::Superseded || correction.output.is_some() {
        anyhow::bail!(
            "retired commit-style corrective Writer is no longer superseded and output-free"
        );
    }
    let output = source_writer
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("retired commit-style source Writer has no candidate"))?;
    let gate_path =
        crate::recovery_contracts::resolve_input_artifact_path(project_root, &receipt.gate_path)?;
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "retired commit-style admitted candidate workspace",
    )?;
    if !crate::recovery_contracts::candidate_workspace_is_clean_at_managed(
        project_root,
        output,
        &output.commit,
    ) {
        anyhow::bail!("retired commit-style admitted candidate is no longer clean and unchanged");
    }
    let classified = crate::candidate_gate::classify_retired_commit_style_gate(
        project_root,
        &workspace,
        &gate_path,
        output
            .base_commit
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("retired commit-style source Writer has no base"))?,
        &output.commit,
    )?;
    let expected = RetiredCommitStyleReclassification {
        schema_version: RETIRED_COMMIT_STYLE_RECLASSIFICATION_SCHEMA_VERSION,
        work_item_id: item.id.clone(),
        attempt_id: attempt_id.to_string(),
        source_writer_task_id: source_writer.id.clone(),
        superseded_writer_task_id: correction.id.clone(),
        base_commit: output.base_commit.clone().unwrap(),
        candidate_commit: output.commit.clone(),
        candidate_tree: classified.candidate_tree,
        workspace_path: output.workspace_path.clone(),
        gate_path: receipt.gate_path.clone(),
        gate_digest: classified.gate_digest,
        hook_log_path: classified.hook_log_path,
        hook_log_digest: classified.hook_log_digest,
        findings: classified.findings,
        policy_commit: classified.policy_commit,
        recorded_at: receipt.recorded_at.clone(),
    };
    if receipt.recorded_at.trim().is_empty() || receipt != expected {
        anyhow::bail!(
            "retired commit-style reclassification no longer matches its candidate evidence"
        );
    }
    if !correction.input_artifacts.contains(&ArtifactRef {
        producer_id: source_writer.id.clone(),
        path: receipt.gate_path.clone(),
    }) {
        anyhow::bail!("retired commit-style corrective Writer lost its source gate binding");
    }
    Ok(Some(artifact))
}

fn task_execution_is_live(project_root: &Path, task: &Task) -> Result<bool> {
    let Some(area) = task.artifact_area.as_ref() else {
        return Ok(false);
    };
    let path = crate::execution::record_path_for_artifact(project_root, &area.path);
    if !path.exists() {
        return Ok(false);
    }
    crate::execution::execution_is_live(&path)
}

pub(crate) fn read_retired_commit_style_reclassification(
    path: &Path,
) -> Result<RetiredCommitStyleReclassification> {
    serde_json::from_slice(&read_reclassification_bytes(path)?)
        .context("parse retired commit-style reclassification")
}

fn read_reclassification_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "inspect retired commit-style reclassification {}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("retired commit-style reclassification is not a regular file");
    }
    if metadata.len() > RETIRED_COMMIT_STYLE_RECLASSIFICATION_MAX_BYTES {
        anyhow::bail!("retired commit-style reclassification is too large");
    }
    fs::read(path).with_context(|| {
        format!(
            "read retired commit-style reclassification {}",
            path.display()
        )
    })
}

/// The immutable frontier that authorizes a targeted uncertain-review retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UncertainReviewRecovery {
    pub writer_task_id: String,
    pub candidate_commit: String,
    pub latest_review_task_ids: Vec<String>,
    pub uncertain_roles: Vec<String>,
}

/// Return whether this Attempt has the durable lifecycle shape of a failed
/// pre-land Learner. Callers use this coarse check only to select the detailed
/// inspector below; it grants no recovery authority by itself.
pub(crate) fn is_learner_retry_recovery(item: &WorkItem, attempt_id: &str) -> bool {
    let Some(attempt) = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
    else {
        return false;
    };
    attempt.status == AttemptStatus::Complete
        && attempt.review_state == Some(AttemptReviewState::Passed)
        && attempt
            .learning
            .as_ref()
            .is_some_and(|learning| learning.is_failed())
        && item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id
                && candidate.merge_state.status == MergeCandidateMergeStatus::Pending
        })
}

/// Verify that a completed passing Attempt still exposes exactly one safe,
/// relaunchable Learner frontier. Queue admission and claim both call this
/// function; the Attempt loop repeats its own candidate and Learning gates
/// immediately before launch.
pub(crate) fn inspect_learner_retry_recovery(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
) -> Result<()> {
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    if attempt.kind.is_review_only_like()
        || attempt.status != AttemptStatus::Complete
        || attempt.review_state != Some(AttemptReviewState::Passed)
    {
        anyhow::bail!("Attempt {attempt_id:?} is not a completed, review-passed Learner retry");
    }
    if attempt
        .tasks
        .iter()
        .any(|task| task.status != TaskStatus::Complete)
    {
        anyhow::bail!("Attempt {attempt_id:?} still has an active or incomplete Task");
    }
    let learning = attempt
        .learning
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} has no failed Learner record"))?;
    if !learning.is_failed() {
        anyhow::bail!("Attempt {attempt_id:?} Learner has not failed");
    }
    if !learning.is_relaunchable() {
        anyhow::bail!("Attempt {attempt_id:?} Learner failure is not relaunchable");
    }
    if crate::lease::is_leased(&crate::lease::learner_lock_path(
        project_root,
        &item.id,
        attempt_id,
    )) {
        anyhow::bail!("Attempt {attempt_id:?} already has a live Learner run");
    }

    let mut candidates = item.merge_candidates.iter().filter(|candidate| {
        candidate.attempt_id == attempt_id
            && candidate.merge_state.status == MergeCandidateMergeStatus::Pending
    });
    let candidate = candidates
        .next()
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} has no pending Merge Candidate"))?;
    if candidates.next().is_some() {
        anyhow::bail!("Attempt {attempt_id:?} has multiple pending Merge Candidates");
    }
    if candidate.merge_review_state != MergeReviewState::Pending {
        anyhow::bail!("Learner retry candidate is no longer pending review");
    }
    candidate.validate(item)?;

    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &candidate.source_workspace.path,
        "Learner retry candidate workspace",
    )?;
    let head = crate::git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "resolve Learner retry candidate HEAD",
    )?;
    if head != candidate.candidate_commit {
        anyhow::bail!(
            "Learner retry candidate HEAD no longer matches {}",
            candidate.candidate_commit
        );
    }
    let status = crate::git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
        "check Learner retry candidate cleanliness",
    )?;
    if !status.is_empty() {
        anyhow::bail!("Learner retry candidate workspace is dirty");
    }

    Ok(())
}

/// Verify that an uncertain review pause still points at one clean candidate and
/// readable latest verdicts. Callers may reopen only the returned roles.
pub(crate) fn inspect_uncertain_review_recovery(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
) -> Result<UncertainReviewRecovery> {
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    attempt.ensure_resume_ready()?;
    if !attempt.is_uncertain_review_pause() {
        anyhow::bail!("Attempt {attempt_id:?} is not paused on uncertain reviews");
    }
    if item
        .merge_candidates
        .iter()
        .any(|candidate| candidate.attempt_id == attempt_id)
    {
        anyhow::bail!("uncertain-review recovery cannot reopen an existing Merge Candidate");
    }

    let write_index = attempt
        .tasks
        .iter()
        .rposition(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| anyhow::anyhow!("uncertain-review pause has no completed Writer Task"))?;
    let writer = &attempt.tasks[write_index];
    let output = writer
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("uncertain-review Writer has no candidate output"))?;
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "uncertain-review candidate workspace",
    )?;
    let head = crate::git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "resolve uncertain-review candidate HEAD",
    )?;
    if head != output.commit {
        anyhow::bail!(
            "uncertain-review candidate HEAD no longer matches {}",
            output.commit
        );
    }
    let status = crate::git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
        "check uncertain-review candidate cleanliness",
    )?;
    if !status.is_empty() {
        anyhow::bail!("uncertain-review candidate workspace is dirty");
    }

    let mut latest: Vec<(String, &crate::work_model::Task)> = Vec::new();
    for task in attempt.tasks[write_index + 1..].iter().filter(|task| {
        task.kind == TaskKind::Review
            && task.status == TaskStatus::Complete
            && crate::review::REVIEWERS.contains(&task.role.as_str())
    }) {
        if let Some((_, prior)) = latest.iter_mut().find(|(role, _)| role == &task.role) {
            *prior = task;
        } else {
            latest.push((task.role.clone(), task));
        }
    }
    if latest.is_empty() {
        anyhow::bail!("uncertain-review pause has no completed reviewer evidence");
    }

    let mut latest_review_task_ids = Vec::new();
    let mut uncertain_roles = Vec::new();
    for (role, task) in latest {
        let context = task
            .review_context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("review Task {:?} has no candidate context", task.id))?;
        if context.candidate_commit != output.commit
            || context.candidate_workspace_id != output.workspace_id
            || context.candidate_workspace_path != output.workspace_path
            || context.source_branch != output.source_branch
        {
            anyhow::bail!(
                "review Task {:?} does not match the current candidate",
                task.id
            );
        }
        let area = task
            .artifact_area
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("review Task {:?} has no artifact area", task.id))?;
        let artifact_dir = crate::work_task_executor::resolve_managed_artifact_area_path(
            project_root,
            &area.path,
        )?;
        let review_path = artifact_dir.join("review.md");
        let content = fs::read_to_string(&review_path)
            .with_context(|| format!("read review evidence {}", review_path.display()))?;
        match crate::review::extract_verdict(&content) {
            crate::review::Verdict::Pass => {}
            crate::review::Verdict::Uncertain => uncertain_roles.push(role),
            crate::review::Verdict::Fail => {
                anyhow::bail!("review Task {:?} now reports fail, not uncertain", task.id)
            }
        }
        latest_review_task_ids.push(task.id.clone());
    }
    if uncertain_roles.is_empty() {
        anyhow::bail!("uncertain-review pause has no uncertain latest verdict");
    }

    Ok(UncertainReviewRecovery {
        writer_task_id: writer.id.clone(),
        candidate_commit: output.commit.clone(),
        latest_review_task_ids,
        uncertain_roles,
    })
}

fn latest_tester_results_path(project_root: &Path, attempt: &Attempt) -> Option<PathBuf> {
    let start = attempt
        .tasks
        .iter()
        .rposition(|task| task.kind == TaskKind::Write)
        .map(|index| index + 1)
        .unwrap_or(0);
    attempt.tasks[start..]
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Tester && task.status == TaskStatus::Complete)
        .and_then(|task| task.artifact_area.as_ref())
        .and_then(|area| {
            crate::work_task_executor::resolve_managed_artifact_area_path(project_root, &area.path)
                .ok()
        })
        .map(|directory| directory.join("tester-results.json"))
        .filter(|path| path.is_file())
}

/// Recognize the exact pre-final-Tester-gate failure produced by Fluent 0.1.5.
/// A generic diagnostic alone is never enough to rewrite lifecycle state.
pub(crate) fn is_legacy_learner_tester_evidence_failure(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
) -> Result<bool> {
    let Some(attempt) = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
    else {
        return Ok(false);
    };
    let Some(learning) = attempt.learning.as_ref() else {
        return Ok(false);
    };
    let tester_results = latest_tester_results_path(project_root, attempt);
    if attempt.kind.is_review_only_like()
        || attempt.status != AttemptStatus::Complete
        || attempt.review_state != Some(AttemptReviewState::Passed)
        || attempt.writer_completion_contract.is_none()
        || !learning.is_failed()
        || learning.failure_kind != Some(LearningFailureKind::Generic)
        || learning.failure_stage.is_some()
        || !learning
            .last_failure
            .as_deref()
            .is_some_and(|failure| failure.contains("Learner candidate gate rejected"))
        || tester_results.is_none()
    {
        return Ok(false);
    }
    let Some(writer_output) = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
    else {
        return Ok(false);
    };
    let Some(candidate) = item
        .merge_candidates
        .iter()
        .find(|candidate| candidate.attempt_id == attempt_id)
    else {
        return Ok(false);
    };
    if candidate.merge_state.status != MergeCandidateMergeStatus::Pending
        || candidate.merge_review_state != MergeReviewState::Pending
        || candidate.candidate_commit != writer_output.commit
    {
        return Ok(false);
    }

    let runs_dir = project_root
        .join(crate::learner::handoff_dir_rel(&item.id, attempt_id))
        .join("runs");
    let entries = match fs::read_dir(&runs_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read Learner run directory {}", runs_dir.display()));
        }
    };
    if learning.runs == 0 {
        return Ok(false);
    }
    for entry in entries {
        let entry = entry
            .with_context(|| format!("read a Learner run entry beneath {}", runs_dir.display()))?;
        let name = entry.file_name();
        let Some(run) = name
            .to_str()
            .and_then(|name| name.strip_prefix("run-"))
            .and_then(|run| run.parse::<u32>().ok())
        else {
            continue;
        };
        if run > learning.runs {
            let mut contents = fs::read_dir(entry.path()).with_context(|| {
                format!(
                    "inspect unrecorded Learner run directory {}",
                    entry.path().display()
                )
            })?;
            if contents.next().transpose()?.is_some() {
                return Ok(false);
            }
        }
    }
    // Capture mode may preallocate empty bounded-retry directories. The durable
    // run counter, not the highest directory name, identifies the completed run.
    // A non-empty later directory is possible newer evidence and fails closed.
    let gate_path = runs_dir
        .join(format!("run-{}", learning.runs))
        .join(crate::candidate_gate::CANDIDATE_GATE_FILE);
    if !gate_path.is_file() {
        return Ok(false);
    }
    let evidence = crate::candidate_gate::read_prior_writer_gate(&gate_path)
        .map_err(|reason| anyhow::anyhow!(reason))?;
    let missing_tester_evidence = evidence.checks.iter().filter(|check| {
        check.name == "tester-reference-evidence"
            && check.status == crate::candidate_gate::CandidateCheckStatus::Failed
            && !check.diagnostics.is_empty()
    });
    let expected_checks = [
        "candidate-identity",
        "whitespace",
        "commit-metadata",
        "behavior-references",
        "tester-reference-evidence",
        "project-check",
    ];
    let checks_are_complete = evidence.checks.len() == expected_checks.len()
        && expected_checks.iter().all(|name| {
            evidence
                .checks
                .iter()
                .filter(|check| check.name == *name)
                .count()
                == 1
        });
    let check_status = |name: &str| {
        evidence
            .checks
            .iter()
            .find(|check| check.name == name)
            .map(|check| check.status)
    };
    let statuses_match_legacy_gate = check_status("candidate-identity")
        == Some(crate::candidate_gate::CandidateCheckStatus::Passed)
        && matches!(
            check_status("whitespace"),
            Some(
                crate::candidate_gate::CandidateCheckStatus::Passed
                    | crate::candidate_gate::CandidateCheckStatus::Failed
            )
        )
        && check_status("commit-metadata")
            == Some(crate::candidate_gate::CandidateCheckStatus::Passed)
        && check_status("behavior-references")
            == Some(crate::candidate_gate::CandidateCheckStatus::Passed)
        && check_status("tester-reference-evidence")
            == Some(crate::candidate_gate::CandidateCheckStatus::Failed)
        && matches!(
            check_status("project-check"),
            Some(
                crate::candidate_gate::CandidateCheckStatus::Passed
                    | crate::candidate_gate::CandidateCheckStatus::Skipped
            )
        );
    let tester_results = tester_results.expect("checked above");
    let expected_tester_path = tester_results
        .strip_prefix(project_root)
        .unwrap_or(&tester_results)
        .to_string_lossy();
    Ok(
        evidence.schema_version == LEGACY_LEARNER_GATE_SCHEMA_VERSION
            && evidence.phase == crate::candidate_gate::CandidateGatePhase::Learner
            && evidence.disposition == crate::candidate_gate::CandidateGateDisposition::Rejected
            && evidence.base_commit == writer_output.commit
            && evidence.tester_results_path.as_deref() == Some(expected_tester_path.as_ref())
            && evidence.metadata_rewrite.is_none()
            && checks_are_complete
            && statuses_match_legacy_gate
            && missing_tester_evidence.count() == 1,
    )
}
