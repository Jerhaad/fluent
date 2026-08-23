//! Recover one host-verified metadata rewrite that accidentally replaced the
//! candidate's protected base while preserving the exact candidate tree.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
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
    TaskKind, TaskStatus, WorkModelError, WorkModelStore, WriterCompletionMatrix,
    resolve_managed_sibling_workspace_path, round_cap_resolution_digest, work_artifact_path,
};

const RECOVERY_SCHEMA_VERSION: u32 = 1;
const RECOVERY_PRODUCER: &str = "candidate-ancestry-recovery";
const RECOVERY_FILE: &str = "recovery.json";
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
    preserved_ref: String,
    resolution: String,
    resolution_digest: String,
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
    task_id: String,
}

pub fn recovery_action(project_root: &Path, attempt: &Attempt) -> Option<String> {
    inspect_eligible(project_root, attempt).ok().map(|_| {
        format!(
            "fluent attempt repair-candidate-ancestry {} {} --resolution \"<describe the approved ancestry repair>\"",
            attempt.work_item_id, attempt.id
        )
    })
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
        return Ok(repair);
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
            let recovered = crate::git::run_stdout(
                &workspace,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &eligible.authorization.protected_base_commit,
                    "-m",
                    "Recover candidate ancestry",
                    "-m",
                    "Preserve the verified candidate tree and protected base.",
                ],
                "create candidate ancestry recovery commit",
            )?;
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
                preserved_ref,
                resolution: resolution.clone(),
                resolution_digest: round_cap_resolution_digest(&resolution),
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
    let authorization = candidate_ancestry_repair_authorization(&gate)
        .ok_or_else(|| anyhow::anyhow!("candidate gate is not eligible for ancestry repair"))?;
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
        task_id: format!("{}-write-{next_round}", attempt.id),
    })
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
    if evidence.schema_version != RECOVERY_SCHEMA_VERSION
        || evidence.task_id != eligible.task_id
        || evidence.source_task_id != eligible.source_task_id
        || evidence.source_candidate_commit != eligible.source_candidate_commit
        || evidence.rejected_candidate_commit != eligible.authorization.rejected_candidate_commit
        || evidence.protected_base_commit != eligible.authorization.protected_base_commit
        || evidence.candidate_tree != eligible.authorization.candidate_tree
        || evidence.source_gate_path != eligible.source_gate_path
        || evidence.prior_gate_path != eligible.authorization.prior_gate_path
        || evidence.preserved_ref != expected_ref
        || evidence.resolution.trim().is_empty()
        || evidence.resolution_digest != round_cap_resolution_digest(&evidence.resolution)
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
        assert!(resumed.attempts[0].tasks.iter().any(|task| {
            task.kind == TaskKind::Review
                && task.review_context.as_ref().is_some_and(|context| {
                    context.candidate_commit == repair_result.recovered_candidate_commit
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
            preserved_ref,
            resolution: resolution.to_string(),
            resolution_digest: round_cap_resolution_digest(resolution),
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
}
