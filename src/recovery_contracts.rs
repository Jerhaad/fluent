//! Shared admission and completion contracts for recovery-bound Writers.
//!
//! This boundary is intentionally independent from the host-evidence producer
//! and task executor. Queue admission, direct execution, and host recovery all
//! consume the same immutable checks without introducing a module cycle.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::candidate_gate;
use crate::git;
use crate::review;
use crate::work_model::{
    self, ArtifactRef, EvidenceAttachment, EvidenceRecovery, EvidenceRecoveryState,
    EvidenceReviewTarget, HostEvidenceClassification, Task, TaskKind, TaskStatus, WorkModelError,
    WriterCompletionFinding,
};

pub(crate) const WRITER_COMPLETION_MATRIX_FILE: &str = "writer-completion.json";
const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;
const CODE_CHANGE_DISPOSITION: &str = "code-change";

/// Read the approved host obligations against the exact clean Writer candidate.
/// Consumers may overlay accepted evidence, but the Writer cannot supply it.
pub(crate) fn owned_host_completion_matrix(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<(Task, ArtifactRef, work_model::WriterCompletionMatrix)>> {
    let Some(contract) = attempt
        .writer_completion_contract
        .as_ref()
        .filter(|contract| {
            contract
                .requirements
                .iter()
                .any(|row| row.responsibility == Some(work_model::CompletionResponsibility::Host))
        })
    else {
        return Ok(None);
    };
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .context("required host verification has no completed Writer")?;
    let output = writer
        .output
        .as_ref()
        .context("required host verification has no candidate")?;
    anyhow::ensure!(
        candidate_workspace_is_clean_at(project_root, output, &output.commit),
        "required host verification needs the unchanged clean candidate {}",
        output.commit
    );
    let artifact = writer_completion_matrix_ref(project_root, attempt, writer)
        .context("required host verification has no source matrix")?;
    let bytes = fs::read(resolve_input_artifact_path(project_root, &artifact.path)?)?;
    anyhow::ensure!(
        bytes.len() <= work_model::WRITER_COMPLETION_MATRIX_MAX_BYTES,
        "completion matrix exceeds size limit"
    );
    let mut matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
    let findings = writer_completion_findings(project_root, writer)?;
    matrix
        .status_against(contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    matrix
        .repair_writer_claims_against(contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    Ok(Some((writer.clone(), artifact, matrix)))
}

/// Version-one operator evidence document. The parser below is the single
/// validation boundary for host-evidence attachment and source correction.
/// Callers retain the exact serialized bytes separately as the audit artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDocument {
    pub schema_version: u32,
    pub producer: String,
    pub check: String,
    pub working_directory: String,
    pub result: EvidenceResult,
    pub run_at: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceResult {
    Pass,
    Fail,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CodeChangeClassificationAudit<'a> {
    pub(crate) schema_version: u32,
    pub(crate) operation_id: &'a str,
    pub(crate) work_item_id: &'a str,
    pub(crate) attempt_id: &'a str,
    pub(crate) candidate_commit: &'a str,
    pub(crate) recovery_id: &'a str,
    pub(crate) evidence: &'a EvidenceAttachment,
    pub(crate) review_artifact: &'a str,
    pub(crate) review_digest: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disposition: Option<&'a str>,
    pub(crate) authorization: &'a str,
    pub(crate) authorization_digest: &'a str,
    pub(crate) writer_task_id: &'a str,
}

/// Return the task-owned completion matrix path without opening it.
pub(crate) fn writer_completion_matrix_path(project_root: &Path, task: &Task) -> Option<PathBuf> {
    task.artifact_area.as_ref().map(|area| {
        project_root
            .join(&area.path)
            .join(WRITER_COMPLETION_MATRIX_FILE)
    })
}

/// Resolve the task-owned completion matrix path under the managed artifact
/// root before it is read or written.
pub(crate) fn managed_writer_completion_matrix_path(
    project_root: &Path,
    task: &Task,
) -> Result<Option<PathBuf>> {
    task.artifact_area
        .as_ref()
        .map(|area| {
            resolve_managed_artifact_area_path(project_root, &area.path)
                .map(|dir| dir.join(WRITER_COMPLETION_MATRIX_FILE))
        })
        .transpose()
}

/// Resolve an input artifact from one of Fluent's managed roots.
pub(crate) fn resolve_input_artifact_path(project_root: &Path, path: &str) -> Result<PathBuf> {
    let relative_path = Path::new(path);
    if relative_path.is_absolute() {
        bail!("Input artifact path must be relative: {path}");
    }
    let mut components = Vec::new();
    for component in relative_path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_owned()),
            _ => bail!(
                "Input artifact path must stay under .fluent/work/artifacts/ or .fluent/work/progress/: {path}"
            ),
        }
    }
    let valid = components.len() >= 3
        && components[0] == std::ffi::OsStr::new(".fluent")
        && components[1] == std::ffi::OsStr::new("work")
        && (components[2] == std::ffi::OsStr::new("artifacts")
            || components[2] == std::ffi::OsStr::new("progress"));
    if !valid {
        bail!(
            "Input artifact path must be under .fluent/work/artifacts/ or .fluent/work/progress/: {path}"
        );
    }
    Ok(resolve_workspace_path(project_root, path))
}

/// Resolve a task artifact area below `.fluent/work/artifacts`.
pub(crate) fn resolve_managed_artifact_area_path(
    project_root: &Path,
    path: &str,
) -> Result<PathBuf> {
    let relative_path = Path::new(path);
    if relative_path.is_absolute() {
        bail!("Task artifact area path must be relative: {path}");
    }

    let mut components = Vec::new();
    for component in relative_path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_owned()),
            _ => bail!("Task artifact area path must stay under .fluent/work/artifacts: {path}"),
        }
    }

    let managed_prefix = [
        std::ffi::OsStr::new(".fluent"),
        std::ffi::OsStr::new("work"),
        std::ffi::OsStr::new("artifacts"),
    ];
    if components.len() <= managed_prefix.len()
        || !components
            .iter()
            .zip(managed_prefix.iter())
            .all(|(actual, expected)| actual == expected)
    {
        bail!("Task artifact area path must stay under .fluent/work/artifacts: {path}");
    }

    Ok(resolve_workspace_path(project_root, path))
}

