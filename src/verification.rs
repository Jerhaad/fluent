//! Run authorized public checks in Task workers and validate their compact receipts.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, time::Duration};

use crate::{content::ContentResolver, host_evidence, work_model::*};

const MAX_RECEIPT_BYTES: u64 = 16 * 1024;
pub(crate) const MAX_LOG_BYTES: u64 = 384 * 1024;

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn sandbox_digest(resolver: &ContentResolver) -> Result<String> {
    Ok(digest(
        resolver
            .resolve_content("sandbox/common.sb")
            .context("verification checker requires common sandbox definition")?
            .as_bytes(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReceipt {
    pub schema_version: u32,
    pub work_item_id: String,
    pub attempt_id: String,
    pub task_id: String,
    pub candidate_commit: String,
    pub command: String,
    pub working_directory: String,
    pub checker_digest: String,
    pub result: host_evidence::EvidenceResult,
    pub started_at: String,
    pub completed_at: String,
    pub exit_status: i32,
    pub(crate) artifacts: Vec<BoundRecheckArtifact>,
}

fn read_bounded(project: &Path, path: &str, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let path = host_evidence::validate_managed_file_path(project, path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "verification artifact exceeds its size limit"
    );
    Ok(bytes)
}

pub(crate) fn validate_context(
    project: &Path,
    task: &Task,
    resolver: &ContentResolver,
) -> Result<()> {
    let context = task
        .verification_context
        .as_ref()
        .context("missing verification context")?;
    host_evidence::validate_focused_command(&context.checker.command)?;
    let review = task
        .review_context
        .as_ref()
        .context("missing verification candidate")?;
    ensure!(
        task.kind == TaskKind::Verification && task.workspace_access.writes.is_empty(),
        "verification requires read-only candidate access"
    );
    ensure!(
        context.candidate_commit == review.candidate_commit,
        "verification candidate identity changed"
    );
    ensure!(
        context.checker.schema_version == 1
            && (1..=3600).contains(&context.checker.timeout_seconds),
        "invalid verification checker definition"
    );
    ensure!(
        digest(&serde_json::to_vec(&context.checker)?) == context.checker_digest
            && sandbox_digest(resolver)? == context.checker.sandbox_digest,
        "verification checker digest changed"
    );
    ensure!(
        digest(&read_bounded(
            project,
            &context.review_artifact.path,
            1024 * 1024
        )?) == context.review_artifact.digest,
        "verification review fixture changed"
    );
    let workspace_path = Path::new(&review.candidate_workspace_path).canonicalize()?;
    let workspace = workspace_path.as_path();
    ensure!(
        workspace.is_absolute(),
        "verification candidate path must be absolute"
    );
    ensure!(
        task.workspace_access
            .reads
            .iter()
            .any(|entry| entry.id == review.candidate_workspace_id
                && entry.path == review.candidate_workspace_path),
        "verification candidate is not readable"
    );
    let head =
        crate::git::run_stdout(workspace, &["rev-parse", "HEAD"], "verify candidate commit")?;
    ensure!(
        head.trim() == context.candidate_commit,
        "verification candidate changed"
    );
    let status = crate::git::run_stdout(
        workspace,
        &["status", "--porcelain", "--untracked-files=no"],
        "verify clean candidate",
    )?;
    ensure!(status.trim().is_empty(), "verification candidate is dirty");
    host_evidence::resolve_working_directory(workspace, &context.checker.working_directory)?;
    Ok(())
}

/// Execute only the authorized check; publication never overwrites a prior receipt.
pub(crate) fn execute(project: &Path, task: &Task, resolver: &ContentResolver) -> Result<()> {
    validate_context(project, task, resolver)?;
    let context = task.verification_context.as_ref().unwrap();
    let candidate_path = Path::new(
        &task
            .review_context
            .as_ref()
            .unwrap()
            .candidate_workspace_path,
    )
    .canonicalize()?;
    let candidate = candidate_path.as_path();
    let area = task
        .artifact_area
        .as_ref()
        .context("missing verification artifact area")?;
    let run_dir = host_evidence::create_managed_directory_chain(project, Path::new(&area.path))?;
    let working_path =
        host_evidence::resolve_working_directory(candidate, &context.checker.working_directory)?;
    let started_at = task
        .started_at
        .clone()
        .context("verification has no start reservation")?;
    let execution = host_evidence::execute_focused_command(
        candidate,
        &working_path,
        &run_dir,
        &context.checker.command,
        Duration::from_secs(context.checker.timeout_seconds),
        resolver,
    )?;
    validate_context(project, task, resolver)?;
    let artifacts = ["stdout.log", "stderr.log"]
        .into_iter()
        .map(|name| {
            let path = format!("{}/{name}", area.path);
            Ok(BoundRecheckArtifact {
                digest: digest(&read_bounded(project, &path, MAX_LOG_BYTES)?),
                path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let receipt = VerificationReceipt {
        schema_version: 1,
        work_item_id: task.work_item_id.clone(),
        attempt_id: task
            .attempt_id
            .clone()
            .context("missing verification Attempt")?,
        task_id: task.id.clone(),
        candidate_commit: context.candidate_commit.clone(),
        command: context.checker.command.clone(),
        working_directory: context.checker.working_directory.clone(),
        checker_digest: context.checker_digest.clone(),
        result: if execution.exit_code == 0 {
            host_evidence::EvidenceResult::Pass
        } else {
            host_evidence::EvidenceResult::Fail
        },
        started_at,
        completed_at: now_iso8601(),
        exit_status: execution.exit_code,
        artifacts,
    };
    host_evidence::write_exclusive(
        &run_dir.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )
}

/// Validate bounded receipt metadata and artifact hashes before evidence review.
/// Detailed command output remains in the worker's artifact area.
pub fn accept(
    project: &Path,
    store: &WorkModelStore,
    task: &Task,
    resolver: &ContentResolver,
) -> Result<EvidenceRecovery> {
    validate_context(project, task, resolver)?;
    let context = task.verification_context.as_ref().unwrap();
    let area = &task
        .artifact_area
        .as_ref()
        .context("missing verification artifact area")?
        .path;
    let bytes = read_bounded(project, &format!("{area}/receipt.json"), MAX_RECEIPT_BYTES)?;
    let receipt: VerificationReceipt =
        serde_json::from_slice(&bytes).context("invalid verification receipt")?;
    ensure!(
        task.status == TaskStatus::Complete
            && receipt.schema_version == 1
            && receipt.work_item_id == task.work_item_id
            && Some(&receipt.attempt_id) == task.attempt_id.as_ref()
            && receipt.task_id == task.id
            && receipt.candidate_commit == context.candidate_commit
            && receipt.command == context.checker.command
            && receipt.working_directory == context.checker.working_directory
            && receipt.checker_digest == context.checker_digest,
        "verification receipt identity mismatch"
    );
    ensure!(
        task.started_at.as_ref() == Some(&receipt.started_at),
        "verification start identity mismatch"
    );
    let started = chrono::DateTime::parse_from_rfc3339(&receipt.started_at)?;
    let completed = chrono::DateTime::parse_from_rfc3339(&receipt.completed_at)?;
    ensure!(
        started <= completed && completed <= chrono::Utc::now(),
        "invalid verification timestamps"
    );
    ensure!(
        (receipt.exit_status == 0) == (receipt.result == host_evidence::EvidenceResult::Pass),
        "verification receipt result contradicts exit status"
    );
    ensure!(
        receipt.artifacts.len() == 2,
        "missing verification artifact digests"
    );
    for (artifact, name) in receipt.artifacts.iter().zip(["stdout.log", "stderr.log"]) {
        ensure!(
            artifact.path == format!("{area}/{name}")
                && artifact.digest
                    == digest(&read_bounded(project, &artifact.path, MAX_LOG_BYTES)?),
            "verification artifact digest mismatch"
        );
    }
    let document = host_evidence::EvidenceDocument {
        schema_version: 1,
        producer: "fluent verification worker".into(),
        check: receipt.command,
        working_directory: receipt.working_directory,
        result: receipt.result,
        run_at: receipt.completed_at,
        output: format!(
            "exit_code: {}\nReceipt: {area}/receipt.json\nReceipt digest: {}",
            receipt.exit_status,
            digest(&bytes)
        ),
    };
    let document_path = project.join(area).join("evidence.json");
    let document_bytes = serde_json::to_vec_pretty(&document)?;
    if document_path.exists() {
        ensure!(
            read_bounded(project, &format!("{area}/evidence.json"), MAX_RECEIPT_BYTES)?
                == document_bytes,
            "immutable verification evidence changed"
        );
    } else {
        host_evidence::write_exclusive(&document_path, &document_bytes)?;
    }
    host_evidence::attach(
        project,
        store,
        &task.work_item_id,
        task.attempt_id.as_deref().unwrap(),
        &context.candidate_commit,
        &document_path,
        &[context.review_artifact.path.clone()],
        None,
    )
}

pub(crate) fn is_accepted(project: &Path, attempt: &Attempt, task: &Task) -> Result<bool> {
    let area = &task
        .artifact_area
        .as_ref()
        .context("missing verification artifact area")?
        .path;
    let path = format!("{area}/evidence.json");
    match fs::symlink_metadata(project.join(&path)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let digest = digest(&read_bounded(project, &path, MAX_RECEIPT_BYTES)?);
            Ok(attempt
                .evidence_recoveries
                .iter()
                .any(|recovery| recovery.attachment.digest == digest))
        }
    }
}
