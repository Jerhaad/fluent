//! Recover one host-verified metadata rewrite that accidentally replaced the
//! candidate's protected base while preserving the exact candidate tree.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::candidate_gate::{
    CandidateAncestryRepairAuthorization, CandidateGateEvidence,
    candidate_ancestry_repair_authorization, read_prior_writer_gate, validate_prior_writer_gate,
};
use crate::work_model::{
    ArtifactRef, Attempt, AttemptReviewState, AttemptStatus, CANDIDATE_ANCESTRY_REPAIR_ROLE,
    DurableTransitionMetadata, DurableTransitionOrigin, MAX_ROUND_CAP_RESOLUTION_BYTES, PauseKind,
    TaskKind, TaskStatus, WorkItem, WorkModelError, WorkModelStore, WriterCompletionMatrix,
    now_iso8601, resolve_managed_sibling_workspace_path, round_cap_resolution_digest,
    work_artifact_path,
};

const RECOVERY_SCHEMA_VERSION: u32 = 1;
const RECOVERY_PRODUCER: &str = "candidate-ancestry-recovery";
const RECOVERY_FILE: &str = "recovery.json";
const SUPERSEDED_ATTEMPTS_FILE: &str = "superseded-attempts.json";
const SUPERSEDED_ATTEMPT_ROLE: &str = "superseded-candidate-ancestry-queue-admission";
const MAX_RECOVERY_EVIDENCE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CandidateAncestryRecoveryEvidence {
    schema_version: u32,
    task_id: String,
    source_task_id: String,
    source_candidate_commit: String,
    rejected_candidate_commit: String,
    protected_base_commit: String,
    recovered_candidate_commit: String,
    candidate_tree: String,
    source_gate_path: String,
    prior_gate_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_recovery_path: Option<String>,
    preserved_ref: String,
    resolution: String,
    resolution_digest: String,
    #[serde(default)]
    recovery_mode: RecoveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseded_candidate_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authorization_extension_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authorization_resolution_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RecoveryMode {
    #[default]
    ReconstructProtectedBase,
    AcceptAuthorizedHistoryRewrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SupersededAttemptsEvidence {
    schema_version: u32,
    repair_task_id: String,
    resumed_attempt_id: String,
    superseded_attempt_ids: Vec<String>,
    recovery_artifact: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateAncestryRepair {
    pub task_id: String,
    pub source_candidate_commit: String,
    pub recovered_candidate_commit: String,
    pub protected_base_commit: String,
    pub already_repaired: bool,
}

struct EligibleRecovery {
    work_item_id: String,
    attempt_id: String,
    source_task_id: String,
    workspace_path: String,
    source_branch: String,
    source_candidate_commit: String,
    source_gate_path: String,
    authorization: CandidateAncestryRepairAuthorization,
    prior_recovery_path: Option<String>,
    task_id: String,
    recovery_mode: RecoveryMode,
    authorization_extension_id: Option<String>,
    authorization_resolution_digest: Option<String>,
}

pub fn recovery_action(project_root: &Path, attempt: &Attempt) -> Option<String> {
    inspect_eligible(project_root, attempt).ok().map(|_| {
        format!(
            "fluent attempt repair-candidate-ancestry {} {} --resolution \"<describe the approved ancestry repair>\"",
            attempt.work_item_id, attempt.id
        )
    })
}

pub fn scheduler_action(work_item_id: &str, attempt_id: &str) -> String {
    format!("fluent queue resume {work_item_id} --attempt {attempt_id}")
}

pub fn repair(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    resolution: &str,
) -> Result<CandidateAncestryRepair> {
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire candidate ancestry recovery lock")?;
    let resolution = normalize_resolution(resolution)?;
    let item = store.read_work_item(work_item_id)?;
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    if let Some(repair) = completed_repair(project_root, attempt)? {
        let latest_write_id = attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .map(|task| task.id.as_str());
        if latest_write_id == Some(repair.task_id.as_str()) {
            reconcile_completed_repair_review_inputs(store, work_item_id, attempt_id, &repair)?;
            reconcile_superseded_queue_admissions(
                project_root,
                store,
                work_item_id,
                attempt_id,
                &repair,
            )?;
            return Ok(repair);
        }
    }
    let eligible = inspect_eligible(project_root, attempt)?;
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &eligible.workspace_path,
        "candidate ancestry recovery workspace",
    )?;
    ensure_registered_worktree(project_root, &workspace)?;
    let status = crate::git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all"],
        "verify candidate ancestry recovery cleanliness",
    )?;
    if !status.is_empty() {
        bail!("candidate ancestry repair requires a clean candidate worktree");
    }
    verify_gate_provenance(project_root, &eligible)?;
    let recovery_path = recovery_artifact_path(work_item_id, attempt_id, &eligible.task_id);
    let evidence = match read_recovery_evidence(project_root, &recovery_path)? {
        Some(evidence) => {
            validate_recovery_evidence(&workspace, &eligible, &evidence)?;
            evidence
        }
        None => {
            verify_live_candidate(&workspace, &eligible)?;
            let tree = crate::git::run_stdout(
                &workspace,
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", eligible.source_candidate_commit),
                ],
                "resolve candidate ancestry repair tree",
            )?;
            if tree != eligible.authorization.candidate_tree {
                bail!(
                    "candidate tree changed after the blocked gate: expected {}, found {tree}",
                    eligible.authorization.candidate_tree
                );
            }
            let recovered = match eligible.recovery_mode {
                RecoveryMode::ReconstructProtectedBase => {
                    let source_message = crate::git::run_stdout(
                        &workspace,
                        &[
                            "show",
                            "-s",
                            "--format=%B",
                            &eligible.source_candidate_commit,
                        ],
                        "read candidate ancestry recovery message",
                    )?;
                    if source_message.trim().is_empty() {
                        bail!("candidate ancestry repair source has an empty commit message");
                    }
                    crate::git::run_stdout(
                        &workspace,
                        &[
                            "commit-tree",
                            &tree,
                            "-p",
                            &eligible.authorization.protected_base_commit,
                            "-m",
                            &source_message,
                        ],
                        "create candidate ancestry recovery commit",
                    )?
                }
                RecoveryMode::AcceptAuthorizedHistoryRewrite => {
                    eligible.source_candidate_commit.clone()
                }
            };
            let preserved_ref = format!(
                "refs/fluent/recovery/{work_item_id}/{attempt_id}/candidate-ancestry-{}",
                eligible.source_candidate_commit
            );
            preserve_source_ref(
                &workspace,
                &preserved_ref,
                &eligible.source_candidate_commit,
            )?;
            let evidence = CandidateAncestryRecoveryEvidence {
                schema_version: RECOVERY_SCHEMA_VERSION,
                task_id: eligible.task_id.clone(),
                source_task_id: eligible.source_task_id.clone(),
                source_candidate_commit: eligible.source_candidate_commit.clone(),
                rejected_candidate_commit: eligible.authorization.rejected_candidate_commit.clone(),
                protected_base_commit: eligible.authorization.protected_base_commit.clone(),
                recovered_candidate_commit: recovered,
                candidate_tree: tree,
                source_gate_path: eligible.source_gate_path.clone(),
                prior_gate_path: eligible.authorization.prior_gate_path.clone(),
                prior_recovery_path: eligible.prior_recovery_path.clone(),
                preserved_ref,
                resolution: resolution.clone(),
                resolution_digest: round_cap_resolution_digest(&resolution),
                recovery_mode: eligible.recovery_mode,
                superseded_candidate_ref: (eligible.recovery_mode
                    == RecoveryMode::AcceptAuthorizedHistoryRewrite)
                    .then(|| {
                        format!(
                            "refs/fluent/recovery/{work_item_id}/{attempt_id}/superseded-candidate-{}",
                            eligible.authorization.rejected_candidate_commit
                        )
                    }),
                authorization_extension_id: eligible.authorization_extension_id.clone(),
                authorization_resolution_digest: eligible
                    .authorization_resolution_digest
                    .clone(),
            };
            validate_recovery_evidence(&workspace, &eligible, &evidence)?;
            write_evidence_once(project_root, &recovery_path, &evidence)?;
            evidence
        }
    };
    preserve_source_ref(
        &workspace,
        &evidence.preserved_ref,
        &eligible.source_candidate_commit,
    )?;
    if let Some(reference) = evidence.superseded_candidate_ref.as_deref() {
        preserve_source_ref(
            &workspace,
            reference,
            &eligible.authorization.rejected_candidate_commit,
        )?;
    }
    clone_completion_matrix(
        project_root,
        attempt,
        &eligible.source_task_id,
        &eligible.task_id,
    )?;
    move_candidate_ref(
        &workspace,
        &eligible.source_candidate_commit,
        &evidence.recovered_candidate_commit,
    )?;

    let recovery_artifact = ArtifactRef {
        producer_id: RECOVERY_PRODUCER.to_string(),
        path: recovery_path,
    };
    let expected_source = eligible.source_candidate_commit.clone();
    let expected_gate = eligible.source_gate_path.clone();
    let protected_base = eligible.authorization.protected_base_commit.clone();
    let task_id = eligible.task_id.clone();
    let recovered_for_state = evidence.recovered_candidate_commit.clone();
    let transition = DurableTransitionMetadata {
        origin: DurableTransitionOrigin::AttemptExecutor,
        reason: Some("repair candidate ancestry".to_string()),
        evidence_ref: Some(recovery_artifact.path.clone()),
    };
    store.mutate_work_item_with_transition(work_item_id, &transition, |fresh| {
        let attempt = fresh
            .attempts
            .iter()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        if attempt.tasks.iter().any(|task| {
            task.id == task_id
                && task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE
                && task.output.as_ref().is_some_and(|output| {
                    output.commit == recovered_for_state
                        && output.base_commit.as_deref() == Some(protected_base.as_str())
                })
        }) {
            return Ok(());
        }
        let latest = attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref().map(|output| (task, output)))
            .ok_or_else(|| WorkModelError::AttemptResumeIneligible {
                attempt_id: attempt_id.to_string(),
                reason: "candidate ancestry source Writer disappeared".to_string(),
            })?;
        if latest.0.id != eligible.source_task_id
            || latest.1.commit != expected_source
            || attempt.status != AttemptStatus::NeedsUser
            || attempt.pause_kind != Some(PauseKind::CandidateGate)
            || attempt.review_state != Some(AttemptReviewState::NotReviewed)
            || !attempt
                .artifacts
                .iter()
                .any(|artifact| artifact.path == expected_gate)
        {
            return Err(WorkModelError::AttemptResumeIneligible {
                attempt_id: attempt_id.to_string(),
                reason: "the candidate ancestry frontier changed before recovery".to_string(),
            });
        }
        fresh.add_completed_candidate_ancestry_repair(
            attempt_id,
            &protected_base,
            &recovered_for_state,
            recovery_artifact.clone(),
        )?;
        let repaired = fresh
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .expect("repair method retained Attempt");
        if !repaired.artifacts.contains(&recovery_artifact) {
            repaired.artifacts.push(recovery_artifact.clone());
        }
        Ok(())
    })?;

    Ok(CandidateAncestryRepair {
        task_id,
        source_candidate_commit: eligible.source_candidate_commit,
        recovered_candidate_commit: evidence.recovered_candidate_commit,
        protected_base_commit: protected_base,
        already_repaired: false,
    })
}