fn resolve_workspace_path(project_root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

/// Collect immutable findings that a Writer completion matrix must address.
pub(crate) fn writer_completion_findings(
    project_root: &Path,
    task: &Task,
) -> Result<Vec<WriterCompletionFinding>> {
    let mut findings = BTreeMap::new();
    // A Tester-harness repair is authorized by its explicit operator decision.
    // Its ordinary command failures are not product obligations for the scoped
    // repair and therefore do not enter this matrix.
    if task.role == work_model::TESTER_HARNESS_REPAIR_ROLE {
        return Ok(findings.into_values().collect());
    }
    for artifact in &task.input_artifacts {
        let relative = Path::new(&artifact.path);
        let is_review = relative.file_name().is_some_and(|name| {
            name == "review.md" || name.to_string_lossy().starts_with("review-")
        });
        if !is_review {
            continue;
        }
        let path = resolve_input_artifact_path(project_root, &artifact.path)?;
        let source = fs::read_to_string(&path).with_context(|| {
            format!(
                "read review findings for Writer completion matrix: {}",
                path.display()
            )
        })?;
        let role = review::REVIEWERS
            .iter()
            .find(|role| {
                artifact.producer_id.contains(**role)
                    || artifact.path.contains(&format!("review-{role}"))
            })
            .copied()
            .unwrap_or("unknown");
        for title in review::open_finding_titles(&source) {
            let identity = review::finding_identity(role, &title);
            findings
                .entry((role.to_string(), identity.clone()))
                .or_insert(WriterCompletionFinding {
                    id: format!("finding:{role}:{identity}"),
                    role: role.to_string(),
                    source: artifact.path.clone(),
                    requirement: title,
                });
        }
    }
    for artifact in &task.input_artifacts {
        let relative = Path::new(&artifact.path);
        let is_candidate_gate = relative
            .file_name()
            .is_some_and(|name| name == candidate_gate::CANDIDATE_GATE_FILE);
        if !is_candidate_gate {
            continue;
        }
        let path = resolve_input_artifact_path(project_root, &artifact.path)?;
        let source = fs::read(&path).with_context(|| {
            format!(
                "read candidate gate findings for Writer completion matrix: {}",
                path.display()
            )
        })?;
        let evidence = serde_json::from_slice::<candidate_gate::CandidateGateEvidence>(&source)
            .with_context(|| {
                format!(
                    "parse candidate gate findings for Writer completion matrix: {}",
                    path.display()
                )
            })?;
        if evidence.disposition != candidate_gate::CandidateGateDisposition::Rejected {
            continue;
        }
        for check in evidence
            .checks
            .iter()
            .filter(|check| check.status == candidate_gate::CandidateCheckStatus::Failed)
        {
            let diagnostics = if check.diagnostics.is_empty() {
                vec![format!("{} check failed", check.name)]
            } else {
                check.diagnostics.clone()
            };
            for diagnostic in diagnostics {
                let role = "candidate-gate";
                let identity = review::finding_identity(role, &diagnostic);
                findings
                    .entry((role.to_string(), identity.clone()))
                    .or_insert(WriterCompletionFinding {
                        id: format!("finding:{role}:{identity}"),
                        role: role.to_string(),
                        source: artifact.path.clone(),
                        requirement: diagnostic,
                    });
            }
        }
    }
    for artifact in &task.input_artifacts {
        let relative = Path::new(&artifact.path);
        let is_tester_results = relative.file_name().is_some_and(|name| {
            name == "tester-results.json"
                && relative
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|parent| parent.to_string_lossy().contains("-tester"))
        });
        if !is_tester_results {
            continue;
        }
        let path = resolve_input_artifact_path(project_root, &artifact.path)?;
        let source = fs::read(&path).with_context(|| {
            format!(
                "read Tester findings for Writer completion matrix: {}",
                path.display()
            )
        })?;
        let results = serde_json::from_slice::<crate::tester::TesterResults>(&source)
            .with_context(|| {
                format!(
                    "parse Tester findings for Writer completion matrix: {}",
                    path.display()
                )
            })?;
        for (index, command) in results
            .commands
            .iter()
            .enumerate()
            .filter(|(_, command)| command.exit_code != 0)
        {
            let role = "tester";
            let stable_title = format!("configured command {index}: {}", command.command);
            let identity = review::finding_identity(role, &stable_title);
            findings
                .entry((role.to_string(), identity.clone()))
                .or_insert(WriterCompletionFinding {
                    id: format!("finding:{role}:{identity}"),
                    role: role.to_string(),
                    source: artifact.path.clone(),
                    requirement: format!(
                        "Configured Tester command {index} failed with exit code {}",
                        command.exit_code
                    ),
                });
        }
    }
    Ok(findings.into_values().collect())
}

