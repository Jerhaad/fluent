//! Read-only recognition of narrowly supported lifecycle recovery frontiers.
//!
//! Queue admission and Attempt execution share these predicates so an explicit
//! resume cannot authorize a broader state than execution knows how to repair.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::work_model::{
    Attempt, AttemptReviewState, AttemptStatus, LearningFailureKind, MergeCandidateMergeStatus,
    MergeReviewState, TaskKind, TaskStatus, WorkItem, resolve_managed_sibling_workspace_path,
};

const LEGACY_LEARNER_GATE_SCHEMA_VERSION: u32 = 2;

/// The immutable frontier that authorizes a targeted uncertain-review retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UncertainReviewRecovery {
    pub writer_task_id: String,
    pub candidate_commit: String,
    pub latest_review_task_ids: Vec<String>,
    pub uncertain_roles: Vec<String>,
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