/// Whether an exact Attempt repaired by the host can be rebound to a resume
/// dispatch. Any later Attempt must be an audited, output-free queue admission
/// that this recovery explicitly superseded; ordinary candidate drift remains
/// ineligible.
pub fn is_resume_ready(project_root: &Path, item: &WorkItem, attempt_id: &str) -> Result<bool> {
    let Some(index) = item
        .attempts
        .iter()
        .position(|attempt| attempt.id == attempt_id)
    else {
        return Ok(false);
    };
    let Some(repair) = completed_repair(project_root, &item.attempts[index])? else {
        return Ok(false);
    };
    if !matches!(
        item.attempts[index].status,
        AttemptStatus::Planned | AttemptStatus::Reviewing
    ) {
        return Ok(false);
    }
    let later = &item.attempts[index + 1..];
    if later.is_empty() {
        return Ok(true);
    }
    let evidence_path = superseded_attempts_artifact_path(&item.id, attempt_id, &repair.task_id);
    let bytes = match fs::read(project_root.join(&evidence_path)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let evidence: SupersededAttemptsEvidence = serde_json::from_slice(&bytes)
        .context("parse superseded candidate-ancestry queue admissions")?;
    let expected_ids = later
        .iter()
        .map(|attempt| attempt.id.clone())
        .collect::<Vec<_>>();
    if evidence.schema_version != RECOVERY_SCHEMA_VERSION
        || evidence.repair_task_id != repair.task_id
        || evidence.resumed_attempt_id != attempt_id
        || evidence.superseded_attempt_ids != expected_ids
        || evidence.recovery_artifact
            != recovery_artifact_path(&item.id, attempt_id, &repair.task_id)
    {
        return Ok(false);
    }
    Ok(later.iter().all(|attempt| {
        attempt.status == AttemptStatus::Failed
            && attempt.pause_kind.is_none()
            && attempt.tasks.len() == 1
            && has_preserved_superseded_task(attempt)
            && attempt.tasks[0].input_artifacts.iter().any(|artifact| {
                artifact.producer_id == RECOVERY_PRODUCER && artifact.path == evidence_path
            })
    }))
}

fn reconcile_superseded_queue_admissions(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    repair: &CandidateAncestryRepair,
) -> Result<()> {
    let current = store.read_work_item(work_item_id)?;
    let index = current
        .attempts
        .iter()
        .position(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    let later = &current.attempts[index + 1..];
    if later.is_empty() {
        return Ok(());
    }
    let evidence_path =
        superseded_attempts_artifact_path(work_item_id, attempt_id, &repair.task_id);
    if is_resume_ready(project_root, &current, attempt_id)? {
        return Ok(());
    }
    let prior_supersession =
        prior_superseded_admissions_artifact(project_root, &current, index, attempt_id)?;
    for attempt in later {
        if !is_safe_interrupted_queue_admission(attempt)
            && !prior_supersession
                .as_ref()
                .is_some_and(|artifact| is_superseded_queue_admission(attempt, artifact))
        {
            bail!(
                "Attempt {:?} followed the repaired Attempt and is not an interrupted, output-free queue admission; Fluent will not supersede it",
                attempt.id
            );
        }
    }
    let superseded_attempt_ids = later
        .iter()
        .map(|attempt| attempt.id.clone())
        .collect::<Vec<_>>();
    let evidence = SupersededAttemptsEvidence {
        schema_version: RECOVERY_SCHEMA_VERSION,
        repair_task_id: repair.task_id.clone(),
        resumed_attempt_id: attempt_id.to_string(),
        superseded_attempt_ids: superseded_attempt_ids.clone(),
        recovery_artifact: recovery_artifact_path(work_item_id, attempt_id, &repair.task_id),
    };
    write_bytes_once(
        &project_root.join(&evidence_path),
        &serde_json::to_vec_pretty(&evidence)?,
    )?;
    let artifact = ArtifactRef {
        producer_id: RECOVERY_PRODUCER.to_string(),
        path: evidence_path.clone(),
    };
    let transition = DurableTransitionMetadata {
        origin: DurableTransitionOrigin::AttemptExecutor,
        reason: Some(
            "supersede accidental queue admission after candidate ancestry repair".to_string(),
        ),
        evidence_ref: Some(evidence_path),
    };
    let prior_supersession_for_guard = prior_supersession.clone();
    store.mutate_work_item_with_transition(work_item_id, &transition, |item| {
        let target_index = item
            .attempts
            .iter()
            .position(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        let fresh_ids = item.attempts[target_index + 1..]
            .iter()
            .map(|attempt| attempt.id.clone())
            .collect::<Vec<_>>();
        if fresh_ids != superseded_attempt_ids
            || item.attempts[target_index + 1..].iter().any(|attempt| {
                !is_safe_interrupted_queue_admission(attempt)
                    && !prior_supersession_for_guard
                        .as_ref()
                        .is_some_and(|artifact| is_superseded_queue_admission(attempt, artifact))
            })
        {
            return Err(WorkModelError::AttemptResumeIneligible {
                attempt_id: attempt_id.to_string(),
                reason: "a later queue admission changed before ancestry recovery reconciliation"
                    .to_string(),
            });
        }
        for attempt in &mut item.attempts[target_index + 1..] {
            let task = &mut attempt.tasks[0];
            task.status = if task.output.is_some() {
                TaskStatus::Complete
            } else {
                TaskStatus::Superseded
            };
            task.role = SUPERSEDED_ATTEMPT_ROLE.to_string();
            task.completed_at.get_or_insert_with(now_iso8601);
            if !task.input_artifacts.contains(&artifact) {
                task.input_artifacts.push(artifact.clone());
            }
            attempt.status = AttemptStatus::Failed;
            attempt.pause_kind = None;
            attempt.pause_recovery = None;
            attempt.completed_at.get_or_insert_with(now_iso8601);
            if !attempt.artifacts.contains(&artifact) {
                attempt.artifacts.push(artifact.clone());
            }
        }
        Ok(())
    })?;
    let refreshed = store.read_work_item(work_item_id)?;
    if !is_resume_ready(project_root, &refreshed, attempt_id)? {
        bail!(
            "candidate ancestry queue-admission reconciliation did not produce a resumable exact Attempt"
        );
    }
    Ok(())
}

fn is_safe_interrupted_queue_admission(attempt: &Attempt) -> bool {
    attempt.status == AttemptStatus::NeedsUser
        && attempt.pause_kind == Some(PauseKind::Interrupted)
        && attempt.tasks.len() == 1
        && attempt.tasks[0].kind == TaskKind::Write
        && attempt.tasks[0].status == TaskStatus::NeedsUser
        && attempt.tasks[0].output.is_none()
}

fn is_superseded_queue_admission(attempt: &Attempt, artifact: &ArtifactRef) -> bool {
    attempt.status == AttemptStatus::Failed
        && attempt.pause_kind.is_none()
        && attempt.tasks.len() == 1
        && attempt.tasks[0].kind == TaskKind::Write
        && has_preserved_superseded_task(attempt)
        && attempt.tasks[0].input_artifacts.contains(artifact)
        && attempt.artifacts.contains(artifact)
}

fn has_preserved_superseded_task(attempt: &Attempt) -> bool {
    let task = &attempt.tasks[0];
    if task.role != SUPERSEDED_ATTEMPT_ROLE {
        return false;
    }
    match (&task.status, &task.output) {
        (TaskStatus::Superseded, None) => true,
        (TaskStatus::Complete, Some(output)) => attempt
            .artifacts
            .iter()
            .any(|artifact| artifact.producer_id == task.id && artifact.path == output.commit),
        _ => false,
    }
}

fn prior_superseded_admissions_artifact(
    project_root: &Path,
    item: &WorkItem,
    target_index: usize,
    attempt_id: &str,
) -> Result<Option<ArtifactRef>> {
    let later = &item.attempts[target_index + 1..];
    let Some(first) = later.first() else {
        return Ok(None);
    };
    let Some(artifact) = first.tasks.first().and_then(|task| {
        task.input_artifacts.iter().find(|artifact| {
            artifact.producer_id == RECOVERY_PRODUCER
                && artifact.path.ends_with(SUPERSEDED_ATTEMPTS_FILE)
        })
    }) else {
        return Ok(None);
    };
    if !later
        .iter()
        .all(|attempt| is_superseded_queue_admission(attempt, artifact))
    {
        return Ok(None);
    }
    let evidence: SupersededAttemptsEvidence =
        serde_json::from_slice(&fs::read(project_root.join(&artifact.path))?)
            .context("parse prior superseded candidate-ancestry queue admissions")?;
    let expected_ids = later
        .iter()
        .map(|attempt| attempt.id.clone())
        .collect::<Vec<_>>();
    let expected_recovery = recovery_artifact_path(&item.id, attempt_id, &evidence.repair_task_id);
    if evidence.schema_version != RECOVERY_SCHEMA_VERSION
        || evidence.resumed_attempt_id != attempt_id
        || evidence.superseded_attempt_ids != expected_ids
        || evidence.recovery_artifact != expected_recovery
    {
        bail!("prior candidate-ancestry queue supersession evidence is invalid");
    }
    let target = &item.attempts[target_index];
    let Some(repair_task) = target.tasks.iter().find(|task| {
        task.id == evidence.repair_task_id
            && task.kind == TaskKind::Write
            && task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE
            && task.status == TaskStatus::Complete
    }) else {
        bail!("prior queue supersession does not name a completed ancestry repair");
    };
    let recovery = read_recovery_evidence(project_root, &expected_recovery)?
        .ok_or_else(|| anyhow::anyhow!("prior queue supersession recovery evidence is missing"))?;
    if recovery.task_id != repair_task.id
        || repair_task
            .output
            .as_ref()
            .map(|output| output.commit.as_str())
            != Some(recovery.recovered_candidate_commit.as_str())
    {
        bail!("prior queue supersession recovery evidence does not match its repair Task");
    }
    Ok(Some(artifact.clone()))
}

fn superseded_attempts_artifact_path(
    work_item_id: &str,
    attempt_id: &str,
    task_id: &str,
) -> String {
    work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("candidate-ancestry-repairs/{task_id}/{SUPERSEDED_ATTEMPTS_FILE}"),
    )
}

fn reconcile_completed_repair_review_inputs(
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    repair: &CandidateAncestryRepair,
) -> Result<()> {
    let evidence_path = recovery_artifact_path(work_item_id, attempt_id, &repair.task_id);
    let evidence = ArtifactRef {
        producer_id: RECOVERY_PRODUCER.to_string(),
        path: evidence_path.clone(),
    };
    let current = store.read_work_item(work_item_id)?;
    let current_attempt = current
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    let needs_reconciliation = current_attempt.tasks.iter().any(|task| {
        task.kind == TaskKind::Review
            && task.review_context.as_ref().is_some_and(|context| {
                context.candidate_commit == repair.recovered_candidate_commit
            })
            && (task.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == repair.task_id
                    && artifact.path.ends_with("/transcript.jsonl")
            }) || !task.input_artifacts.contains(&evidence))
    });
    if !needs_reconciliation {
        return Ok(());
    }
    let transition = DurableTransitionMetadata {
        origin: DurableTransitionOrigin::AttemptExecutor,
        reason: Some("reconcile candidate ancestry review inputs".to_string()),
        evidence_ref: Some(evidence_path),
    };
    store.mutate_work_item_with_transition(work_item_id, &transition, |item| {
        let attempt = item
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        for task in attempt.tasks.iter_mut().filter(|task| {
            task.kind == TaskKind::Review
                && task.review_context.as_ref().is_some_and(|context| {
                    context.candidate_commit == repair.recovered_candidate_commit
                })
        }) {
            let stale_transcript = task.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == repair.task_id
                    && artifact.path.ends_with("/transcript.jsonl")
            });
            let missing_evidence = !task.input_artifacts.contains(&evidence);
            if (stale_transcript || missing_evidence) && task.status != TaskStatus::Planned {
                return Err(WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: format!(
                        "review Task {:?} advanced before host recovery inputs were reconciled",
                        task.id
                    ),
                });
            }
            task.input_artifacts.retain(|artifact| {
                !(artifact.producer_id == repair.task_id
                    && artifact.path.ends_with("/transcript.jsonl"))
            });
            if missing_evidence {
                task.input_artifacts.push(evidence.clone());
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn inspect_eligible(project_root: &Path, attempt: &Attempt) -> Result<EligibleRecovery> {
    if attempt.status != AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(PauseKind::CandidateGate)
        || attempt.review_state != Some(AttemptReviewState::NotReviewed)
    {
        bail!("Attempt is not paused at a pre-review candidate gate");
    }
    let source = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| anyhow::anyhow!("candidate ancestry recovery has no completed Writer"))?;
    let output = source
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("candidate ancestry source Writer has no output"))?;
    let gate_ref = attempt
        .artifacts
        .iter()
        .rev()
        .find(|artifact| {
            artifact.producer_id == source.id
                && artifact
                    .path
                    .ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
        })
        .ok_or_else(|| anyhow::anyhow!("candidate ancestry source gate is missing"))?;
    let gate_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &gate_ref.path,
    )?;
    let gate: CandidateGateEvidence =
        read_prior_writer_gate(&gate_path).map_err(anyhow::Error::msg)?;
    if gate.candidate_commit != output.commit {
        bail!("candidate ancestry gate does not match the latest Writer output");
    }
    let (
        authorization,
        prior_recovery_path,
        recovery_mode,
        authorization_extension_id,
        authorization_resolution_digest,
    ) = if let Some(authorization) = candidate_ancestry_repair_authorization(&gate) {
        (
            authorization,
            None,
            RecoveryMode::ReconstructProtectedBase,
            None,
            None,
        )
    } else if let Some(authorization) =
        authorized_history_rewrite(project_root, attempt, output, &gate)?
    {
        let extension = attempt
            .round_cap_extensions
            .last()
            .expect("authorized rewrite requires a round-cap extension");
        (
            authorization,
            None,
            RecoveryMode::AcceptAuthorizedHistoryRewrite,
            Some(extension.id.clone()),
            Some(extension.human_resolution_digest.clone()),
        )
    } else {
        let prior = completed_repair(project_root, attempt)?
            .ok_or_else(|| anyhow::anyhow!("candidate gate is not eligible for ancestry repair"))?;
        let prior_path = recovery_artifact_path(&attempt.work_item_id, &attempt.id, &prior.task_id);
        let authorization = followup_repair_authorization(project_root, output, &gate, &prior)?
            .ok_or_else(|| anyhow::anyhow!("candidate gate is not eligible for ancestry repair"))?;
        (
            authorization,
            Some(prior_path),
            RecoveryMode::ReconstructProtectedBase,
            None,
            None,
        )
    };
    let next_round = attempt
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Write)
        .count()
        + 1;
    Ok(EligibleRecovery {
        work_item_id: attempt.work_item_id.clone(),
        attempt_id: attempt.id.clone(),
        source_task_id: source.id.clone(),
        workspace_path: output.workspace_path.clone(),
        source_branch: output.source_branch.clone(),
        source_candidate_commit: output.commit.clone(),
        source_gate_path: gate_ref.path.clone(),
        authorization,
        prior_recovery_path,
        task_id: format!("{}-write-{next_round}", attempt.id),
        recovery_mode,
        authorization_extension_id,
        authorization_resolution_digest,
    })
}