/// Revalidate a classified source-correction Writer immediately before it is
/// reserved for execution. Ordinary Writers do not carry a matching
/// classification and pass through unchanged.
pub(crate) fn validate_code_change_writer_start(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    task_id: &str,
) -> std::result::Result<(), WorkModelError> {
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| code_change_writer_rejection(attempt_id, "Attempt is not present"))?;
    let (writer_index, writer) = attempt
        .tasks
        .iter()
        .enumerate()
        .find(|(_, task)| task.id == task_id && task.kind == TaskKind::Write)
        .ok_or_else(|| code_change_writer_rejection(attempt_id, "classified Writer is missing"))?;

    let classified = attempt
        .evidence_recoveries
        .iter()
        .rev()
        .find_map(|recovery| {
            recovery
                .code_change_classification
                .as_ref()
                .filter(|classification| classification.writer_task_id == task_id)
                .map(|classification| (recovery, classification))
        });
    let pending_classification_for_other_writer = attempt
        .evidence_recoveries
        .iter()
        .rev()
        .find_map(|recovery| {
            recovery
                .code_change_classification
                .as_ref()
                .filter(|classification| {
                    classification.writer_task_id != task_id
                        && attempt.tasks.iter().any(|task| {
                            task.id == classification.writer_task_id
                                && task.kind == TaskKind::Write
                                && task.status == TaskStatus::Planned
                                && task.output.is_none()
                        })
                })
        });
    let Some((recovery, classification)) = classified else {
        if pending_classification_for_other_writer.is_some() {
            return Err(code_change_writer_rejection(
                attempt_id,
                "another classified Writer owns the planned continuation",
            ));
        }
        return Ok(());
    };

    let rejected = |reason: &str| code_change_writer_rejection(attempt_id, reason);
    validate_classification_recovery_scope(attempt, &recovery.id)
        .map_err(|reason| rejected(&reason))?;
    if item.attempts.last().map(|current| current.id.as_str()) != Some(attempt_id)
        || attempt.status != work_model::AttemptStatus::Planned
        || attempt.review_state != Some(work_model::AttemptReviewState::Failed)
        || attempt.pause_kind.is_some()
        || attempt.pause_recovery.is_some()
        || attempt.completed_at.is_some()
    {
        return Err(rejected(
            "classified source-correction Attempt is not the current planned frontier",
        ));
    }
    if recovery.state != EvidenceRecoveryState::CodeChange
        || writer.status != TaskStatus::Planned
        || writer.output.is_some()
        || writer.role != "author"
        || writer.work_item_id != item.id
        || writer.attempt_id.as_deref() != Some(attempt_id)
        || writer_index + 1 != attempt.tasks.len()
    {
        return Err(rejected(
            "classified Writer is not the final planned continuation",
        ));
    }
    if attempt.tasks.iter().enumerate().any(|(index, task)| {
        index != writer_index
            && (task.status == TaskStatus::Executing
                || task.status == TaskStatus::NeedsUser
                || (task.status == TaskStatus::Failed
                    && (task.kind != TaskKind::Write || task.output.is_some()))
                || task.status == TaskStatus::Planned)
    }) {
        return Err(rejected("Attempt contains another live or unresolved Task"));
    }
    if item.merge_candidates.iter().any(|candidate| {
        candidate.attempt_id == attempt_id
            && candidate.candidate_commit == recovery.candidate_commit
    }) {
        return Err(rejected("a Merge Candidate already owns this candidate"));
    }

    let source_output = attempt
        .tasks
        .iter()
        .take(writer_index)
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .ok_or_else(|| rejected("the source candidate Writer is missing"))?;
    if writer.workspace_access.writes.len() != 1
        || writer.workspace_access.writes[0].id != source_output.workspace_id
        || writer.workspace_access.writes[0].path != source_output.workspace_path
    {
        return Err(rejected(
            "the corrective Writer workspace is not candidate-bound",
        ));
    }

    let target = unique_tests_review_target(attempt, recovery, &classification.review_artifact)
        .map_err(|reason| rejected(&reason))?
        .target;
    let review_task_id = target
        .review_task_id
        .as_deref()
        .ok_or_else(|| rejected("classified recovery target has no review Task identity"))?;
    if target
        .review_digest
        .as_deref()
        .is_some_and(|digest| digest != classification.review_digest)
    {
        return Err(rejected("saved review digest changed"));
    }
    let review_task = attempt
        .tasks
        .iter()
        .find(|task| {
            task.id == review_task_id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task.work_item_id == item.id
                && task.attempt_id.as_deref() == Some(attempt_id)
                && review_artifact_path(task).as_deref()
                    == Some(classification.review_artifact.as_str())
                && task.review_context.as_ref().is_some_and(|context| {
                    context.candidate_commit == recovery.candidate_commit
                        && context.candidate_workspace_id == source_output.workspace_id
                        && context.candidate_workspace_path == source_output.workspace_path
                })
                && task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        context.recovery_id == recovery.id
                            && context.candidate_commit == recovery.candidate_commit
                            && context.attachment == recovery.attachment
                            && context.prior_review_artifact == target.prior_review_artifact
                    })
        })
        .ok_or_else(|| rejected("bound Tests Review Task is missing or mismatched"))?;
    validate_code_change_candidate_binding(
        project_root,
        item,
        attempt,
        recovery,
        target,
        review_task,
        source_output,
        &recovery.candidate_commit,
        &classification.review_artifact,
        &recovery.attachment.digest,
    )
    .map_err(|reason| rejected(&reason))?;
    validate_classification_task_frontier(attempt, review_task_id, Some(task_id))
        .map_err(|reason| rejected(&reason))?;
    let evidence_ref = ArtifactRef {
        producer_id: "host-evidence".to_string(),
        path: recovery.attachment.snapshot_path.clone(),
    };
    if review_task
        .input_artifacts
        .iter()
        .filter(|input| *input == &evidence_ref)
        .count()
        != 1
        || review_task
            .input_artifacts
            .iter()
            .any(|input| input.path == evidence_ref.path && input != &evidence_ref)
    {
        return Err(rejected("bound Tests Review Task lost its evidence input"));
    }

    let review_path = validate_managed_file_path(project_root, &classification.review_artifact)
        .map_err(|error| {
            code_change_writer_rejection(
                attempt_id,
                format!("review artifact is not a managed regular file: {error}"),
            )
        })?;
    let review_bytes = fs::read(&review_path).map_err(|error| {
        code_change_writer_rejection(
            attempt_id,
            format!("cannot read bound review artifact: {error}"),
        )
    })?;
    let review_digest = format!("sha256:{:x}", Sha256::digest(&review_bytes));
    if review_digest != classification.review_digest {
        return Err(rejected("bound review bytes changed"));
    }
    let review_text = std::str::from_utf8(&review_bytes)
        .map_err(|_| rejected("bound review artifact is not valid UTF-8"))?;
    let disposition = review::extract_evidence_disposition(review_text);
    let legacy_disposition = review::has_unqualified_evidence_needed_disposition(review_text);
    if review::extract_verdict(review_text) != review::Verdict::Fail
        || (disposition != Some(review::EvidenceDisposition::EvidenceNeeded) && !legacy_disposition)
    {
        return Err(rejected("bound review no longer requests evidence-needed"));
    }

    let authorization_digest = format!(
        "sha256:{:x}",
        Sha256::digest(classification.authorization.as_bytes())
    );
    if authorization_digest != classification.authorization_digest {
        return Err(rejected("classification authorization digest changed"));
    }
    if classification.disposition != CODE_CHANGE_DISPOSITION {
        return Err(rejected("classification uses an unsupported disposition"));
    }
    let expected_operation_id = code_change_operation_id(
        attempt_id,
        &recovery.candidate_commit,
        &recovery.id,
        &classification.review_artifact,
        &classification.review_digest,
        &recovery.attachment.digest,
        &classification.disposition,
        &classification.authorization_digest,
    );
    let legacy_identity = classification_uses_legacy_identity(classification, attempt_id, recovery);
    if classification.operation_id != expected_operation_id && !legacy_identity {
        return Err(rejected("classification operation identity changed"));
    }
    let expected_writer_task_id = format!(
        "{attempt_id}-write-{}",
        attempt
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Write)
            .count()
    );
    if classification.writer_task_id != expected_writer_task_id {
        return Err(rejected("classification Writer identity changed"));
    }
    let expected_audit_artifact = ArtifactRef {
        producer_id: classification.operation_id.clone(),
        path: work_model::work_artifact_path(
            &item.id,
            attempt_id,
            &format!("{}/classification.json", classification.operation_id),
        ),
    };
    if classification.audit_artifact != expected_audit_artifact
        || attempt
            .artifacts
            .iter()
            .filter(|artifact| *artifact == &classification.audit_artifact)
            .count()
            != 1
    {
        return Err(rejected("classification audit binding changed"));
    }
    let audit_path = validate_managed_file_path(project_root, &classification.audit_artifact.path)
        .map_err(|error| {
            code_change_writer_rejection(
                attempt_id,
                format!("classification audit is not a managed regular file: {error}"),
            )
        })?;
    let audit_bytes = fs::read(&audit_path).map_err(|error| {
        code_change_writer_rejection(
            attempt_id,
            format!("cannot read classification audit: {error}"),
        )
    })?;
    let expected_audit = CodeChangeClassificationAudit {
        schema_version: 1,
        operation_id: &classification.operation_id,
        work_item_id: &item.id,
        attempt_id,
        candidate_commit: &recovery.candidate_commit,
        recovery_id: &recovery.id,
        evidence: &recovery.attachment,
        review_artifact: &classification.review_artifact,
        review_digest: &classification.review_digest,
        disposition: (!legacy_identity).then_some(classification.disposition.as_str()),
        authorization: &classification.authorization,
        authorization_digest: &classification.authorization_digest,
        writer_task_id: &classification.writer_task_id,
    };
    let expected_audit_bytes = serde_json::to_vec_pretty(&expected_audit)
        .map_err(|error| code_change_writer_rejection(attempt_id, error.to_string()))?;
    if audit_bytes != expected_audit_bytes {
        return Err(rejected("classification audit bytes changed"));
    }

    let expected_review_input = ArtifactRef {
        producer_id: review_task.id.clone(),
        path: classification.review_artifact.clone(),
    };
    let expected_refs = [
        expected_review_input,
        evidence_ref,
        classification.audit_artifact.clone(),
    ];
    for expected in expected_refs {
        if writer
            .input_artifacts
            .iter()
            .filter(|input| *input == &expected)
            .count()
            != 1
            || writer
                .input_artifacts
                .iter()
                .any(|input| input.path == expected.path && input != &expected)
        {
            return Err(rejected("corrective Writer input binding changed"));
        }
    }
    Ok(())
}

/// Fill the target digest on a legacy classified recovery after validating the
/// exact persisted Review and every replay binding against a cloned aggregate.
///
/// Older classified recoveries may omit `EvidenceReviewTarget.review_digest`,
/// even though the classification already stores the digest used to create the
/// operation and its audit. Derive the missing field from the managed Review
/// bytes, validate the candidate-bound replay with that field present, and only
/// then update the caller's aggregate. This keeps a rejected migration a
/// durable no-op and never changes the classification identity or audit.
/// Queue admission uses this replay-only contract so an interrupted corrective
/// Writer can be migrated before the Attempt loop reopens it. The strict
/// planned-frontier contract remains at the actual Writer launch.
pub(crate) fn migrate_legacy_code_change_review_digest(
    project_root: &Path,
    item: &mut work_model::WorkItem,
    attempt_id: &str,
    writer_task_id: Option<&str>,
) -> std::result::Result<bool, String> {
    let Some(attempt_index) = item
        .attempts
        .iter()
        .position(|attempt| attempt.id == attempt_id)
    else {
        return Ok(false);
    };
    let attempt = &item.attempts[attempt_index];
    let Some((recovery_index, classification)) = attempt
        .evidence_recoveries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, recovery)| {
            let classification = recovery.code_change_classification.as_ref()?;
            (recovery.state == EvidenceRecoveryState::CodeChange
                && writer_task_id
                    .is_none_or(|writer_task_id| classification.writer_task_id == writer_task_id))
            .then_some((index, classification))
        })
    else {
        return Ok(false);
    };
    let review_artifact = classification.review_artifact.clone();
    let review_digest = classification.review_digest.clone();
    let recovery = &attempt.evidence_recoveries[recovery_index];
    let selected_target = unique_tests_review_target(attempt, recovery, &review_artifact)?;
    let target_index = selected_target.index;
    let target = selected_target.target;
    if target.review_digest.is_some() {
        return Ok(false);
    }
    let review_task_id = target
        .review_task_id
        .clone()
        .ok_or_else(|| "classified recovery target has no review Task identity".to_string())?;
    let review_path = validate_managed_file_path(project_root, &review_artifact)
        .map_err(|error| format!("review artifact is not a managed regular file: {error}"))?;
    let review_bytes = fs::read(&review_path)
        .map_err(|error| format!("cannot read bound review artifact: {error}"))?;
    let derived_review_digest = format!("sha256:{:x}", Sha256::digest(&review_bytes));
    if derived_review_digest != review_digest {
        return Err("bound review bytes changed".to_string());
    }

    let mut migrated = item.clone();
    migrated.attempts[attempt_index].evidence_recoveries[recovery_index].targets[target_index]
        .review_digest = Some(derived_review_digest.clone());
    let migrated_attempt = &migrated.attempts[attempt_index];
    let migrated_recovery = &migrated_attempt.evidence_recoveries[recovery_index];
    let migrated_classification = migrated_recovery
        .code_change_classification
        .as_ref()
        .expect("legacy classification remains present in cloned aggregate");
    validate_code_change_classification_replay(
        project_root,
        &migrated,
        migrated_attempt,
        migrated_recovery,
        migrated_classification,
        &review_task_id,
    )?;
    item.attempts[attempt_index].evidence_recoveries[recovery_index].targets[target_index]
        .review_digest = Some(derived_review_digest);
    Ok(true)
}