/// Recognize an explicitly approved Writer rewrite that changes only commit
/// messages while preserving the protected base and the complete linear suffix.
fn authorized_history_rewrite(
    project_root: &Path,
    attempt: &Attempt,
    output: &crate::work_model::TaskOutput,
    gate: &CandidateGateEvidence,
) -> Result<Option<CandidateAncestryRepairAuthorization>> {
    if gate.metadata_rewrite.is_some()
        || gate.candidate_commit != output.commit
        || output.base_commit.as_deref() != Some(gate.base_commit.as_str())
        || !only_candidate_identity_blocks(gate)
    {
        return Ok(None);
    }
    let Some(extension) = attempt.round_cap_extensions.last() else {
        return Ok(None);
    };
    extension
        .validate_human_resolution()
        .map_err(anyhow::Error::msg)?;
    if extension.human_resolution.is_empty() {
        return Ok(None);
    }
    for approved in &extension.failed_reviews {
        let path = crate::work_task_executor::resolve_managed_artifact_area_path(
            project_root,
            &approved.artifact.path,
        )?;
        let bytes = fs::read(path)?;
        if approved.digest != format!("sha256:{:x}", Sha256::digest(&bytes)) {
            bail!("authorized history rewrite review evidence digest changed");
        }
    }
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "authorized history rewrite workspace",
    )?;
    if !crate::git::run_raw(
        &workspace,
        &[
            "merge-base",
            "--is-ancestor",
            &extension.candidate_commit,
            &gate.base_commit,
        ],
    )?
    .status
    .success()
    {
        return Ok(None);
    }
    let protected_base = attempt
        .tasks
        .iter()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .and_then(|output| output.base_commit.clone())
        .ok_or_else(|| anyhow::anyhow!("authorized history rewrite has no protected base"))?;
    if !verify_equivalent_message_only_suffix(
        &workspace,
        &protected_base,
        &gate.base_commit,
        &gate.candidate_commit,
        &extension.human_resolution,
    )? {
        return Ok(None);
    }
    let candidate_tree = crate::git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", gate.candidate_commit)],
        "resolve authorized rewritten candidate tree",
    )?;
    Ok(Some(CandidateAncestryRepairAuthorization {
        protected_base_commit: protected_base,
        rejected_candidate_commit: gate.base_commit.clone(),
        candidate_commit: gate.candidate_commit.clone(),
        candidate_tree,
        prior_gate_path: String::new(),
    }))
}

fn only_candidate_identity_blocks(gate: &CandidateGateEvidence) -> bool {
    if gate.schema_version == 0
        || gate.schema_version > crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION
        || gate.phase != crate::candidate_gate::CandidateGatePhase::Writer
        || gate.disposition != crate::candidate_gate::CandidateGateDisposition::Blocked
    {
        return false;
    }
    let expected = format!(
        "base commit {} is not an ancestor of candidate {}",
        gate.base_commit, gate.candidate_commit
    );
    let blocking = gate
        .checks
        .iter()
        .filter(|check| {
            matches!(
                check.status,
                crate::candidate_gate::CandidateCheckStatus::Failed
                    | crate::candidate_gate::CandidateCheckStatus::Error
            )
        })
        .collect::<Vec<_>>();
    blocking.len() == 1
        && blocking[0].name == "candidate-identity"
        && blocking[0].status == crate::candidate_gate::CandidateCheckStatus::Error
        && blocking[0].diagnostics.as_slice() == [expected]
}

fn verify_equivalent_message_only_suffix(
    workspace: &Path,
    protected_base: &str,
    old_tip: &str,
    new_tip: &str,
    authorization: &str,
) -> Result<bool> {
    let old = linear_suffix(workspace, protected_base, old_tip)?;
    let new = linear_suffix(workspace, protected_base, new_tip)?;
    if old.len() != new.len() || old.is_empty() {
        return Ok(false);
    }
    let mut changed = 0usize;
    for (old_commit, new_commit) in old.iter().zip(&new) {
        let old_fields = commit_identity(workspace, old_commit)?;
        let new_fields = commit_identity(workspace, new_commit)?;
        if old_fields.len() != 5 || new_fields.len() != 5 || old_fields[..3] != new_fields[..3] {
            return Ok(false);
        }
        if old_fields[4] != new_fields[4] {
            changed += 1;
            if !authorization_names_commit(workspace, authorization, old_commit)? {
                return Ok(false);
            }
        }
    }
    Ok(changed > 0)
}

fn authorization_names_commit(workspace: &Path, authorization: &str, commit: &str) -> Result<bool> {
    for length in 7..=commit.len() {
        let prefix = &commit[..length];
        if !authorization.contains(prefix) {
            continue;
        }
        let resolved = crate::git::run_raw(
            workspace,
            &["rev-parse", "--verify", &format!("{prefix}^{{commit}}")],
        )?;
        if resolved.status.success() && String::from_utf8_lossy(&resolved.stdout).trim() == commit {
            return Ok(true);
        }
    }
    Ok(false)
}

fn linear_suffix(workspace: &Path, base: &str, tip: &str) -> Result<Vec<String>> {
    if !crate::git::run_raw(workspace, &["merge-base", "--is-ancestor", base, tip])?
        .status
        .success()
    {
        return Ok(Vec::new());
    }
    let raw = crate::git::run_stdout(
        workspace,
        &[
            "rev-list",
            "--reverse",
            "--first-parent",
            &format!("{base}..{tip}"),
        ],
        "enumerate candidate rewrite suffix",
    )?;
    let commits = raw.lines().map(str::to_string).collect::<Vec<_>>();
    let mut parent = base;
    for commit in &commits {
        let row = crate::git::run_stdout(
            workspace,
            &["rev-list", "--parents", "-n", "1", commit],
            "verify candidate rewrite linearity",
        )?;
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.as_slice() != [commit.as_str(), parent] {
            return Ok(Vec::new());
        }
        parent = commit;
    }
    Ok(commits)
}

fn commit_identity(workspace: &Path, commit: &str) -> Result<Vec<String>> {
    let raw = crate::git::run_stdout(
        workspace,
        &[
            "show",
            "-s",
            "--format=%T%x00%an%x00%ae%x00%aI%x00%B",
            commit,
        ],
        "read candidate rewrite commit identity",
    )?;
    Ok(raw.split('\0').map(str::to_string).collect())
}

fn followup_repair_authorization(
    project_root: &Path,
    output: &crate::work_model::TaskOutput,
    gate: &CandidateGateEvidence,
    prior: &CandidateAncestryRepair,
) -> Result<Option<CandidateAncestryRepairAuthorization>> {
    if gate.schema_version == 0
        || gate.schema_version > crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION
        || gate.phase != crate::candidate_gate::CandidateGatePhase::Writer
        || gate.disposition != crate::candidate_gate::CandidateGateDisposition::Blocked
        || gate.candidate_commit != output.commit
        || gate.metadata_rewrite.is_some()
    {
        return Ok(None);
    }
    let expected_checks = [
        "candidate-identity",
        "whitespace",
        "commit-metadata",
        "behavior-references",
        "tester-reference-evidence",
        "project-check",
    ];
    if gate.checks.len() != expected_checks.len()
        || expected_checks.iter().any(|name| {
            gate.checks
                .iter()
                .filter(|check| check.name == *name)
                .count()
                != 1
        })
    {
        return Ok(None);
    }
    let mut blocking = gate.checks.iter().filter(|check| {
        matches!(
            check.status,
            crate::candidate_gate::CandidateCheckStatus::Failed
                | crate::candidate_gate::CandidateCheckStatus::Error
        )
    });
    let Some(identity) = blocking.next() else {
        return Ok(None);
    };
    let expected = format!(
        "base commit {} is not an ancestor of candidate {}",
        gate.base_commit, gate.candidate_commit
    );
    if identity.name != "candidate-identity"
        || identity.status != crate::candidate_gate::CandidateCheckStatus::Error
        || identity.diagnostics.as_slice() != [expected]
        || blocking.next().is_some()
    {
        return Ok(None);
    }
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "candidate ancestry follow-up recovery workspace",
    )?;
    let base_tree = crate::git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", gate.base_commit)],
        "resolve follow-up recovery base tree",
    )?;
    let candidate_tree = crate::git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", gate.candidate_commit)],
        "resolve follow-up recovery candidate tree",
    )?;
    if base_tree != candidate_tree
        || !crate::git::run_raw(
            &workspace,
            &[
                "merge-base",
                "--is-ancestor",
                &prior.recovered_candidate_commit,
                &gate.base_commit,
            ],
        )?
        .status
        .success()
    {
        return Ok(None);
    }
    Ok(Some(CandidateAncestryRepairAuthorization {
        protected_base_commit: gate.base_commit.clone(),
        rejected_candidate_commit: gate.candidate_commit.clone(),
        candidate_commit: gate.candidate_commit.clone(),
        candidate_tree,
        prior_gate_path: String::new(),
    }))
}

fn verify_live_candidate(workspace: &Path, eligible: &EligibleRecovery) -> Result<()> {
    let head = crate::git::run_stdout(workspace, &["rev-parse", "HEAD"], "resolve candidate HEAD")?;
    if head != eligible.source_candidate_commit {
        bail!(
            "candidate HEAD {head} does not match blocked candidate {}",
            eligible.source_candidate_commit
        );
    }
    let branch = crate::git::run_stdout(
        workspace,
        &["symbolic-ref", "-q", "HEAD"],
        "resolve candidate branch",
    )?;
    let short_branch = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
    let expected_branch = eligible.source_branch.as_str();
    if short_branch != expected_branch && expected_branch != "main" {
        bail!(
            "candidate branch {short_branch:?} does not match recorded source branch {expected_branch:?}"
        );
    }
    Ok(())
}

fn verify_gate_provenance(project_root: &Path, eligible: &EligibleRecovery) -> Result<()> {
    if eligible.recovery_mode == RecoveryMode::AcceptAuthorizedHistoryRewrite {
        // Eligibility already bound the exact round-cap decision, its immutable
        // review bytes, and both commit suffixes while the recovery lock was held.
        return Ok(());
    }
    if let Some(prior_recovery_path) = eligible.prior_recovery_path.as_deref() {
        let prior = read_recovery_evidence(project_root, prior_recovery_path)?
            .ok_or_else(|| anyhow::anyhow!("prior candidate ancestry recovery is missing"))?;
        if prior.schema_version != RECOVERY_SCHEMA_VERSION
            || prior.recovered_candidate_commit == eligible.source_candidate_commit
            || !crate::git::run_raw(
                &resolve_managed_sibling_workspace_path(
                    project_root,
                    &eligible.workspace_path,
                    "candidate ancestry follow-up provenance workspace",
                )?,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &prior.recovered_candidate_commit,
                    &eligible.authorization.protected_base_commit,
                ],
            )?
            .status
            .success()
        {
            bail!("prior candidate ancestry recovery does not authorize this follow-up repair");
        }
        return Ok(());
    }
    let prior_path = crate::work_task_executor::resolve_managed_artifact_area_path(
        project_root,
        &eligible.authorization.prior_gate_path,
    )?;
    let prior = read_prior_writer_gate(&prior_path).map_err(anyhow::Error::msg)?;
    validate_prior_writer_gate(&prior, &eligible.authorization.rejected_candidate_commit)
        .map_err(anyhow::Error::msg)?;
    if prior.base_commit != eligible.authorization.protected_base_commit {
        bail!("prior metadata gate names a different protected base");
    }
    let failed = prior
        .checks
        .iter()
        .filter(|check| check.status == crate::candidate_gate::CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .collect::<Vec<_>>();
    if failed != ["commit-metadata"] {
        bail!("prior gate did not authorize a metadata-only correction");
    }
    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &eligible.workspace_path,
        "candidate ancestry recovery workspace",
    )?;
    let rejected_tree = crate::git::run_stdout(
        &workspace,
        &[
            "rev-parse",
            &format!(
                "{}^{{tree}}",
                eligible.authorization.rejected_candidate_commit
            ),
        ],
        "resolve rejected candidate tree",
    )?;
    if rejected_tree != eligible.authorization.candidate_tree {
        bail!("rejected and replacement candidate trees no longer match");
    }
    if !crate::git::run_raw(
        &workspace,
        &[
            "merge-base",
            "--is-ancestor",
            &eligible.authorization.protected_base_commit,
            &eligible.authorization.rejected_candidate_commit,
        ],
    )?
    .status
    .success()
    {
        bail!("rejected candidate does not descend from the protected base");
    }
    Ok(())
}

fn normalize_resolution(resolution: &str) -> Result<String> {
    let resolution = resolution.trim();
    if resolution.is_empty() {
        bail!("provide a non-empty --resolution for the candidate ancestry repair");
    }
    if resolution.len() > MAX_ROUND_CAP_RESOLUTION_BYTES {
        bail!(
            "candidate ancestry repair resolution is {} bytes; limit is {} bytes",
            resolution.len(),
            MAX_ROUND_CAP_RESOLUTION_BYTES
        );
    }
    Ok(resolution.to_string())
}

fn preserve_source_ref(workspace: &Path, reference: &str, source: &str) -> Result<()> {
    let existing = crate::git::run_raw(workspace, &["rev-parse", "--verify", reference])?;
    if existing.status.success() {
        let existing = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if existing != source {
            bail!(
                "candidate ancestry history ref {reference} points at {existing}, expected {source}"
            );
        }
        return Ok(());
    }
    crate::git::run(
        workspace,
        &["update-ref", reference, source],
        "preserve rejected candidate history",
    )
}

fn move_candidate_ref(workspace: &Path, source: &str, recovered: &str) -> Result<()> {
    let head = crate::git::run_stdout(workspace, &["rev-parse", "HEAD"], "resolve candidate HEAD")?;
    if head == recovered {
        return Ok(());
    }
    if head != source {
        bail!("candidate changed before ancestry repair could advance its branch");
    }
    let reference = crate::git::run_stdout(
        workspace,
        &["symbolic-ref", "-q", "HEAD"],
        "resolve candidate branch",
    )?;
    crate::git::run(
        workspace,
        &["update-ref", &reference, recovered, source],
        "advance candidate ancestry repair with compare-and-swap",
    )?;
    let status = crate::git::run_stdout(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
        "verify recovered candidate cleanliness",
    )?;
    if !status.is_empty() {
        bail!("candidate worktree became dirty after ancestry repair");
    }
    Ok(())
}

fn clone_completion_matrix(
    project_root: &Path,
    attempt: &Attempt,
    source_task_id: &str,
    target_task_id: &str,
) -> Result<()> {
    let Some(source) = attempt.tasks.iter().find(|task| task.id == source_task_id) else {
        bail!("candidate ancestry source Writer disappeared");
    };
    let Some(area) = source.artifact_area.as_ref() else {
        if attempt.writer_completion_contract.is_some() {
            bail!("candidate ancestry source Writer has no completion artifact area");
        }
        return Ok(());
    };
    let source_path = project_root
        .join(&area.path)
        .join(crate::work_task_executor::WRITER_COMPLETION_MATRIX_FILE);
    if !source_path.is_file() {
        if attempt.writer_completion_contract.is_some() {
            bail!("candidate ancestry source Writer has no completion matrix");
        }
        return Ok(());
    }
    let mut matrix: WriterCompletionMatrix = serde_json::from_slice(&fs::read(&source_path)?)
        .context("parse source Writer completion matrix")?;
    matrix.task_id = target_task_id.to_string();
    let target = project_root
        .join(work_artifact_path(
            &attempt.work_item_id,
            &attempt.id,
            target_task_id,
        ))
        .join(crate::work_task_executor::WRITER_COMPLETION_MATRIX_FILE);
    write_bytes_once(&target, &serde_json::to_vec_pretty(&matrix)?)
}

fn write_evidence_once(
    project_root: &Path,
    relative: &str,
    evidence: &CandidateAncestryRecoveryEvidence,
) -> Result<()> {
    write_bytes_once(
        &project_root.join(relative),
        &serde_json::to_vec_pretty(evidence)?,
    )
}