/// Validate an exact replay of an already-persisted source classification.
///
/// Classification mutates the model before a corrective Writer can be claimed,
/// so a retry may observe that Writer in a resumable lifecycle state. A hard
/// failure is not resumable without an explicit validated retry transition.
/// Keep this contract separate from the first-mutation frontier: replay checks
/// the stored operation and every immutable binding, while the frontier
/// validator below remains strict for a new classification.
pub(crate) fn validate_code_change_classification_replay(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    classification: &HostEvidenceClassification,
    review_task_id: &str,
) -> std::result::Result<(), String> {
    let rejected = |reason: &str| reason.to_string();
    if item.attempts.last().map(|current| current.id.as_str()) != Some(attempt.id.as_str()) {
        return Err(rejected("the classified Attempt is no longer current"));
    }
    validate_classification_recovery_scope(attempt, &recovery.id)?;
    if recovery.state != EvidenceRecoveryState::CodeChange {
        return Err(rejected(
            "stored classification recovery is not code-change",
        ));
    }
    if classification.disposition != CODE_CHANGE_DISPOSITION {
        return Err(rejected(
            "stored classification uses an unsupported disposition",
        ));
    }
    if item.merge_candidates.iter().any(|candidate| {
        candidate.attempt_id == attempt.id
            && candidate.candidate_commit == recovery.candidate_commit
    }) {
        return Err(rejected("a Merge Candidate already owns this candidate"));
    }

    let (writer_index, writer) = attempt
        .tasks
        .iter()
        .enumerate()
        .find(|(_, task)| task.id == classification.writer_task_id)
        .ok_or_else(|| rejected("classified Writer is missing"))?;
    if writer.kind != TaskKind::Write
        || writer.role != "author"
        || writer.work_item_id != item.id
        || writer.attempt_id.as_deref() != Some(attempt.id.as_str())
    {
        return Err(rejected("classified Writer identity changed"));
    }
    let lifecycle_is_replayable = match writer.status {
        TaskStatus::Planned | TaskStatus::Executing | TaskStatus::NeedsUser => {
            writer.output.is_none()
        }
        TaskStatus::Complete => writer.output.is_some(),
        TaskStatus::Failed => false,
        TaskStatus::Superseded => false,
    };
    if !lifecycle_is_replayable {
        return Err(rejected("classified Writer lifecycle is not replayable"));
    }

    let source_output = attempt
        .tasks
        .iter()
        .take(writer_index)
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .ok_or_else(|| rejected("the source candidate Writer is missing"))?;
    if writer.workspace_access.writes.len() != 1
        || writer.workspace_access.writes[0].id != source_output.workspace_id
        || writer.workspace_access.writes[0].path != source_output.workspace_path
    {
        return Err(rejected(
            "the corrective Writer workspace is not candidate-bound",
        ));
    }

    validate_classification_replay_task_frontier(attempt, review_task_id, writer_index)?;

    let target =
        unique_tests_review_target(attempt, recovery, &classification.review_artifact)?.target;
    if target.review_task_id.as_deref() != Some(review_task_id) {
        return Err(rejected(
            "classified recovery target does not match the stored Review",
        ));
    }
    if target
        .review_digest
        .as_deref()
        .is_some_and(|digest| digest != classification.review_digest)
    {
        return Err(rejected("saved review digest changed"));
    }
    let review_task = attempt
        .tasks
        .iter()
        .find(|task| {
            task.id == review_task_id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task.work_item_id == item.id
                && task.attempt_id.as_deref() == Some(attempt.id.as_str())
                && review_artifact_path(task).as_deref()
                    == Some(classification.review_artifact.as_str())
                && task.review_context.as_ref().is_some_and(|context| {
                    context.candidate_commit == recovery.candidate_commit
                        && context.candidate_workspace_id == source_output.workspace_id
                        && context.candidate_workspace_path == source_output.workspace_path
                })
                && task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        context.recovery_id == recovery.id
                            && context.candidate_commit == recovery.candidate_commit
                            && context.attachment == recovery.attachment
                            && context.prior_review_artifact == target.prior_review_artifact
                    })
        })
        .ok_or_else(|| rejected("bound Tests Review Task is missing or mismatched"))?;
    match writer.status {
        TaskStatus::Complete => {
            let corrective_output = writer
                .output
                .as_ref()
                .ok_or_else(|| rejected("completed classified Writer has no output"))?;
            validate_code_change_post_correction_binding(
                project_root,
                item,
                attempt,
                recovery,
                target,
                review_task,
                source_output,
                corrective_output,
                &classification.review_artifact,
                &recovery.attachment.digest,
            )?;
        }
        TaskStatus::Planned | TaskStatus::Executing | TaskStatus::NeedsUser => {
            validate_code_change_candidate_binding(
                project_root,
                item,
                attempt,
                recovery,
                target,
                review_task,
                source_output,
                &recovery.candidate_commit,
                &classification.review_artifact,
                &recovery.attachment.digest,
            )?;
        }
        TaskStatus::Failed | TaskStatus::Superseded => {
            return Err(rejected("classified Writer lifecycle is not replayable"));
        }
    }
    let evidence_ref = ArtifactRef {
        producer_id: "host-evidence".to_string(),
        path: recovery.attachment.snapshot_path.clone(),
    };
    if review_task
        .input_artifacts
        .iter()
        .filter(|input| *input == &evidence_ref)
        .count()
        != 1
        || review_task
            .input_artifacts
            .iter()
            .any(|input| input.path == evidence_ref.path && input != &evidence_ref)
    {
        return Err(rejected("bound Tests Review Task lost its evidence input"));
    }

    let review_path = validate_managed_file_path(project_root, &classification.review_artifact)
        .map_err(|error| {
            rejected(&format!(
                "review artifact is not a managed regular file: {error}"
            ))
        })?;
    let review_bytes = fs::read(&review_path)
        .map_err(|error| rejected(&format!("cannot read bound review artifact: {error}")))?;
    if format!("sha256:{:x}", Sha256::digest(&review_bytes)) != classification.review_digest {
        return Err(rejected("bound review bytes changed"));
    }
    let review_text = std::str::from_utf8(&review_bytes)
        .map_err(|_| rejected("bound review artifact is not valid UTF-8"))?;
    let disposition = review::extract_evidence_disposition(review_text);
    let legacy_disposition = review::has_unqualified_evidence_needed_disposition(review_text);
    if review::extract_verdict(review_text) != review::Verdict::Fail
        || (disposition != Some(review::EvidenceDisposition::EvidenceNeeded) && !legacy_disposition)
    {
        return Err(rejected("bound review no longer requests evidence-needed"));
    }

    let authorization_digest = format!(
        "sha256:{:x}",
        Sha256::digest(classification.authorization.as_bytes())
    );
    if authorization_digest != classification.authorization_digest {
        return Err(rejected("classification authorization digest changed"));
    }
    let expected_operation_id = code_change_operation_id(
        &attempt.id,
        &recovery.candidate_commit,
        &recovery.id,
        &classification.review_artifact,
        &classification.review_digest,
        &recovery.attachment.digest,
        &classification.disposition,
        &classification.authorization_digest,
    );
    let legacy_identity =
        classification_uses_legacy_identity(classification, &attempt.id, recovery);
    if classification.operation_id != expected_operation_id && !legacy_identity {
        return Err(rejected("classification operation identity changed"));
    }
    let expected_writer_task_id = format!(
        "{}-write-{}",
        attempt.id,
        attempt.tasks[..=writer_index]
            .iter()
            .filter(|task| task.kind == TaskKind::Write)
            .count()
    );
    if classification.writer_task_id != expected_writer_task_id {
        return Err(rejected("classification Writer identity changed"));
    }
    let expected_audit_artifact = ArtifactRef {
        producer_id: classification.operation_id.clone(),
        path: work_model::work_artifact_path(
            &item.id,
            &attempt.id,
            &format!("{}/classification.json", classification.operation_id),
        ),
    };
    if classification.audit_artifact != expected_audit_artifact
        || attempt
            .artifacts
            .iter()
            .filter(|artifact| *artifact == &classification.audit_artifact)
            .count()
            != 1
    {
        return Err(rejected("classification audit binding changed"));
    }
    let expected_audit = CodeChangeClassificationAudit {
        schema_version: 1,
        operation_id: &classification.operation_id,
        work_item_id: &item.id,
        attempt_id: &attempt.id,
        candidate_commit: &recovery.candidate_commit,
        recovery_id: &recovery.id,
        evidence: &recovery.attachment,
        review_artifact: &classification.review_artifact,
        review_digest: &classification.review_digest,
        disposition: (!legacy_identity).then_some(classification.disposition.as_str()),
        authorization: &classification.authorization,
        authorization_digest: &classification.authorization_digest,
        writer_task_id: &classification.writer_task_id,
    };
    let expected_audit_bytes = serde_json::to_vec_pretty(&expected_audit)
        .map_err(|error| rejected(&format!("serialize classification audit: {error}")))?;
    let audit_file = project_root.join(&classification.audit_artifact.path);
    match fs::symlink_metadata(&audit_file) {
        Ok(_) => {
            let audit_path =
                validate_managed_file_path(project_root, &classification.audit_artifact.path)
                    .map_err(|error| {
                        rejected(&format!(
                            "classification audit is not a managed regular file: {error}"
                        ))
                    })?;
            let audit_bytes = fs::read(&audit_path)
                .map_err(|error| rejected(&format!("cannot read classification audit: {error}")))?;
            if audit_bytes != expected_audit_bytes {
                return Err(rejected("classification audit bytes changed"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(rejected(&format!(
                "cannot inspect classification audit: {error}"
            )));
        }
    }

    let expected_review_input = ArtifactRef {
        producer_id: review_task.id.clone(),
        path: classification.review_artifact.clone(),
    };
    for expected in [
        expected_review_input,
        evidence_ref,
        classification.audit_artifact.clone(),
    ] {
        if writer
            .input_artifacts
            .iter()
            .filter(|input| *input == &expected)
            .count()
            != 1
            || writer
                .input_artifacts
                .iter()
                .any(|input| input.path == expected.path && input != &expected)
        {
            return Err(rejected("corrective Writer input binding changed"));
        }
    }
    Ok(())
}

fn code_change_writer_rejection(attempt_id: &str, reason: impl Into<String>) -> WorkModelError {
    WorkModelError::HostEvidenceRecoveryIneligible {
        attempt_id: attempt_id.to_string(),
        reason: reason.into(),
    }
}

/// Validate the task frontier that a host-evidence classification may mutate.
pub(crate) fn validate_classification_task_frontier(
    attempt: &work_model::Attempt,
    review_task_id: &str,
    allowed_planned_writer_id: Option<&str>,
) -> std::result::Result<(), String> {
    let (review_index, review_task) = attempt
        .tasks
        .iter()
        .enumerate()
        .find(|(_, task)| task.id == review_task_id)
        .ok_or_else(|| "the classified Tests Review Task is missing".to_string())?;
    if review_task.kind != TaskKind::Review
        || review_task.role != "tests"
        || review_task.status != TaskStatus::Complete
    {
        return Err("the classified Tests Review Task is not complete".to_string());
    }

    let mut failed_writer_count = 0usize;
    let mut planned_writer_count = 0usize;
    for (index, task) in attempt.tasks.iter().enumerate() {
        let allowed_planned_writer = allowed_planned_writer_id.is_some_and(|id| id == task.id);
        match task.status {
            TaskStatus::Complete | TaskStatus::Superseded => {}
            TaskStatus::Failed
                if task.kind == TaskKind::Write
                    && task.output.is_none()
                    && index < review_index =>
            {
                failed_writer_count += 1;
            }
            TaskStatus::Planned
                if allowed_planned_writer
                    && task.kind == TaskKind::Write
                    && task.output.is_none() =>
            {
                planned_writer_count += 1;
            }
            _ => {
                return Err(format!(
                    "Task {} is an unrelated live or unresolved frontier entry",
                    task.id
                ));
            }
        }
    }
    if failed_writer_count > 1 {
        return Err("the frontier contains multiple failed output-free Writers".to_string());
    }
    if allowed_planned_writer_id.is_some() && planned_writer_count != 1 {
        return Err(
            "the classified corrective Writer is not the only planned continuation".to_string(),
        );
    }
    Ok(())
}

/// Validate the immutable Task frontier while replaying a classification.
///
/// A replay may observe the classified Writer after reservation, but no other
/// live or unresolved Task may take ownership of the recovery. The failed
/// output-free source Writer remains permitted historical frontier state.
fn validate_classification_replay_task_frontier(
    attempt: &work_model::Attempt,
    review_task_id: &str,
    writer_index: usize,
) -> std::result::Result<(), String> {
    let (review_index, review_task) = attempt
        .tasks
        .iter()
        .enumerate()
        .find(|(_, task)| task.id == review_task_id)
        .ok_or_else(|| "the classified Tests Review Task is missing".to_string())?;
    if review_task.kind != TaskKind::Review
        || review_task.role != "tests"
        || review_task.status != TaskStatus::Complete
    {
        return Err("the classified Tests Review Task is not complete".to_string());
    }
    if writer_index <= review_index {
        return Err("the classified Writer is not after the Tests Review".to_string());
    }

    let mut failed_writer_count = 0usize;
    for (index, task) in attempt.tasks.iter().enumerate() {
        if index == writer_index {
            continue;
        }
        match task.status {
            TaskStatus::Complete | TaskStatus::Superseded => {}
            TaskStatus::Failed
                if task.kind == TaskKind::Write
                    && task.output.is_none()
                    && index < review_index =>
            {
                failed_writer_count += 1;
            }
            _ => {
                return Err(format!(
                    "Task {} is an unrelated live or unresolved frontier entry",
                    task.id
                ));
            }
        }
    }
    if failed_writer_count > 1 {
        return Err("the frontier contains multiple failed output-free Writers".to_string());
    }
    Ok(())
}

/// Ensure a classification targets the current unresolved host-evidence
/// frontier. A later active or evidence-waiting recovery supersedes an older
/// paused recovery for new classifications and Writer continuation.
pub(crate) fn validate_classification_recovery_scope(
    attempt: &work_model::Attempt,
    recovery_id: &str,
) -> std::result::Result<(), String> {
    let recovery_index = attempt
        .evidence_recoveries
        .iter()
        .position(|recovery| recovery.id == recovery_id)
        .ok_or_else(|| "the classified recovery is missing".to_string())?;
    if attempt
        .evidence_recoveries
        .iter()
        .skip(recovery_index + 1)
        .any(|recovery| {
            matches!(
                recovery.state,
                EvidenceRecoveryState::Reviewing | EvidenceRecoveryState::NeedsEvidence
            )
        })
    {
        return Err(
            "a later active or NeedsEvidence recovery owns the current frontier".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn code_change_operation_id(
    attempt_id: &str,
    candidate_commit: &str,
    recovery_id: &str,
    review_artifact: &str,
    review_digest: &str,
    evidence_digest: &str,
    disposition: &str,
    authorization_digest: &str,
) -> String {
    let operation_material = format!(
        "{attempt_id}\0{candidate_commit}\0{recovery_id}\0{review_artifact}\0{review_digest}\0{evidence_digest}\0{disposition}\0{authorization_digest}"
    );
    format!(
        "host-evidence-classification-{:x}",
        Sha256::digest(operation_material.as_bytes())
    )
}

fn legacy_code_change_operation_id(
    attempt_id: &str,
    candidate_commit: &str,
    recovery_id: &str,
    review_artifact: &str,
    review_digest: &str,
    evidence_digest: &str,
    authorization_digest: &str,
) -> String {
    let operation_material = format!(
        "{attempt_id}\0{candidate_commit}\0{recovery_id}\0{review_artifact}\0{review_digest}\0{evidence_digest}\0{authorization_digest}"
    );
    format!(
        "host-evidence-classification-{:x}",
        Sha256::digest(operation_material.as_bytes())
    )
}

pub(crate) fn classification_uses_legacy_identity(
    classification: &HostEvidenceClassification,
    attempt_id: &str,
    recovery: &EvidenceRecovery,
) -> bool {
    classification.operation_id
        == legacy_code_change_operation_id(
            attempt_id,
            &recovery.candidate_commit,
            &recovery.id,
            &classification.review_artifact,
            &classification.review_digest,
            &recovery.attachment.digest,
            &classification.authorization_digest,
        )
}

pub(crate) fn classification_audit_bytes(
    work_item_id: &str,
    attempt_id: &str,
    recovery: &EvidenceRecovery,
    classification: &HostEvidenceClassification,
    include_disposition: bool,
) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(&CodeChangeClassificationAudit {
        schema_version: 1,
        operation_id: &classification.operation_id,
        work_item_id,
        attempt_id,
        candidate_commit: &recovery.candidate_commit,
        recovery_id: &recovery.id,
        evidence: &recovery.attachment,
        review_artifact: &classification.review_artifact,
        review_digest: &classification.review_digest,
        disposition: include_disposition.then_some(classification.disposition.as_str()),
        authorization: &classification.authorization,
        authorization_digest: &classification.authorization_digest,
        writer_task_id: &classification.writer_task_id,
    })
    .context("serialize host-evidence classification audit")
}

/// Check candidate HEAD and cleanliness for ordinary evidence paths.
///
/// Classified recovery callers use `candidate_workspace_is_clean_at_managed`
/// when they must also prove the workspace is a registered worktree in the
/// source repository.
pub(crate) fn candidate_workspace_is_clean_at(
    project_root: &Path,
    output: &work_model::TaskOutput,
    candidate: &str,
) -> bool {
    let workspace = candidate_workspace_path(project_root, output);
    candidate_workspace_is_clean_at_path(&workspace, candidate)
}

/// Check the candidate commit and cleanliness of a classified recovery's
/// registered managed worktree.
pub(crate) fn candidate_workspace_is_clean_at_managed(
    project_root: &Path,
    output: &work_model::TaskOutput,
    candidate: &str,
) -> bool {
    let workspace = candidate_workspace_path(project_root, output);
    if crate::worktree::ensure_candidate_worktree(project_root, &workspace).is_err() {
        return false;
    }
    candidate_workspace_is_clean_at_path(&workspace, candidate)
}

fn candidate_workspace_path(project_root: &Path, output: &work_model::TaskOutput) -> PathBuf {
    let workspace = Path::new(&output.workspace_path);
    if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        project_root.join(workspace)
    }
}

fn candidate_workspace_is_clean_at_path(workspace: &Path, candidate: &str) -> bool {
    git::run_stdout(
        workspace,
        &["rev-parse", "HEAD"],
        "inspect evidence candidate",
    )
    .is_ok_and(|head| head == candidate)
        && git::run_stdout(
            workspace,
            &["status", "--porcelain", "--untracked-files=all"],
            "inspect evidence candidate cleanliness",
        )
        .is_ok_and(|status| status.is_empty())
}

pub(crate) fn validate_managed_file_path(project_root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    let managed_root = Path::new(".fluent/work/artifacts");
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || !relative_path.starts_with(managed_root)
    {
        bail!("artifact path must stay under the managed artifact root");
    }
    let components = relative_path.components().collect::<Vec<_>>();
    if components.len() <= managed_root.components().count()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("artifact path must use normal managed components");
    }
    let full_path = project_root.join(relative_path);
    let mut current = project_root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!("validated artifact component");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            bail!("artifact path must not traverse a symlink");
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            bail!("artifact path has an invalid file type");
        }
    }
    Ok(full_path)
}

pub(crate) fn parse_evidence_document(bytes: &[u8]) -> Result<EvidenceDocument> {
    if bytes.is_empty() || bytes.len() > MAX_EVIDENCE_BYTES {
        bail!("evidence document must be between 1 and {MAX_EVIDENCE_BYTES} bytes");
    }
    let document: EvidenceDocument = serde_json::from_slice(bytes)
        .context("evidence document must be valid schema-version-1 JSON")?;
    if document.schema_version != 1
        || document.producer.trim().is_empty()
        || document.check.trim().is_empty()
        || document.working_directory.trim().is_empty()
    {
        bail!("evidence document has an unsupported version or empty required field");
    }
    DateTime::parse_from_rfc3339(&document.run_at).context("evidence run_at must be RFC3339")?;
    Ok(document)
}

/// Require host evidence to record a failed check before allowing a source
/// correction classification or replay.
pub(crate) fn validate_failed_evidence_snapshot(bytes: &[u8]) -> Result<()> {
    match parse_evidence_document(bytes)?.result {
        EvidenceResult::Fail => Ok(()),
        EvidenceResult::Pass => {
            bail!("evidence snapshot result must be fail for source correction")
        }
    }
}

/// Select the one persisted Tests Review target bound to a review artifact.
///
/// Every source-correction boundary must establish this cardinality before it
/// reads or mutates the recovery. The candidate workspace and host-evidence
/// bindings are checked by `validate_code_change_candidate_binding` after this
/// target has been selected.
pub(crate) struct UniqueTestsReviewTarget<'a> {
    pub(crate) index: usize,
    pub(crate) target: &'a EvidenceReviewTarget,
}