fn read_recovery_evidence(
    project_root: &Path,
    relative: &str,
) -> Result<Option<CandidateAncestryRecoveryEvidence>> {
    let path = project_root.join(relative);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > MAX_RECOVERY_EVIDENCE_BYTES {
        bail!(
            "candidate ancestry recovery evidence is {} bytes; limit is {} bytes",
            metadata.len(),
            MAX_RECOVERY_EVIDENCE_BYTES
        );
    }
    let evidence = serde_json::from_slice(&fs::read(&path)?).with_context(|| {
        format!(
            "parse candidate ancestry recovery evidence {}",
            path.display()
        )
    })?;
    Ok(Some(evidence))
}

fn validate_recovery_evidence(
    workspace: &Path,
    eligible: &EligibleRecovery,
    evidence: &CandidateAncestryRecoveryEvidence,
) -> Result<()> {
    let expected_ref = format!(
        "refs/fluent/recovery/{}/{}/candidate-ancestry-{}",
        eligible.work_item_id, eligible.attempt_id, eligible.source_candidate_commit
    );
    let expected_superseded_ref =
        (eligible.recovery_mode == RecoveryMode::AcceptAuthorizedHistoryRewrite).then(|| {
            format!(
                "refs/fluent/recovery/{}/{}/superseded-candidate-{}",
                eligible.work_item_id,
                eligible.attempt_id,
                eligible.authorization.rejected_candidate_commit
            )
        });
    if evidence.schema_version != RECOVERY_SCHEMA_VERSION
        || evidence.task_id != eligible.task_id
        || evidence.source_task_id != eligible.source_task_id
        || evidence.source_candidate_commit != eligible.source_candidate_commit
        || evidence.rejected_candidate_commit != eligible.authorization.rejected_candidate_commit
        || evidence.protected_base_commit != eligible.authorization.protected_base_commit
        || evidence.candidate_tree != eligible.authorization.candidate_tree
        || evidence.source_gate_path != eligible.source_gate_path
        || evidence.prior_gate_path != eligible.authorization.prior_gate_path
        || evidence.prior_recovery_path != eligible.prior_recovery_path
        || evidence.preserved_ref != expected_ref
        || evidence.resolution.trim().is_empty()
        || evidence.resolution_digest != round_cap_resolution_digest(&evidence.resolution)
        || evidence.recovery_mode != eligible.recovery_mode
        || evidence.superseded_candidate_ref != expected_superseded_ref
        || evidence.authorization_extension_id != eligible.authorization_extension_id
        || evidence.authorization_resolution_digest != eligible.authorization_resolution_digest
    {
        bail!("candidate ancestry recovery evidence does not match the blocked frontier");
    }
    let recovered_tree = crate::git::run_stdout(
        workspace,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", evidence.recovered_candidate_commit),
        ],
        "verify recovered candidate tree",
    )?;
    if recovered_tree != evidence.candidate_tree {
        bail!("recorded candidate ancestry recovery has a different tree");
    }
    match evidence.recovery_mode {
        RecoveryMode::ReconstructProtectedBase => {
            let ancestry = crate::git::run_stdout(
                workspace,
                &[
                    "rev-list",
                    "--parents",
                    "-n",
                    "1",
                    &evidence.recovered_candidate_commit,
                ],
                "verify recovered candidate parent",
            )?;
            let commits = ancestry.split_whitespace().collect::<Vec<_>>();
            if commits.as_slice()
                != [
                    evidence.recovered_candidate_commit.as_str(),
                    evidence.protected_base_commit.as_str(),
                ]
            {
                bail!(
                    "recorded candidate ancestry recovery does not have the protected base as its sole parent"
                );
            }
        }
        RecoveryMode::AcceptAuthorizedHistoryRewrite => {
            if evidence.recovered_candidate_commit != evidence.source_candidate_commit
                || linear_suffix(
                    workspace,
                    &evidence.protected_base_commit,
                    &evidence.recovered_candidate_commit,
                )?
                .is_empty()
            {
                bail!(
                    "recorded authorized history rewrite no longer preserves its protected linear ancestry"
                );
            }
        }
    }
    let head = crate::git::run_stdout(workspace, &["rev-parse", "HEAD"], "verify recovery HEAD")?;
    if head != evidence.source_candidate_commit && head != evidence.recovered_candidate_commit {
        bail!("candidate changed after candidate ancestry recovery was prepared");
    }
    Ok(())
}

fn write_bytes_once(path: &Path, bytes: &[u8]) -> Result<()> {
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
            crate::atomic_write::atomic_write(path, bytes)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn recovery_artifact_path(work_item_id: &str, attempt_id: &str, task_id: &str) -> String {
    work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("candidate-ancestry-repairs/{task_id}/{RECOVERY_FILE}"),
    )
}

fn completed_repair(
    project_root: &Path,
    attempt: &Attempt,
) -> Result<Option<CandidateAncestryRepair>> {
    let Some(task) = attempt.tasks.iter().rev().find(|task| {
        task.kind == TaskKind::Write
            && task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE
            && task.status == TaskStatus::Complete
    }) else {
        return Ok(None);
    };
    let output = task
        .output
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("completed candidate ancestry repair has no output"))?;
    let artifact = task
        .input_artifacts
        .iter()
        .find(|artifact| artifact.producer_id == RECOVERY_PRODUCER)
        .ok_or_else(|| anyhow::anyhow!("candidate ancestry repair has no recovery artifact"))?;
    let evidence: CandidateAncestryRecoveryEvidence =
        serde_json::from_slice(&fs::read(project_root.join(&artifact.path))?)
            .context("parse completed candidate ancestry recovery evidence")?;
    if evidence.schema_version != RECOVERY_SCHEMA_VERSION
        || evidence.task_id != task.id
        || evidence.recovered_candidate_commit != output.commit
        || Some(evidence.protected_base_commit.as_str()) != output.base_commit.as_deref()
    {
        bail!("completed candidate ancestry recovery evidence does not match its Task");
    }
    Ok(Some(CandidateAncestryRepair {
        task_id: task.id.clone(),
        source_candidate_commit: evidence.source_candidate_commit,
        recovered_candidate_commit: output.commit.clone(),
        protected_base_commit: evidence.protected_base_commit,
        already_repaired: true,
    }))
}