pub(crate) fn unique_tests_review_target<'a>(
    attempt: &work_model::Attempt,
    recovery: &'a EvidenceRecovery,
    review_artifact: &str,
) -> std::result::Result<UniqueTestsReviewTarget<'a>, String> {
    let matching_targets = recovery
        .targets
        .iter()
        .enumerate()
        .filter(|(_, target)| {
            target.role == "tests"
                && target
                    .review_task_id
                    .as_ref()
                    .is_some_and(|review_task_id| {
                        attempt.tasks.iter().any(|task| {
                            task.id == *review_task_id
                                && task.kind == TaskKind::Review
                                && task.role == "tests"
                                && review_artifact_path(task).as_deref() == Some(review_artifact)
                        })
                    })
        })
        .collect::<Vec<_>>();
    if matching_targets.len() != 1 {
        return Err("classified recovery has an ambiguous Tests Review target".to_string());
    }
    let (index, target) = matching_targets[0];
    Ok(UniqueTestsReviewTarget { index, target })
}

/// Validate the immutable candidate and host-evidence bindings shared by every
/// source-correction classification boundary.
///
/// Callers must run this contract before changing the Work Item. It proves that
/// the failed Review still reads the exact candidate workspace, names the
/// persisted recovery, and carries the exact failed host-evidence snapshot.
/// Queue admission and Writer reservation use this strict source binding, while
/// classification replay uses it for no-output resumable Writers and the
/// post-correction binding below for completed Writers. Keeping both replay
/// contracts here prevents a boundary from accepting an immutable binding that
/// another boundary later rejects.
pub(crate) fn validate_code_change_candidate_binding(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    target: &EvidenceReviewTarget,
    review_task: &Task,
    source_output: &work_model::TaskOutput,
    candidate_commit: &str,
    review_artifact: &str,
    evidence_digest: &str,
) -> std::result::Result<(), String> {
    let rejected = |reason: &str| reason.to_string();
    if source_output.commit != candidate_commit
        || recovery.candidate_commit != candidate_commit
        || !candidate_workspace_is_clean_at_managed(project_root, source_output, candidate_commit)
    {
        return Err(rejected(
            "the candidate workspace no longer matches the classified commit",
        ));
    }
    validate_code_change_review_and_evidence_binding(
        project_root,
        item,
        attempt,
        recovery,
        target,
        review_task,
        source_output,
        candidate_commit,
        review_artifact,
        evidence_digest,
    )
}

/// Validate a completed classified Writer against the candidate it recorded.
///
/// Classification binds its source Review and failed evidence to the original
/// candidate commit. Once the corrective Writer has completed, that Writer's
/// output is the new candidate boundary, so replay must validate its saved
/// workspace and commit rather than require the source commit to remain HEAD.
pub(crate) fn validate_code_change_post_correction_binding(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    target: &EvidenceReviewTarget,
    review_task: &Task,
    source_output: &work_model::TaskOutput,
    corrective_output: &work_model::TaskOutput,
    review_artifact: &str,
    evidence_digest: &str,
) -> std::result::Result<(), String> {
    let rejected = |reason: &str| reason.to_string();
    if source_output.commit != recovery.candidate_commit
        || corrective_output.workspace_id != source_output.workspace_id
        || corrective_output.workspace_path != source_output.workspace_path
        || !candidate_workspace_is_clean_at_managed(
            project_root,
            corrective_output,
            &corrective_output.commit,
        )
    {
        return Err(rejected(
            "the completed corrective Writer output is not the current clean candidate",
        ));
    }
    validate_code_change_review_and_evidence_binding(
        project_root,
        item,
        attempt,
        recovery,
        target,
        review_task,
        source_output,
        &recovery.candidate_commit,
        review_artifact,
        evidence_digest,
    )
}

fn validate_code_change_review_and_evidence_binding(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    target: &EvidenceReviewTarget,
    review_task: &Task,
    source_output: &work_model::TaskOutput,
    candidate_commit: &str,
    review_artifact: &str,
    evidence_digest: &str,
) -> std::result::Result<(), String> {
    let rejected = |reason: &str| reason.to_string();
    let unique_target = unique_tests_review_target(attempt, recovery, review_artifact)?;
    if unique_target.target != target {
        return Err(rejected(
            "bound Tests review target is not uniquely selected",
        ));
    }
    if review_task.kind != TaskKind::Review
        || review_task.role != "tests"
        || review_task.status != TaskStatus::Complete
        || review_task.work_item_id != item.id
        || review_task.attempt_id.as_deref() != Some(attempt.id.as_str())
        || review_artifact_path(review_task).as_deref() != Some(review_artifact)
        || target.role != review_task.role
        || target.review_task_id.as_deref() != Some(review_task.id.as_str())
    {
        return Err(rejected("bound Tests review Task identity changed"));
    }
    let review_context = review_task
        .review_context
        .as_ref()
        .ok_or_else(|| rejected("bound Tests review has no candidate workspace context"))?;
    let matching_candidate_reads = review_task
        .workspace_access
        .reads
        .iter()
        .filter(|workspace| {
            workspace.id == source_output.workspace_id
                && workspace.path == source_output.workspace_path
        })
        .count();
    let conflicting_candidate_reads = review_task.workspace_access.reads.iter().any(|workspace| {
        workspace.id == source_output.workspace_id && workspace.path != source_output.workspace_path
    });
    if review_context.candidate_commit != candidate_commit
        || review_context.candidate_workspace_id != source_output.workspace_id
        || review_context.candidate_workspace_path != source_output.workspace_path
        || matching_candidate_reads != 1
        || conflicting_candidate_reads
    {
        return Err(rejected(
            "bound Tests review is not attached to the candidate workspace",
        ));
    }
    let evidence_context = review_task
        .evidence_review_context
        .as_ref()
        .ok_or_else(|| rejected("bound Tests review has no host-evidence context"))?;
    if evidence_context.recovery_id != recovery.id
        || evidence_context.candidate_commit != candidate_commit
        || evidence_context.attachment != recovery.attachment
        || evidence_context.prior_review_artifact != target.prior_review_artifact
    {
        return Err(rejected("bound Tests review host-evidence context changed"));
    }
    if recovery.attachment.digest != evidence_digest {
        return Err(rejected("evidence snapshot binding changed"));
    }
    let evidence_ref = ArtifactRef {
        producer_id: "host-evidence".to_string(),
        path: recovery.attachment.snapshot_path.clone(),
    };
    if review_task
        .input_artifacts
        .iter()
        .filter(|input| *input == &evidence_ref)
        .count()
        != 1
        || review_task
            .input_artifacts
            .iter()
            .any(|input| input.path == evidence_ref.path && input != &evidence_ref)
    {
        return Err(rejected("bound Tests review lost its evidence input"));
    }
    let expected_snapshot = snapshot_path(&item.id, &attempt.id, &recovery.attachment.digest)
        .map_err(|error| rejected(&error.to_string()))?;
    if recovery.attachment.snapshot_path != expected_snapshot {
        return Err(rejected("evidence snapshot binding changed"));
    }
    let evidence_path =
        validate_managed_file_path(project_root, &recovery.attachment.snapshot_path).map_err(
            |error| {
                rejected(&format!(
                    "evidence snapshot is not a managed regular file: {error}"
                ))
            },
        )?;
    let evidence_bytes = fs::read(&evidence_path)
        .map_err(|error| rejected(&format!("cannot read bound evidence snapshot: {error}")))?;
    if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest {
        return Err(rejected("evidence snapshot bytes changed"));
    }
    validate_failed_evidence_snapshot(&evidence_bytes)
        .map_err(|error| rejected(&format!("invalid evidence snapshot: {error}")))?;
    Ok(())
}

pub(crate) fn snapshot_path(work: &str, attempt: &str, digest: &str) -> Result<String> {
    let hex = digest
        .strip_prefix("sha256:")
        .context("invalid evidence digest")?;
    Ok(format!(
        ".fluent/work/artifacts/{work}/{attempt}/host-evidence/{hex}.json"
    ))
}

pub(crate) fn review_artifact_path(task: &Task) -> Option<String> {
    task.artifact_area
        .as_ref()
        .map(|area| format!("{}/review.md", area.path))
}

#[cfg(test)]
mod tests {
    use super::{EvidenceResult, parse_evidence_document, validate_failed_evidence_snapshot};

    #[test]
    fn failed_evidence_snapshot_rejects_pass_result() {
        let passing = br#"{"schema_version":1,"producer":"host","check":"cargo test","working_directory":".","result":"pass","run_at":"2026-09-02T00:00:00Z","output":"ok"}"#;

        assert_eq!(
            parse_evidence_document(passing).unwrap().result,
            EvidenceResult::Pass
        );
        let error = validate_failed_evidence_snapshot(passing).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("evidence snapshot result must be fail")
        );
    }
}

pub(crate) fn writer_completion_matrix_ref(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
) -> Option<ArtifactRef> {
    // A planning-reference recovery can leave several immutable matrices in
    // the Attempt. Select only the one whose task identity matches this
    // Writer; otherwise an older recovery could make a valid current row look
    // absent and produce an ineligible handoff.
    for artifact in attempt.artifacts.iter().rev() {
        if !artifact.path.ends_with("/writer-completion.json") {
            continue;
        }
        if artifact.producer_id == "writer-completion-repair"
            && !artifact
                .path
                .contains(&format!("/writer-completion-repairs/{}/", writer.id))
        {
            continue;
        }
        if artifact.producer_id != "writer-completion-repair"
            && artifact.producer_id != "planning-reference-recovery"
        {
            continue;
        }
        let Ok(bytes) = fs::read(project_root.join(&artifact.path)) else {
            // An unreadable recovery artifact cannot be selected as the
            // authoritative matrix; the task-owned fallback below still gives
            // the caller a deterministic path to diagnose.
            continue;
        };
        if serde_json::from_slice::<work_model::WriterCompletionMatrix>(&bytes)
            .is_ok_and(|matrix| matrix.task_id == writer.id)
        {
            return Some(artifact.clone());
        }
    }
    writer.artifact_area.as_ref().map(|area| ArtifactRef {
        producer_id: writer.id.clone(),
        path: format!(
            "{}/{}",
            area.path,
            crate::recovery_contracts::WRITER_COMPLETION_MATRIX_FILE
        ),
    })
}