fn ensure_registered_worktree(project_root: &Path, workspace: &Path) -> Result<()> {
    let expected = fs::canonicalize(workspace)?;
    let output = crate::git::run_raw(project_root, &["worktree", "list", "--porcelain"])?;
    if output.status.success()
        && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.strip_prefix("worktree ")
                .is_some_and(|path| fs::canonicalize(path).is_ok_and(|actual| actual == expected))
        })
    {
        return Ok(());
    }
    bail!(
        "workspace {} is not a registered Git worktree",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate_gate::{
        CandidateGateDisposition, CandidateGatePhase, CandidateGateRequest, run_candidate_gate,
        run_candidate_gate_with_prior,
    };
    use crate::work_model::{TaskOutput, WorkItem};
    use std::path::PathBuf;

    struct Fixture {
        _outer: tempfile::TempDir,
        project: PathBuf,
        workspace: PathBuf,
        store: WorkModelStore,
        protected: String,
        replacement: String,
        tree: String,
    }

    impl Fixture {
        fn new() -> Self {
            let outer = tempfile::TempDir::new().unwrap();
            let project = outer.path().join("main");
            let workspace = outer.path().join("work-1-attempt-1");
            fs::create_dir_all(&project).unwrap();
            crate::git::run(&project, &["init", "-q"], "initialize project").unwrap();
            for (key, value) in [
                ("user.name", "Test Author"),
                ("user.email", "test@example.com"),
            ] {
                crate::git::run(&project, &["config", key, value], "configure project").unwrap();
            }
            fs::write(project.join("README.md"), "base\n").unwrap();
            crate::git::run(&project, &["add", "README.md"], "stage base").unwrap();
            crate::git::run(
                &project,
                &[
                    "commit",
                    "-q",
                    "-m",
                    "Create base",
                    "-m",
                    "Create the recovery fixture.",
                ],
                "commit base",
            )
            .unwrap();
            let base =
                crate::git::run_stdout(&project, &["rev-parse", "HEAD"], "read base").unwrap();
            crate::git::run(
                &project,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "work/attempt-1",
                    workspace.to_str().unwrap(),
                    "HEAD",
                ],
                "create candidate worktree",
            )
            .unwrap();
            fs::write(workspace.join("candidate.txt"), "protected\n").unwrap();
            crate::git::run(
                &workspace,
                &["add", "candidate.txt"],
                "stage protected candidate",
            )
            .unwrap();
            crate::git::run(
                &workspace,
                &[
                    "commit",
                    "-q",
                    "-m",
                    "Add protected candidate",
                    "-m",
                    "Establish the protected candidate base.",
                ],
                "commit protected candidate",
            )
            .unwrap();
            let protected = crate::git::run_stdout(
                &workspace,
                &["rev-parse", "HEAD"],
                "read protected candidate",
            )
            .unwrap();
            fs::write(workspace.join("candidate.txt"), "corrected\n").unwrap();
            crate::git::run(&workspace, &["add", "candidate.txt"], "stage correction").unwrap();
            crate::git::run(
                &workspace,
                &[
                    "commit",
                    "-q",
                    "-m",
                    "Correct candidate",
                    "-m",
                    "Use literal \\n metadata that the candidate gate rejects.",
                ],
                "commit malformed correction",
            )
            .unwrap();
            let rejected = crate::git::run_stdout(
                &workspace,
                &["rev-parse", "HEAD"],
                "read rejected candidate",
            )
            .unwrap();
            let prior_gate_relative = work_artifact_path(
                "work-1",
                "attempt-1",
                "candidate-gates/attempt-1-write-2/candidate-gate.json",
            );
            let prior_gate_path = project.join(&prior_gate_relative);
            let prior = run_candidate_gate(CandidateGateRequest {
                project_root: &project,
                workspace: &workspace,
                work_item_id: "work-1",
                attempt_id: "attempt-1",
                task_id: Some("attempt-1-write-2"),
                phase: CandidateGatePhase::Writer,
                base_commit: &protected,
                candidate_commit: &rejected,
                completion_contract: None,
                allow_tree_identical_metadata_rewrite: false,
                tester_results_path: None,
                artifact_path: &prior_gate_path,
            })
            .unwrap();
            assert_eq!(prior.disposition, CandidateGateDisposition::Rejected);

            let protected_tree = crate::git::run_stdout(
                &workspace,
                &["rev-parse", &format!("{protected}^{{tree}}")],
                "read protected tree",
            )
            .unwrap();
            let rewritten_protected = crate::git::run_stdout(
                &workspace,
                &[
                    "commit-tree",
                    &protected_tree,
                    "-p",
                    &base,
                    "-m",
                    "Add protected candidate",
                    "-m",
                    "Rewrite metadata while accidentally replacing the protected base.",
                ],
                "rewrite protected base",
            )
            .unwrap();
            let tree = crate::git::run_stdout(
                &workspace,
                &["rev-parse", &format!("{rejected}^{{tree}}")],
                "read rejected tree",
            )
            .unwrap();
            let replacement = crate::git::run_stdout(
                &workspace,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &rewritten_protected,
                    "-m",
                    "Correct candidate",
                    "-m",
                    "Repair the correction metadata without changing its tree.",
                ],
                "create replacement candidate",
            )
            .unwrap();
            crate::git::run(
                &workspace,
                &["update-ref", "HEAD", &replacement, &rejected],
                "install replacement candidate",
            )
            .unwrap();
            let blocked_gate_relative = work_artifact_path(
                "work-1",
                "attempt-1",
                "candidate-gates/attempt-1-write-3/candidate-gate.json",
            );
            let blocked_gate_path = project.join(&blocked_gate_relative);
            let blocked = run_candidate_gate_with_prior(
                CandidateGateRequest {
                    project_root: &project,
                    workspace: &workspace,
                    work_item_id: "work-1",
                    attempt_id: "attempt-1",
                    task_id: Some("attempt-1-write-3"),
                    phase: CandidateGatePhase::Writer,
                    base_commit: &rejected,
                    candidate_commit: &replacement,
                    completion_contract: None,
                    allow_tree_identical_metadata_rewrite: false,
                    tester_results_path: None,
                    artifact_path: &blocked_gate_path,
                },
                Some(&prior_gate_path),
            )
            .unwrap();
            assert_eq!(blocked.disposition, CandidateGateDisposition::Blocked);
            assert!(candidate_ancestry_repair_authorization(&blocked).is_some());

            let relative_workspace =
                format!("../{}", workspace.file_name().unwrap().to_string_lossy());
            let mut item = WorkItem::planned("work-1", "Recover candidate ancestry");
            item.add_initial_attempt("attempt-1").unwrap();
            let initial = &mut item.attempts[0].tasks[0];
            initial.status = TaskStatus::Complete;
            initial.output = Some(TaskOutput {
                workspace_id: "candidate".to_string(),
                workspace_path: relative_workspace.clone(),
                source_branch: "work/attempt-1".to_string(),
                base_commit: Some(base.clone()),
                commit: protected.clone(),
                no_change: None,
                learner_canonicalization: None,
            });
            let write_2 = item.add_next_write_round("attempt-1", Vec::new()).unwrap();
            let task = item.attempts[0]
                .tasks
                .iter_mut()
                .find(|task| task.id == write_2)
                .unwrap();
            task.status = TaskStatus::Complete;
            task.output = Some(TaskOutput {
                workspace_id: "candidate".to_string(),
                workspace_path: relative_workspace.clone(),
                source_branch: "work/attempt-1".to_string(),
                base_commit: Some(protected.clone()),
                commit: rejected.clone(),
                no_change: None,
                learner_canonicalization: None,
            });
            let prior_ref = ArtifactRef {
                producer_id: write_2,
                path: prior_gate_relative,
            };
            let write_3 = item
                .add_next_write_round("attempt-1", vec![prior_ref.clone()])
                .unwrap();
            let task = item.attempts[0]
                .tasks
                .iter_mut()
                .find(|task| task.id == write_3)
                .unwrap();
            task.status = TaskStatus::Complete;
            task.output = Some(TaskOutput {
                workspace_id: "candidate".to_string(),
                workspace_path: relative_workspace,
                source_branch: "work/attempt-1".to_string(),
                base_commit: Some(rejected.clone()),
                commit: replacement.clone(),
                no_change: None,
                learner_canonicalization: None,
            });
            let blocked_ref = ArtifactRef {
                producer_id: write_3,
                path: blocked_gate_relative,
            };
            item.attempts[0].artifacts.extend([prior_ref, blocked_ref]);
            item.attempts[0].status = AttemptStatus::NeedsUser;
            item.attempts[0].pause_kind = Some(PauseKind::CandidateGate);
            item.attempts[0].review_state = Some(AttemptReviewState::NotReviewed);
            let store = WorkModelStore::new(&project);
            store.create_work_item(&item).unwrap();
            Self {
                _outer: outer,
                project,
                workspace,
                store,
                protected,
                replacement,
                tree,
            }
        }
    }

    #[test]
    fn repairs_exact_tree_on_protected_base_and_is_idempotent() {
        let fixture = Fixture::new();
        let before = fixture.store.read_work_item("work-1").unwrap();
        assert_eq!(
            recovery_action(&fixture.project, &before.attempts[0]).as_deref(),
            Some(
                "fluent attempt repair-candidate-ancestry work-1 attempt-1 --resolution \"<describe the approved ancestry repair>\""
            )
        );
        assert_eq!(
            scheduler_action("work-1", "attempt-1"),
            "fluent queue resume work-1 --attempt attempt-1"
        );
        let repair_result = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Preserve the accepted protected base and unchanged final tree.",
        )
        .unwrap();
        assert!(!repair_result.already_repaired);
        assert_eq!(repair_result.source_candidate_commit, fixture.replacement);
        assert_eq!(repair_result.protected_base_commit, fixture.protected);
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", repair_result.recovered_candidate_commit),
                ],
                "read repaired tree",
            )
            .unwrap(),
            fixture.tree
        );
        assert!(
            crate::git::run_raw(
                &fixture.workspace,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &fixture.protected,
                    &repair_result.recovered_candidate_commit,
                ],
            )
            .unwrap()
            .status
            .success()
        );
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &[
                    "show",
                    "-s",
                    "--format=%B",
                    &repair_result.recovered_candidate_commit
                ],
                "read recovered candidate message",
            )
            .unwrap(),
            crate::git::run_stdout(
                &fixture.workspace,
                &["show", "-s", "--format=%B", &fixture.replacement],
                "read source candidate message",
            )
            .unwrap()
        );
        let stored = fixture.store.read_work_item("work-1").unwrap();
        let recovered_task = stored.attempts[0].tasks.last().unwrap();
        assert_eq!(recovered_task.role, CANDIDATE_ANCESTRY_REPAIR_ROLE);
        assert_eq!(recovered_task.status, TaskStatus::Complete);
        assert_eq!(stored.attempts[0].status, AttemptStatus::Planned);
        assert!(stored.attempts[0].artifacts.iter().any(|artifact| {
            artifact.producer_id == RECOVERY_PRODUCER && artifact.path.ends_with("/recovery.json")
        }));
        let preserved = format!(
            "refs/fluent/recovery/work-1/attempt-1/candidate-ancestry-{}",
            fixture.replacement
        );
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &["rev-parse", &preserved],
                "read preserved candidate",
            )
            .unwrap(),
            fixture.replacement
        );

        let gate_path = fixture.project.join("recovered-gate.json");
        let gate = run_candidate_gate(CandidateGateRequest {
            project_root: &fixture.project,
            workspace: &fixture.workspace,
            work_item_id: "work-1",
            attempt_id: "attempt-1",
            task_id: Some(&repair_result.task_id),
            phase: CandidateGatePhase::Writer,
            base_commit: &fixture.protected,
            candidate_commit: &repair_result.recovered_candidate_commit,
            completion_contract: None,
            allow_tree_identical_metadata_rewrite: false,
            tester_results_path: None,
            artifact_path: &gate_path,
        })
        .unwrap();
        assert_eq!(gate.disposition, CandidateGateDisposition::Passed);

        let outcome = crate::work_attempt_loop::plan_after_completed_writer(
            &fixture.project,
            &fixture.store,
            fixture.store.read_work_item("work-1").unwrap(),
            "attempt-1",
        )
        .unwrap();
        assert!(matches!(
            outcome,
            crate::work_attempt_loop::WorkAttemptRunOutcome::PlannedReviews { .. }
        ));
        let resumed = fixture.store.read_work_item("work-1").unwrap();
        let reviewers = resumed.attempts[0]
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Review)
            .collect::<Vec<_>>();
        assert!(!reviewers.is_empty());
        assert!(reviewers.iter().all(|task| {
            task.review_context.as_ref().is_some_and(|context| {
                context.candidate_commit == repair_result.recovered_candidate_commit
            })
        }));
        assert!(reviewers.iter().all(|task| {
            task.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == RECOVERY_PRODUCER
                    && artifact.path.ends_with("/recovery.json")
            }) && !task.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == repair_result.task_id
                    && artifact.path.ends_with("/transcript.jsonl")
            })
        }));

        let repeated = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "A repeated invocation must not create another repair.",
        )
        .unwrap();
        assert!(repeated.already_repaired);
        assert_eq!(
            repeated.recovered_candidate_commit,
            repair_result.recovered_candidate_commit
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .iter()
                .filter(|task| task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE)
                .count(),
            1
        );
    }

    #[test]
    fn supersedes_interrupted_fresh_queue_admission_and_resumes_exact_repair() {
        let fixture = Fixture::new();
        let repair_result = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Preserve the protected base and resume this exact Attempt.",
        )
        .unwrap();
        crate::work_attempt_loop::plan_after_completed_writer(
            &fixture.project,
            &fixture.store,
            fixture.store.read_work_item("work-1").unwrap(),
            "attempt-1",
        )
        .unwrap();

        crate::queue::add(&fixture.project, "work-1", None).unwrap();
        crate::queue::claim(&fixture.project, "work-1", "attempt-2")
            .unwrap()
            .unwrap();
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                item.add_initial_attempt("attempt-2")?;
                let accidental = item.attempts.last_mut().unwrap();
                crate::work_model::set_task_terminal(
                    &mut accidental.tasks[0],
                    TaskStatus::NeedsUser,
                );
                crate::work_model::suspend_attempt(accidental, PauseKind::Interrupted);
                Ok(())
            })
            .unwrap();
        crate::queue::remove(&fixture.project, "work-1").unwrap();

        let repeated = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Reconcile the accidental queue admission without changing the candidate.",
        )
        .unwrap();
        assert!(repeated.already_repaired);
        assert_eq!(repeated.task_id, repair_result.task_id);
        let reconciled = fixture.store.read_work_item("work-1").unwrap();
        assert!(is_resume_ready(&fixture.project, &reconciled, "attempt-1").unwrap());
        let accidental = &reconciled.attempts[1];
        assert_eq!(accidental.status, AttemptStatus::Failed);
        assert_eq!(accidental.tasks[0].status, TaskStatus::Superseded);
        assert_eq!(accidental.tasks[0].role, SUPERSEDED_ATTEMPT_ROLE);

        assert_eq!(
            crate::queue::resume(&fixture.project, "work-1", "attempt-1", None).unwrap(),
            crate::queue::ResumeDispatchOutcome::Created
        );
        let dispatch = crate::queue::show(&fixture.project, "work-1").unwrap();
        assert_eq!(dispatch.bound_attempt_id.as_deref(), Some("attempt-1"));
        assert_eq!(dispatch.intent, crate::queue::DispatchIntent::ResumeAttempt);

        let again = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "A repeated recovery remains idempotent.",
        )
        .unwrap();
        assert!(again.already_repaired);
        assert_eq!(
            fixture
                .store
                .read_work_item("work-1")
                .unwrap()
                .attempts
                .len(),
            2
        );
    }

    #[test]
    fn repairs_a_later_tree_identical_rewrite_after_an_earlier_recovery() {
        let fixture = Fixture::new();
        let first = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Recover the first authorized ancestry failure.",
        )
        .unwrap();
        crate::work_attempt_loop::plan_after_completed_writer(
            &fixture.project,
            &fixture.store,
            fixture.store.read_work_item("work-1").unwrap(),
            "attempt-1",
        )
        .unwrap();
        crate::queue::add(&fixture.project, "work-1", None).unwrap();
        crate::queue::claim(&fixture.project, "work-1", "attempt-2")
            .unwrap()
            .unwrap();
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                item.add_initial_attempt("attempt-2")?;
                let accidental = item.attempts.last_mut().unwrap();
                crate::work_model::set_task_terminal(
                    &mut accidental.tasks[0],
                    TaskStatus::NeedsUser,
                );
                crate::work_model::suspend_attempt(accidental, PauseKind::Interrupted);
                Ok(())
            })
            .unwrap();
        crate::queue::remove(&fixture.project, "work-1").unwrap();
        repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Supersede the accidental admission before later corrections.",
        )
        .unwrap();
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                let accidental = &mut item.attempts[1];
                let task = &mut accidental.tasks[0];
                let workspace = task.workspace_access.writes[0].clone();
                task.status = TaskStatus::Complete;
                task.output = Some(TaskOutput {
                    workspace_id: workspace.id,
                    workspace_path: workspace.path,
                    source_branch: "work/attempt-2".to_string(),
                    base_commit: Some(fixture.protected.clone()),
                    commit: first.recovered_candidate_commit.clone(),
                    no_change: None,
                    learner_canonicalization: None,
                });
                accidental.artifacts.push(ArtifactRef {
                    producer_id: task.id.clone(),
                    path: first.recovered_candidate_commit.clone(),
                });
                Ok(())
            })
            .unwrap();
        fs::write(fixture.workspace.join("followup.txt"), "corrected\n").unwrap();
        crate::git::run(
            &fixture.workspace,
            &["add", "followup.txt"],
            "stage substantive follow-up",
        )
        .unwrap();
        crate::git::run(
            &fixture.workspace,
            &[
                "commit",
                "-q",
                "-m",
                "Improve lifecycle recovery",
                "-m",
                "Preserve the running revision until unloading begins.",
            ],
            "commit substantive follow-up",
        )
        .unwrap();
        let substantive = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "read substantive follow-up",
        )
        .unwrap();
        let substantive_tree = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD^{tree}"],
            "read substantive follow-up tree",
        )
        .unwrap();
        let rewritten_message =
            "Improve lifecycle recovery\n\nExplain the operator-visible failure boundary.";
        let rewritten = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &substantive_tree,
                "-p",
                &first.recovered_candidate_commit,
                "-m",
                rewritten_message,
            ],
            "rewrite follow-up metadata",
        )
        .unwrap();
        crate::git::run(
            &fixture.workspace,
            &["update-ref", "HEAD", &rewritten, &substantive],
            "install rewritten follow-up",
        )
        .unwrap();

        fixture
            .store
            .mutate_work_item("work-1", |item| {
                let substantive_task = item.add_next_write_round("attempt-1", Vec::new())?;
                let task = item.attempts[0]
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == substantive_task)
                    .unwrap();
                task.status = TaskStatus::Complete;
                task.output = Some(TaskOutput {
                    workspace_id: "candidate".to_string(),
                    workspace_path: format!(
                        "../{}",
                        fixture.workspace.file_name().unwrap().to_string_lossy()
                    ),
                    source_branch: "work/attempt-1".to_string(),
                    base_commit: Some(first.recovered_candidate_commit.clone()),
                    commit: substantive.clone(),
                    no_change: None,
                    learner_canonicalization: None,
                });
                let rewrite_task = item.add_next_write_round("attempt-1", Vec::new())?;
                let task = item.attempts[0]
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == rewrite_task)
                    .unwrap();
                task.status = TaskStatus::Complete;
                task.output = Some(TaskOutput {
                    workspace_id: "candidate".to_string(),
                    workspace_path: format!(
                        "../{}",
                        fixture.workspace.file_name().unwrap().to_string_lossy()
                    ),
                    source_branch: "work/attempt-1".to_string(),
                    base_commit: Some(substantive.clone()),
                    commit: rewritten.clone(),
                    no_change: None,
                    learner_canonicalization: None,
                });
                Ok(())
            })
            .unwrap();
        let before_gate = fixture.store.read_work_item("work-1").unwrap();
        let source_task = before_gate.attempts[0].tasks.last().unwrap().id.clone();
        clone_completion_matrix(
            &fixture.project,
            &before_gate.attempts[0],
            &first.task_id,
            &source_task,
        )
        .unwrap();
        let gate_relative = work_artifact_path(
            "work-1",
            "attempt-1",
            &format!("candidate-gates/{source_task}/candidate-gate.json"),
        );
        let gate = run_candidate_gate(CandidateGateRequest {
            project_root: &fixture.project,
            workspace: &fixture.workspace,
            work_item_id: "work-1",
            attempt_id: "attempt-1",
            task_id: Some(&source_task),
            phase: CandidateGatePhase::Writer,
            base_commit: &substantive,
            candidate_commit: &rewritten,
            completion_contract: None,
            allow_tree_identical_metadata_rewrite: false,
            tester_results_path: None,
            artifact_path: &fixture.project.join(&gate_relative),
        })
        .unwrap();
        assert_eq!(gate.disposition, CandidateGateDisposition::Blocked);
        assert!(candidate_ancestry_repair_authorization(&gate).is_none());
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                attempt.artifacts.push(ArtifactRef {
                    producer_id: source_task.clone(),
                    path: gate_relative.clone(),
                });
                attempt.status = AttemptStatus::NeedsUser;
                attempt.pause_kind = Some(PauseKind::CandidateGate);
                attempt.review_state = Some(AttemptReviewState::NotReviewed);
                Ok(())
            })
            .unwrap();

        let second = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Recover the later tree-identical rewrite without discarding its substantive base.",
        )
        .unwrap();

        assert_ne!(second.task_id, first.task_id);
        assert_eq!(second.protected_base_commit, substantive);
        let parents = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "rev-list",
                "--parents",
                "-n",
                "1",
                &second.recovered_candidate_commit,
            ],
            "read second recovery parent",
        )
        .unwrap();
        assert_eq!(
            parents.split_whitespace().collect::<Vec<_>>(),
            vec![
                second.recovered_candidate_commit.as_str(),
                substantive.as_str()
            ]
        );
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &[
                    "show",
                    "-s",
                    "--format=%B",
                    &second.recovered_candidate_commit
                ],
                "read second recovery message",
            )
            .unwrap(),
            rewritten_message
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .iter()
                .filter(|task| task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE)
                .count(),
            2
        );
        let repeated = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Repeat the later recovery without revisiting prior superseded admissions.",
        )
        .unwrap();
        assert!(repeated.already_repaired);
        assert_eq!(repeated.task_id, second.task_id);
        assert!(
            is_resume_ready(
                &fixture.project,
                &fixture.store.read_work_item("work-1").unwrap(),
                "attempt-1",
            )
            .unwrap()
        );
    }

    #[test]
    fn rejects_dirty_candidate_without_mutating_history_or_state() {
        let fixture = Fixture::new();
        fs::write(fixture.workspace.join("candidate.txt"), "dirty\n").unwrap();
        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Repair the exact verified candidate.",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a clean candidate worktree")
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0].status,
            AttemptStatus::NeedsUser
        );
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "read unchanged candidate",
            )
            .unwrap(),
            fixture.replacement
        );
    }

    #[test]
    fn rejects_clean_candidate_head_drift_without_mutating_attempt_state() {
        let fixture = Fixture::new();
        crate::git::run(
            &fixture.workspace,
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "Advance unrelated candidate",
                "-m",
                "Create a clean candidate drift boundary.",
            ],
            "advance candidate outside Fluent",
        )
        .unwrap();
        let drifted = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", "HEAD"],
            "read drifted candidate",
        )
        .unwrap();

        let error = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Repair only the candidate recorded by the blocked gate.",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match blocked candidate")
        );
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0].status,
            AttemptStatus::NeedsUser
        );
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &["rev-parse", "HEAD"],
                "read preserved drifted candidate",
            )
            .unwrap(),
            drifted
        );
    }

    #[test]
    fn reconciles_interrupted_repair_after_branch_move() {
        let fixture = Fixture::new();
        let item = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &item.attempts[0];
        let eligible = inspect_eligible(&fixture.project, attempt).unwrap();
        verify_gate_provenance(&fixture.project, &eligible).unwrap();
        verify_live_candidate(&fixture.workspace, &eligible).unwrap();
        let recovered = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &fixture.tree,
                "-p",
                &fixture.protected,
                "-m",
                "Recover candidate ancestry",
                "-m",
                "Preserve the verified candidate tree and protected base.",
            ],
            "prepare interrupted ancestry repair",
        )
        .unwrap();
        let preserved_ref = format!(
            "refs/fluent/recovery/work-1/attempt-1/candidate-ancestry-{}",
            fixture.replacement
        );
        preserve_source_ref(&fixture.workspace, &preserved_ref, &fixture.replacement).unwrap();
        let resolution = "Preserve the verified candidate after an interrupted repair.";
        let evidence = CandidateAncestryRecoveryEvidence {
            schema_version: RECOVERY_SCHEMA_VERSION,
            task_id: eligible.task_id.clone(),
            source_task_id: eligible.source_task_id.clone(),
            source_candidate_commit: eligible.source_candidate_commit.clone(),
            rejected_candidate_commit: eligible.authorization.rejected_candidate_commit.clone(),
            protected_base_commit: eligible.authorization.protected_base_commit.clone(),
            recovered_candidate_commit: recovered.clone(),
            candidate_tree: fixture.tree.clone(),
            source_gate_path: eligible.source_gate_path.clone(),
            prior_gate_path: eligible.authorization.prior_gate_path.clone(),
            prior_recovery_path: eligible.prior_recovery_path.clone(),
            preserved_ref,
            resolution: resolution.to_string(),
            resolution_digest: round_cap_resolution_digest(resolution),
            recovery_mode: RecoveryMode::ReconstructProtectedBase,
            superseded_candidate_ref: None,
            authorization_extension_id: None,
            authorization_resolution_digest: None,
        };
        let recovery_path = recovery_artifact_path("work-1", "attempt-1", &eligible.task_id);
        validate_recovery_evidence(&fixture.workspace, &eligible, &evidence).unwrap();
        write_evidence_once(&fixture.project, &recovery_path, &evidence).unwrap();
        move_candidate_ref(&fixture.workspace, &fixture.replacement, &recovered).unwrap();

        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0].status,
            AttemptStatus::NeedsUser
        );
        let repair_result = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "A retry reconciles the durable recovery intent.",
        )
        .unwrap();
        assert_eq!(repair_result.recovered_candidate_commit, recovered);
        assert!(!repair_result.already_repaired);
        let repaired = fixture.store.read_work_item("work-1").unwrap();
        assert_eq!(repaired.attempts[0].status, AttemptStatus::Planned);
        assert_eq!(
            repaired.attempts[0]
                .tasks
                .iter()
                .filter(|task| task.role == CANDIDATE_ANCESTRY_REPAIR_ROLE)
                .count(),
            1
        );
    }

    #[test]
    fn repeated_repair_reconciles_planned_review_inputs_without_fake_transcript() {
        let fixture = Fixture::new();
        let repair_result = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Preserve the accepted protected base and unchanged final tree.",
        )
        .unwrap();
        crate::work_attempt_loop::plan_after_completed_writer(
            &fixture.project,
            &fixture.store,
            fixture.store.read_work_item("work-1").unwrap(),
            "attempt-1",
        )
        .unwrap();
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                for task in item.attempts[0]
                    .tasks
                    .iter_mut()
                    .filter(|task| task.kind == TaskKind::Review)
                {
                    task.input_artifacts
                        .retain(|artifact| artifact.producer_id != RECOVERY_PRODUCER);
                    task.input_artifacts.push(ArtifactRef {
                        producer_id: repair_result.task_id.clone(),
                        path: format!(
                            ".fluent/work/artifacts/work-1/attempt-1/{}/transcript.jsonl",
                            repair_result.task_id
                        ),
                    });
                }
                Ok(())
            })
            .unwrap();

        let repeated = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "A repeated invocation reconciles its planned reviewers.",
        )
        .unwrap();
        assert!(repeated.already_repaired);
        let stored = fixture.store.read_work_item("work-1").unwrap();
        for reviewer in stored.attempts[0]
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Review)
        {
            assert!(reviewer.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == RECOVERY_PRODUCER
                    && artifact.path.ends_with("/recovery.json")
            }));
            assert!(!reviewer.input_artifacts.iter().any(|artifact| {
                artifact.producer_id == repair_result.task_id
                    && artifact.path.ends_with("/transcript.jsonl")
            }));
        }
    }

    #[test]
    fn accepts_audited_message_only_suffix_rewrite_without_flattening_history() {
        let fixture = Fixture::new();
        let stored = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &stored.attempts[0];
        let protected_base = attempt.tasks[0]
            .output
            .as_ref()
            .unwrap()
            .base_commit
            .clone()
            .unwrap();
        let old_tip = attempt.tasks[1].output.as_ref().unwrap().commit.clone();
        let old_parent = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", &format!("{old_tip}^")],
            "read authorized rewrite parent",
        )
        .unwrap();
        let rewritten = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &fixture.tree,
                "-p",
                &old_parent,
                "-m",
                "Correct candidate terminology",
                "-m",
                "Use project-approved wording without changing content.",
            ],
            "create authorized message-only rewrite",
        )
        .unwrap();
        crate::git::run(
            &fixture.workspace,
            &["update-ref", "HEAD", &rewritten],
            "install authorized history rewrite",
        )
        .unwrap();

        let review_relative = work_artifact_path(
            "work-1",
            "attempt-1",
            "attempt-1-review-1-documentation/review.md",
        );
        let review_bytes = format!("Rewrite commit {old_tip} message only.\n").into_bytes();
        fs::create_dir_all(fixture.project.join(&review_relative).parent().unwrap()).unwrap();
        fs::write(fixture.project.join(&review_relative), &review_bytes).unwrap();
        let gate_relative = work_artifact_path(
            "work-1",
            "attempt-1",
            "candidate-gates/attempt-1-write-3/candidate-gate.json",
        );
        let gate = run_candidate_gate(CandidateGateRequest {
            project_root: &fixture.project,
            workspace: &fixture.workspace,
            work_item_id: "work-1",
            attempt_id: "attempt-1",
            task_id: Some("attempt-1-write-3"),
            phase: CandidateGatePhase::Writer,
            base_commit: &old_tip,
            candidate_commit: &rewritten,
            completion_contract: None,
            allow_tree_identical_metadata_rewrite: false,
            tester_results_path: None,
            artifact_path: &fixture.project.join(&gate_relative),
        })
        .unwrap();
        assert!(only_candidate_identity_blocks(&gate));

        let resolution = format!(
            "Through Fluent's audited Writer workflow, rewrite only commit {} while preserving authorship, tree content, and linear history.",
            &old_tip[..7]
        );
        fixture
            .store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                let source = attempt.tasks.last_mut().unwrap();
                source.output.as_mut().unwrap().base_commit = Some(old_tip.clone());
                source.output.as_mut().unwrap().commit = rewritten.clone();
                attempt.artifacts.clear();
                attempt.artifacts.push(ArtifactRef {
                    producer_id: source.id.clone(),
                    path: gate_relative.clone(),
                });
                attempt
                    .round_cap_extensions
                    .push(crate::work_model::RoundCapExtension {
                        id: "round-cap-3-4".to_string(),
                        old_cap: 3,
                        new_cap: 4,
                        additional_write_rounds: 1,
                        evidence_write_rounds: 0,
                        candidate_commit: old_parent.clone(),
                        failed_reviews: vec![crate::work_model::RoundCapReviewArtifact {
                            artifact: ArtifactRef {
                                producer_id: "attempt-1-review-1-documentation".to_string(),
                                path: review_relative.clone(),
                            },
                            digest: format!("sha256:{:x}", Sha256::digest(&review_bytes)),
                        }],
                        human_resolution: resolution.clone(),
                        human_resolution_digest: round_cap_resolution_digest(&resolution),
                        approved_at: now_iso8601(),
                    });
                Ok(())
            })
            .unwrap();

        let result = repair(
            &fixture.project,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Accept the exact audited message-only rewrite.",
        )
        .unwrap();
        assert_eq!(result.source_candidate_commit, rewritten);
        assert_eq!(result.recovered_candidate_commit, rewritten);
        assert_eq!(result.protected_base_commit, protected_base);
        let recovered = fixture.store.read_work_item("work-1").unwrap();
        assert_eq!(recovered.attempts[0].status, AttemptStatus::Planned);
        let evidence = read_recovery_evidence(
            &fixture.project,
            &recovery_artifact_path("work-1", "attempt-1", &result.task_id),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            evidence.recovery_mode,
            RecoveryMode::AcceptAuthorizedHistoryRewrite
        );
        assert_eq!(
            evidence.authorization_extension_id.as_deref(),
            Some("round-cap-3-4")
        );
        assert_eq!(
            evidence.authorization_resolution_digest.as_deref(),
            Some(round_cap_resolution_digest(&resolution).as_str())
        );
        let superseded_ref = evidence.superseded_candidate_ref.unwrap();
        assert_eq!(
            crate::git::run_stdout(
                &fixture.workspace,
                &["rev-parse", &superseded_ref],
                "read preserved superseded candidate",
            )
            .unwrap(),
            old_tip
        );
        assert_eq!(
            linear_suffix(&fixture.workspace, &protected_base, &rewritten)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn rejects_unauthorized_or_non_equivalent_history_rewrites() {
        let fixture = Fixture::new();
        let stored = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &stored.attempts[0];
        let protected_base = attempt.tasks[0]
            .output
            .as_ref()
            .unwrap()
            .base_commit
            .clone()
            .unwrap();
        let old_tip = attempt.tasks[1].output.as_ref().unwrap().commit.clone();
        let old_parent = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", &format!("{old_tip}^")],
            "read rejected rewrite parent",
        )
        .unwrap();
        let rewritten = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &fixture.tree,
                "-p",
                &old_parent,
                "-m",
                "Rewrite candidate message",
                "-m",
                "Change only approved terminology.",
            ],
            "create message-only rewrite",
        )
        .unwrap();
        assert!(
            !verify_equivalent_message_only_suffix(
                &fixture.workspace,
                &protected_base,
                &old_tip,
                &rewritten,
                "Rewrite some other commit.",
            )
            .unwrap()
        );

        let changed_author = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "-c",
                "user.name=Different Author",
                "-c",
                "user.email=different@example.com",
                "commit-tree",
                &fixture.tree,
                "-p",
                &old_parent,
                "-m",
                "Rewrite candidate message",
            ],
            "create authorship-changing rewrite",
        )
        .unwrap();
        assert!(
            !verify_equivalent_message_only_suffix(
                &fixture.workspace,
                &protected_base,
                &old_tip,
                &changed_author,
                &format!("Rewrite only {old_tip}."),
            )
            .unwrap()
        );

        let changed_tree = crate::git::run_stdout(
            &fixture.workspace,
            &["rev-parse", &format!("{}^{{tree}}", fixture.protected)],
            "read content-changing tree",
        )
        .unwrap();
        let content_change = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &changed_tree,
                "-p",
                &old_parent,
                "-m",
                "Rewrite candidate message",
            ],
            "create content-changing rewrite",
        )
        .unwrap();
        assert!(
            !verify_equivalent_message_only_suffix(
                &fixture.workspace,
                &protected_base,
                &old_tip,
                &content_change,
                &format!("Rewrite only {old_tip}."),
            )
            .unwrap()
        );

        let merge = crate::git::run_stdout(
            &fixture.workspace,
            &[
                "commit-tree",
                &fixture.tree,
                "-p",
                &old_parent,
                "-p",
                &protected_base,
                "-m",
                "Rewrite candidate message",
            ],
            "create nonlinear rewrite",
        )
        .unwrap();
        assert!(
            !verify_equivalent_message_only_suffix(
                &fixture.workspace,
                &protected_base,
                &old_tip,
                &merge,
                &format!("Rewrite only {old_tip}."),
            )
            .unwrap()
        );
    }
}
