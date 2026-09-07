//! Immutable, host-produced evidence attached to a reviewed Attempt.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::content::ContentResolver;
use crate::plan_command::extract_plan_command_candidates;
use crate::recovery_contracts::parse_evidence_document;
pub use crate::recovery_contracts::{EvidenceDocument, EvidenceResult};
use crate::review::{self, Verdict};
use crate::work_model::{
    self, ArtifactRef, BoundRecheckArtifact, EvidenceAttachment, EvidenceRecovery,
    EvidenceRecoveryState, EvidenceReviewContext, EvidenceReviewTarget, HostEvidenceClassification,
    PlanHostEvidenceBinding, Task, TaskKind, TaskStatus, WorkModelError, WorkModelStore,
    WriterCompletionMatrix, WriterCompletionRequirementKind, WriterCompletionRow,
    WriterCompletionState, WriterVerificationClaim, WriterVerificationResult, work_artifact_path,
};

const MAX_FOCUSED_COMMAND_BYTES: usize = 4096;
const MAX_FOCUSED_TIMEOUT_SECONDS: u64 = 3600;
const PLAN_HOST_EVIDENCE_PRODUCER: &str = "plan-host-evidence";
const PLAN_HOST_EVIDENCE_SCHEMA_VERSION: u32 = 1;
const CODE_CHANGE_DISPOSITION: &str = "code-change";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedCommandRecheckEvidenceBinding {
    schema_version: u32,
    candidate_commit: String,
    recheck_task_id: String,
    source_results: BoundRecheckArtifact,
    recheck_artifacts: Vec<BoundRecheckArtifact>,
    command_index: usize,
    command: String,
    /// The command the authorized host actually ran. Older bindings used the
    /// recheck command for both fields, so absence retains that interpretation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_check: Option<String>,
    working_directory: String,
}

#[derive(Debug, Clone)]
struct ValidatedRecheckFrontier {
    prior_tests_review: Task,
    binding: FailedCommandRecheckEvidenceBinding,
    binding_path: String,
    binding_bytes: Vec<u8>,
    correction_artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateGateEvidenceBinding {
    schema_version: u32,
    candidate_commit: String,
    gate_artifact: BoundRecheckArtifact,
    gate_evidence: crate::candidate_gate::CandidateGateEvidence,
    command: String,
    working_directory: String,
}

#[derive(Debug, Clone)]
struct ValidatedCandidateGateFrontier {
    prior_tests_review: Task,
    binding_path: String,
    binding_bytes: Vec<u8>,
}

/// A pending Plan row whose resolved command must run on an authorized host.
/// The Writer cannot complete this row because it does not have that host's
/// credentials or runtime; the operator binds its result to the unchanged
/// candidate instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanHostEvidenceAction {
    pub plan_row: String,
    pub command: String,
    pub working_directory: String,
    pub candidate_commit: String,
    pub matrix_artifact: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPlanCommand {
    command: String,
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedEvidenceRun {
    pub task_id: String,
    pub command: String,
    pub working_directory: String,
}

/// Returned by the public classification boundary after it schedules (or
/// replays) one corrective Writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChangeClassification {
    pub writer_task_id: String,
    pub audit_artifact: String,
    pub disposition: String,
}

/// Classify one paused, evidence-needed Tests Review as an authorized source
/// correction for the unchanged candidate.
///
/// The boundary validates the persisted review task, its saved review digest,
/// the candidate-bound evidence snapshot, and the complete failed-review
/// frontier before it publishes an audit or reopens the Attempt. Repeating the
/// exact review/evidence/authorization request returns the stored Writer and
/// audit identifiers without creating another Task; a different request is
/// rejected.
pub fn classify_for_code_change(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    candidate_commit: &str,
    review_artifact: &str,
    evidence_digest: &str,
    authorization: &str,
) -> Result<CodeChangeClassification> {
    let authorization = authorization.trim();
    anyhow::ensure!(
        !authorization.is_empty() && authorization.len() <= MAX_FOCUSED_COMMAND_BYTES,
        "provide a non-empty --resolution up to {MAX_FOCUSED_COMMAND_BYTES} bytes"
    );
    let authorization_digest = format!("sha256:{:x}", Sha256::digest(authorization.as_bytes()));
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire land lock before classifying host evidence")?;
    store.mutate_work_item_from_external(
        work_item_id,
        "classify candidate-bound host evidence for source correction",
        Some(review_artifact.to_string()),
        |item| {
        let rejected_for = |reason: &str| WorkModelError::HostEvidenceRecoveryIneligible {
            attempt_id: attempt_id.to_string(),
            reason: reason.to_string(),
        };
        let rejected = || WorkModelError::HostEvidenceRecoveryIneligible {
            attempt_id: attempt_id.to_string(),
            reason: "the supplied classification does not match a paused candidate-bound host-evidence recovery".to_string(),
        };
        let attempt = item.attempts.iter().find(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| rejected_for("Attempt is not present"))?;
        // Resolve the recovery by its complete persisted frontier before reading
        // any caller-supplied path. The same attachment may legitimately back
        // multiple later Reviews, so candidate/evidence alone is insufficient.
        let recovery = attempt.evidence_recoveries.iter().rev().find(|recovery| {
            recovery.candidate_commit == candidate_commit
                && recovery.attachment.digest == evidence_digest
                && recovery.targets.iter().any(|target| {
                    target.role == "tests"
                        && target.review_task_id.as_ref().is_some_and(|task_id| {
                            attempt.tasks.iter().any(|task| {
                                task.id == *task_id
                                    && review_artifact_path(task).as_deref()
                                        == Some(review_artifact)
                            })
                        })
                })
        }).ok_or_else(|| rejected_for("no recovery matches candidate, evidence, and review frontier"))?;
        let target = crate::recovery_contracts::unique_tests_review_target(
            attempt,
            recovery,
            review_artifact,
        )
        .map_err(|reason| rejected_for(&reason))?
        .target;
        let review_task_id = target.review_task_id.as_deref()
            .ok_or_else(|| rejected_for("recovery target has no review Task identity"))?;
        let review_task = attempt.tasks.iter().find(|task| {
            task.id == review_task_id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && review_artifact_path(task).as_deref() == Some(review_artifact)
                && task.evidence_review_context.as_ref().is_some_and(|context| {
                    context.recovery_id == recovery.id
                        && context.candidate_commit == candidate_commit
                        && context.attachment == recovery.attachment
                        && context.prior_review_artifact == target.prior_review_artifact
                })
        }).ok_or_else(|| rejected_for("bound Tests review Task is missing or mismatched"))?;
        // The path has now been proven to be the exact persisted Review task.
        // Reject absolute, parent-relative, symlinked, or non-regular paths
        // before opening external bytes while the land/model locks are held.
        let review_path = validate_managed_file_path(project_root, review_artifact)
            .map_err(|error| WorkModelError::HostEvidenceRecoveryIneligible {
                attempt_id: attempt_id.to_string(),
                reason: format!("review artifact is not a managed regular file: {error}"),
            })?;
        let review_bytes = fs::read(&review_path).map_err(|error| WorkModelError::HostEvidenceRecoveryIneligible {
            attempt_id: attempt_id.to_string(),
            reason: format!("cannot read bound review artifact: {error}"),
        })?;
        let review_digest = format!("sha256:{:x}", Sha256::digest(&review_bytes));
        let review_text = std::str::from_utf8(&review_bytes).map_err(|_| rejected())?;
        let operation = crate::work_model::RecoveryOperationRequest::new(
            "host-evidence-code-change",
            work_item_id,
            attempt_id,
            Some(candidate_commit.to_string()),
            Some(review_task.id.clone()),
            Some(evidence_digest.to_string()),
            None,
            Some(authorization.to_string()),
            Some(format!(
                "sha256:{:x}",
                Sha256::digest(
                    format!("{review_artifact}\0{review_digest}\0{evidence_digest}").as_bytes()
                )
            )),
        );
        let _ = item.validate_recovery_operation(project_root, &operation)?;
        let disposition = review::extract_evidence_disposition(review_text);
        let legacy_review = target.review_digest.is_none();
        let review_matches_saved_digest = target
            .review_digest
            .as_deref()
            .is_some_and(|saved| saved == review_digest);
        let repeats_stored_classification = recovery
            .code_change_classification
            .as_ref()
            .is_some_and(|classification| {
                classification.matches_request(
                    review_artifact,
                    &review_digest,
                    authorization,
                    &authorization_digest,
                )
            });
        let legacy_review_is_evidence_needed = legacy_review
            && (disposition == Some(review::EvidenceDisposition::EvidenceNeeded)
                || review::has_unqualified_evidence_needed_disposition(review_text));
        if (!legacy_review && !review_matches_saved_digest)
            || (legacy_review && !legacy_review_is_evidence_needed)
            || review::extract_verdict(review_text) != Verdict::Fail
            || (!legacy_review
                && disposition != Some(review::EvidenceDisposition::EvidenceNeeded)
                && !repeats_stored_classification)
        {
            return Err(rejected_for("bound review no longer requests evidence-needed"));
        }

        if let Some(prior) = recovery.code_change_classification.as_ref() {
            if prior.disposition != CODE_CHANGE_DISPOSITION {
                return Err(rejected_for("stored classification uses an unsupported disposition"));
            }
            if prior.matches_request(
                review_artifact,
                &review_digest,
                authorization,
                &authorization_digest,
            ) {
                let needs_target_digest_migration = target.review_digest.is_none();
                let recovery_id = recovery.id.clone();
                let review_task_id = review_task.id.clone();
                let writer_task_id = prior.writer_task_id.clone();
                let audit_artifact = prior.audit_artifact.path.clone();
                let disposition = prior.disposition.clone();
                crate::recovery_contracts::validate_code_change_classification_replay(
                    project_root,
                    item,
                    attempt,
                    recovery,
                    prior,
                    &review_task_id,
                )
                .map_err(|reason| rejected_for(&reason))?;
                if needs_target_digest_migration {
                    let attempt = item
                        .attempts
                        .iter_mut()
                        .find(|attempt| attempt.id == attempt_id)
                        .ok_or_else(|| rejected_for("Attempt is not present"))?;
                    let recovery = attempt
                        .evidence_recoveries
                        .iter_mut()
                        .find(|recovery| recovery.id == recovery_id)
                        .ok_or_else(|| rejected_for("classified recovery is missing"))?;
                    let target = recovery
                        .targets
                        .iter_mut()
                        .find(|target| {
                            target.review_task_id.as_deref() == Some(review_task_id.as_str())
                        })
                        .ok_or_else(|| rejected_for("classified recovery target is missing"))?;
                    target.review_digest = Some(review_digest.clone());
                }
                return Ok(CodeChangeClassification {
                    writer_task_id,
                    audit_artifact,
                    disposition,
                });
            }
            return Err(rejected_for("authorization differs from the stored classification"));
        }
        let writer_task = attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .ok_or_else(rejected)?;
        let writer = writer_task.output.as_ref().ok_or_else(rejected)?;
        crate::recovery_contracts::validate_code_change_candidate_binding(
            project_root,
            item,
            attempt,
            recovery,
            target,
            review_task,
            writer,
            candidate_commit,
            review_artifact,
            evidence_digest,
        )
        .map_err(|reason| rejected_for(&reason))?;
        let review_task_id = review_task.id.clone();
        crate::recovery_contracts::validate_classification_task_frontier(
            attempt,
            &review_task_id,
            None,
        )
        .map_err(|reason| rejected_for(&reason))?;
        if item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id
                && candidate.candidate_commit == recovery.candidate_commit
        }) {
            return Err(rejected_for("a Merge Candidate already owns this candidate"));
        }
        if recovery.state != EvidenceRecoveryState::NeedsEvidence {
            return Err(rejected());
        }
        crate::recovery_contracts::validate_classification_recovery_scope(attempt, &recovery.id)
            .map_err(|reason| rejected_for(&reason))?;
        // Plan-row host evidence is created before the first review. A failed
        // targeted Tests Review can therefore leave older persisted Attempts
        // with `review_state: not-reviewed`, even though the review and its
        // candidate-bound evidence are complete. Admit only the exact
        // binding produced by the Plan-row attachment path; an ordinary
        // evidence recovery with the same coarse state must remain rejected.
        let is_plan_row_recovery = is_plan_row_evidence_recovery(
            project_root,
            item,
            attempt,
            recovery,
            target,
            writer_task,
            candidate_commit,
            evidence_digest,
        )
        .is_some();
        if item.attempts.last().map(|attempt| attempt.id.as_str()) != Some(attempt_id)
            || attempt.status != work_model::AttemptStatus::NeedsUser
            || attempt.pause_kind != Some(work_model::PauseKind::Uncertain)
            || (attempt.review_state != Some(work_model::AttemptReviewState::Failed)
                && !is_plan_row_recovery)
            || attempt.tasks.iter().any(|task| task.status == TaskStatus::Executing)
        {
            return Err(rejected_for("Attempt is not the current paused failed-review frontier"));
        }
        let selected = validate_evidence_frontier(
            project_root,
            item,
            attempt_id,
            candidate_commit,
            &[review_artifact.to_string()],
        )
        .map_err(|error| WorkModelError::HostEvidenceRecoveryIneligible {
            attempt_id: attempt_id.to_string(),
            reason: format!("failed-review frontier is not eligible: {error}"),
        })?;
        if selected.len() != 1
            || selected[0].id != review_task.id
            || selected[0].role != "tests"
        {
            return Err(rejected_for("shared failed-review frontier selected a different Task"));
        }
        let attachment = recovery.attachment.clone();
        let recovery_id = recovery.id.clone();
        let disposition = CODE_CHANGE_DISPOSITION;
        let operation_id = crate::recovery_contracts::code_change_operation_id(
            attempt_id,
            candidate_commit,
            &recovery.id,
            review_artifact,
            &review_digest,
            evidence_digest,
            disposition,
            &authorization_digest,
        );
        let audit_path = work_model::work_artifact_path(work_item_id, attempt_id, &operation_id);
        let audit_artifact = ArtifactRef { producer_id: operation_id.clone(), path: format!("{audit_path}/classification.json") };
        let writer_task_id = format!("{attempt_id}-write-{}", attempt.tasks.iter().filter(|task| task.kind == TaskKind::Write).count() + 1);
        // A staged audit from an interrupted earlier attempt may be reused only
        // when its deterministic bytes match this operation. Reject a collision
        // before mutating the Work Item; the normal path publishes after the
        // model transaction commits and reconciles a missing file on retry.
        let pending_audit = crate::recovery_contracts::CodeChangeClassificationAudit {
            schema_version: 1,
            operation_id: &operation_id,
            work_item_id,
            attempt_id,
            candidate_commit,
            recovery_id: &recovery_id,
            evidence: &attachment,
            review_artifact,
            review_digest: &review_digest,
            disposition: Some(disposition),
            authorization,
            authorization_digest: &authorization_digest,
            writer_task_id: &writer_task_id,
        };
        let pending_audit_bytes = serde_json::to_vec_pretty(&pending_audit).map_err(|_| rejected())?;
        let audit_file = project_root.join(&audit_artifact.path);
        match fs::symlink_metadata(&audit_file) {
            Ok(_) => {
                let audit_file = crate::recovery_contracts::validate_managed_file_path(project_root, &audit_artifact.path)
                    .map_err(|error| rejected_for(&format!("classification audit is invalid: {error}")))?;
                let existing = fs::read(&audit_file)
                    .map_err(|error| rejected_for(&format!("cannot read classification audit: {error}")))?;
                if existing != pending_audit_bytes {
                    return Err(rejected_for("classification audit already contains conflicting bytes"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(rejected_for(&format!("cannot inspect classification audit: {error}")));
            }
        }
        {
            let attempt = item.attempts.iter_mut().find(|attempt| attempt.id == attempt_id).expect("validated Attempt remains present");
            let recovery = attempt.evidence_recoveries.iter_mut().find(|recovery| recovery.id == recovery_id).expect("validated recovery remains present");
            recovery.state = EvidenceRecoveryState::CodeChange;
            if let Some(target) = recovery.targets.iter_mut().find(|target| {
                target.review_task_id.as_deref() == Some(review_task_id.as_str())
            }) {
                target.review_digest = Some(review_digest.clone());
            }
            recovery.code_change_classification = Some(HostEvidenceClassification {
                operation_id, disposition: disposition.to_string(), review_artifact: review_artifact.to_string(), review_digest, authorization: authorization.to_string(), authorization_digest,
                audit_artifact: audit_artifact.clone(), writer_task_id: writer_task_id.clone(), classified_at: work_model::now_iso8601(),
            });
            if !attempt.artifacts.contains(&audit_artifact) { attempt.artifacts.push(audit_artifact.clone()); }
            // The Plan-row attachment path predates the targeted review and
            // some persisted Attempts retained `not-reviewed` at the pause.
            // Once the operator classifies that completed failed review as a
            // source correction, record the failure before reopening so the
            // corrective Writer's normal admission checks see the same review
            // outcome as newer Attempts.
            if is_plan_row_recovery {
                attempt.review_state = Some(work_model::AttemptReviewState::Failed);
            }
            work_model::reopen_attempt(attempt);
        }
        let task_id = item.add_next_write_round(attempt_id, vec![
            ArtifactRef { producer_id: review_task_id, path: review_artifact.to_string() },
            ArtifactRef { producer_id: "host-evidence".to_string(), path: attachment.snapshot_path },
            audit_artifact.clone(),
        ]).map_err(|_| rejected())?;
        if task_id != writer_task_id { return Err(rejected()); }
        let attempt = item
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| rejected_for("Attempt is not present after recovery"))?;
        let next_action = crate::work_status::derive_attempt_recovery_next_action(
            Some(attempt),
            Some(project_root),
        );
        attempt.record_recovery_operation(&operation, next_action)?;
        Ok(CodeChangeClassification { writer_task_id: task_id, audit_artifact: audit_artifact.path, disposition: disposition.to_string() })
        },
    )
    .map_err(Into::into)
    .and_then(|classification| {
        reconcile_code_change_classification_audit(
            project_root,
            store,
            work_item_id,
            attempt_id,
            &classification.writer_task_id,
        )?;
        Ok(classification)
    })
}

/// Recognize the legacy Plan-row pause shape that can be safely admitted to
/// source-correction recovery.
///
/// Plan-row evidence is attached before the first review is scheduled. Older
/// Fluent versions could finish the targeted Tests Review and persist
/// `NeedsUser`/`Uncertain` with `review_state: not-reviewed`, even though the
/// immutable binding, failed evidence, and failed Review were all present.
/// This helper proves that the recovery really came from that attachment path
/// instead of weakening the ordinary failed-review check for unrelated
/// evidence recoveries.
fn is_plan_row_evidence_recovery(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    target: &EvidenceReviewTarget,
    writer_task: &Task,
    candidate_commit: &str,
    evidence_digest: &str,
) -> Option<()> {
    if attempt.review_state != Some(work_model::AttemptReviewState::NotReviewed)
        || recovery.state != EvidenceRecoveryState::NeedsEvidence
        || recovery.candidate_commit != candidate_commit
        || recovery.attachment.digest != evidence_digest
        || target.role != "tests"
        || target.review_task_id.is_none()
    {
        return None;
    }
    if recovery
        .targets
        .iter()
        .filter(|candidate| {
            candidate.role == target.role
                && candidate.prior_review_artifact == target.prior_review_artifact
        })
        .count()
        != 1
    {
        return None;
    }
    if attempt
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.producer_id == PLAN_HOST_EVIDENCE_PRODUCER
                && artifact.path == target.prior_review_artifact
        })
        .count()
        != 1
    {
        return None;
    }
    let binding_path =
        validate_managed_file_path(project_root, &target.prior_review_artifact).ok()?;
    let binding_bytes = fs::read(binding_path).ok()?;
    let binding = serde_json::from_slice::<PlanHostEvidenceBinding>(&binding_bytes).ok()?;
    let output = writer_task.output.as_ref()?;
    if binding.schema_version != PLAN_HOST_EVIDENCE_SCHEMA_VERSION
        || binding.work_item_id != item.id
        || binding.attempt_id != attempt.id
        || binding.writer_task_id != writer_task.id
        || binding.candidate_commit != candidate_commit
        || binding.evidence.path != recovery.attachment.snapshot_path
        || binding.evidence.digest != recovery.attachment.digest
        || binding.evidence.digest != evidence_digest
        || binding.result != EvidenceResult::Fail
        || output.commit != candidate_commit
    {
        return None;
    }

    let Some(source_matrix) = writer_completion_matrix_ref(project_root, attempt, writer_task)
    else {
        return None;
    };
    if source_matrix.path != binding.source_matrix.path {
        return None;
    }
    let source_matrix_path = validate_managed_file_path(project_root, &source_matrix.path).ok()?;
    let source_matrix_bytes = fs::read(source_matrix_path).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&source_matrix_bytes)) != binding.source_matrix.digest
    {
        return None;
    }
    let source_matrix =
        serde_json::from_slice::<WriterCompletionMatrix>(&source_matrix_bytes).ok()?;
    let row = source_matrix
        .rows
        .iter()
        .find(|row| row.id == binding.plan_row)?;
    if source_matrix.task_id != writer_task.id
        || row.kind != WriterCompletionRequirementKind::Plan
        || row.state != WriterCompletionState::Pending
        || row.requirement != binding.requirement
        || !plan_command_binding_matches(row, &binding.command, binding.command_source.as_deref())
    {
        return None;
    }

    let evidence_path = validate_managed_file_path(project_root, &binding.evidence.path).ok()?;
    let evidence_bytes = fs::read(evidence_path).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != binding.evidence.digest {
        return None;
    }
    let evidence = parse_evidence_document(&evidence_bytes).ok()?;
    (evidence.result == EvidenceResult::Fail
        && evidence.check == binding.command
        && evidence.working_directory == binding.working_directory)
        .then_some(())
}

/// Reconcile the durable audit after the Work Item mutation commits.
///
/// The model is the source of truth: classification state and the corrective
/// Writer are persisted first, then this deterministic artifact is published.
/// A retry of the same operation recreates a missing audit byte-for-byte and
/// rejects any conflicting bytes, so a crash cannot produce an audit that is
/// reported as successful without a corresponding model record.
fn reconcile_code_change_classification_audit(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    writer_task_id: &str,
) -> Result<()> {
    let item = store
        .read_work_item(work_item_id)
        .with_context(|| format!("read classified Work Item {work_item_id}"))?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .with_context(|| format!("classified Attempt {attempt_id} is missing"))?;
    let recovery = attempt
        .evidence_recoveries
        .iter()
        .find(|recovery| {
            recovery
                .code_change_classification
                .as_ref()
                .is_some_and(|classification| classification.writer_task_id == writer_task_id)
        })
        .context("classified recovery is missing")?;
    let classification = recovery
        .code_change_classification
        .as_ref()
        .expect("classification selected above");
    anyhow::ensure!(
        classification.disposition == CODE_CHANGE_DISPOSITION,
        "classified recovery uses an unsupported disposition"
    );
    let expected_operation_id = crate::recovery_contracts::code_change_operation_id(
        attempt_id,
        &recovery.candidate_commit,
        &recovery.id,
        &classification.review_artifact,
        &classification.review_digest,
        &recovery.attachment.digest,
        &classification.disposition,
        &classification.authorization_digest,
    );
    let legacy_identity = crate::recovery_contracts::classification_uses_legacy_identity(
        classification,
        attempt_id,
        recovery,
    );
    anyhow::ensure!(
        classification.operation_id == expected_operation_id || legacy_identity,
        "classified recovery operation identity changed"
    );
    let expected_audit_artifact = ArtifactRef {
        producer_id: classification.operation_id.clone(),
        path: work_artifact_path(
            work_item_id,
            attempt_id,
            &format!("{}/classification.json", classification.operation_id),
        ),
    };
    anyhow::ensure!(
        classification.audit_artifact == expected_audit_artifact,
        "classified recovery audit binding changed"
    );
    let audit_bytes = crate::recovery_contracts::classification_audit_bytes(
        work_item_id,
        attempt_id,
        recovery,
        classification,
        !legacy_identity,
    )?;
    let audit_file = project_root.join(&classification.audit_artifact.path);
    let audit_path = match fs::symlink_metadata(&audit_file) {
        Ok(_) => validate_managed_file_path(project_root, &classification.audit_artifact.path)
            .with_context(|| format!("validate classification audit {}", audit_file.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return publish_snapshot(
                project_root,
                &classification.audit_artifact.path,
                &audit_bytes,
            );
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect classification audit {}", audit_file.display()));
        }
    };
    match fs::read(&audit_path) {
        Ok(existing) if existing == audit_bytes => Ok(()),
        Ok(_) => bail!("classification audit bytes changed"),
        Err(error) => Err(error)
            .with_context(|| format!("read classification audit {}", audit_path.display())),
    }
}

/// Validate a caller-supplied managed artifact path without opening it.
/// Every component must remain under `.fluent/work/artifacts`, no component may
/// be a symlink, and the final entry must be a regular file.
pub(crate) fn validate_managed_file_path(project_root: &Path, relative: &str) -> Result<PathBuf> {
    crate::recovery_contracts::validate_managed_file_path(project_root, relative)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateGateEvidenceAction {
    pub command: String,
    pub gate_artifact: String,
    pub attach_command: String,
}

#[allow(clippy::too_many_arguments)]
pub fn run_focused(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    review_artifact: &str,
    command_override: Option<&str>,
    working_directory_override: Option<&str>,
    timeout_seconds: u64,
    resolver: &ContentResolver,
) -> Result<FocusedEvidenceRun> {
    anyhow::ensure!(
        (1..=MAX_FOCUSED_TIMEOUT_SECONDS).contains(&timeout_seconds),
        "focused evidence timeout must be between 1 and {MAX_FOCUSED_TIMEOUT_SECONDS} seconds"
    );
    let item = store
        .read_work_item(work_item_id)
        .with_context(|| format!("read Work Item {work_item_id}"))?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .with_context(|| format!("Attempt {attempt_id} not found"))?;
    let candidate_commit = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.clone())
        .context("focused evidence requires a completed Writer candidate")?;
    let selected = validate_evidence_frontier(
        project_root,
        &item,
        attempt_id,
        &candidate_commit,
        &[review_artifact.to_string()],
    )?;
    let review_task = selected
        .first()
        .context("focused evidence requires one failed review")?;
    anyhow::ensure!(
        review_task.role == "tests",
        "focused evidence can target only a failed Tests Review"
    );
    let review_path = project_root.join(review_artifact);
    let review_content = fs::read_to_string(&review_path)
        .with_context(|| format!("read failed review {}", review_path.display()))?;
    let declared = review::extract_focused_evidence_request(&review_content);
    let command = resolve_requested_value(
        "command",
        declared.as_ref().map(|request| request.command.as_str()),
        command_override,
    )?;
    validate_focused_command(&command)?;
    let working_directory = resolve_requested_value(
        "working directory",
        declared
            .as_ref()
            .map(|request| request.working_directory.as_str()),
        working_directory_override,
    )?;

    let candidate_workspace = review_task
        .review_context
        .as_ref()
        .map(|context| PathBuf::from(&context.candidate_workspace_path))
        .context("failed Tests Review has no candidate workspace")?;
    let candidate_workspace = if candidate_workspace.is_absolute() {
        candidate_workspace
    } else {
        project_root.join(candidate_workspace)
    };
    let candidate_workspace = canonical_candidate_workspace(&candidate_workspace)?;
    resolve_working_directory(&candidate_workspace, &working_directory)?;
    let checker = work_model::VerificationChecker {
        schema_version: 1,
        command: command.clone(),
        working_directory: working_directory.clone(),
        timeout_seconds,
        sandbox_digest: crate::verification::sandbox_digest(resolver)?,
    };
    let context = work_model::VerificationContext {
        candidate_commit: candidate_commit.clone(),
        checker_digest: crate::verification::digest(&serde_json::to_vec(&checker)?),
        checker,
        review_artifact: BoundRecheckArtifact {
            path: review_artifact.to_string(),
            digest: crate::verification::digest(review_content.as_bytes()),
        },
    };
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))?;
    let task_id = store.mutate_work_item(work_item_id, |item| {
        let prior = item
            .attempts
            .iter()
            .find(|attempt| attempt.id == attempt_id)
            .and_then(|attempt| {
                attempt.tasks.iter().find(|task| {
                    task.kind == TaskKind::Verification
                        && task.verification_context.as_ref() == Some(&context)
                })
            });
        if let Some(prior) = prior {
            return Ok(prior.id.clone());
        }
        let selected = validate_evidence_frontier(
            project_root,
            item,
            attempt_id,
            &candidate_commit,
            &[review_artifact.to_string()],
        )?;
        let source = &selected[0];
        let attempt = item
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .unwrap();
        let index = attempt
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Verification)
            .count()
            + 1;
        let task_id = format!("{attempt_id}-verification-{index}");
        attempt.tasks.push(Task {
            id: task_id.clone(),
            kind: TaskKind::Verification,
            status: TaskStatus::Planned,
            role: "verification".into(),
            instructions: None,
            work_item_id: work_item_id.into(),
            attempt_id: Some(attempt_id.into()),
            workspace_access: source.workspace_access.clone(),
            artifact_area: Some(work_model::TaskArtifactArea {
                path: work_artifact_path(work_item_id, attempt_id, &task_id),
            }),
            review_context: source.review_context.clone(),
            evidence_review_context: None,
            verification_context: Some(context.clone()),
            input_artifacts: vec![ArtifactRef {
                producer_id: source.id.clone(),
                path: review_artifact.into(),
            }],
            depends_on: Some(source.id.clone()),
            output: None,
            created_at: Some(work_model::now_iso8601()),
            started_at: None,
            completed_at: None,
        });
        attempt.status = work_model::AttemptStatus::Reviewing;
        attempt.pause_kind = None;
        attempt.pause_recovery = None;
        attempt.pause_record = None;
        attempt.completed_at = None;
        Ok(task_id)
    })?;
    Ok(FocusedEvidenceRun {
        task_id,
        command,
        working_directory,
    })
}

/// Return the exact pending Plan-row command that can be completed with
/// candidate-bound host evidence. This is intentionally read-only: it is used
/// to make a Writer-completion handoff name a recovery that is valid for the
/// persisted candidate.
pub(crate) fn pending_plan_host_evidence_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<PlanHostEvidenceAction> {
    if attempt
        .writer_completion_contract
        .as_ref()
        .is_some_and(|contract| {
            contract
                .requirements
                .iter()
                .any(|row| row.responsibility.is_some())
        })
    {
        return pending_owned_host_resume_action(project_root, attempt)
            .ok()
            .flatten();
    }
    if attempt.status != work_model::AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(work_model::PauseKind::WriterCompletion)
        || attempt.review_state != Some(work_model::AttemptReviewState::NotReviewed)
    {
        return None;
    }
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)?;
    let output = writer.output.as_ref()?;
    if !candidate_workspace_is_clean_at(project_root, output, &output.commit) {
        return None;
    }
    let matrix_ref = writer_completion_matrix_ref(project_root, attempt, writer)?;
    let bytes = fs::read(project_root.join(&matrix_ref.path)).ok()?;
    let matrix: WriterCompletionMatrix = serde_json::from_slice(&bytes).ok()?;
    (matrix.task_id == writer.id).then_some(())?;
    if let Some(contract) = attempt.writer_completion_contract.as_ref() {
        let findings =
            crate::recovery_contracts::writer_completion_findings(project_root, writer).ok()?;
        matrix
            .status_against(contract, &writer.id, &findings)
            .ok()?;
    }
    matrix.rows.into_iter().find_map(|row| {
        if row.kind != WriterCompletionRequirementKind::Plan
            || row.state != WriterCompletionState::Pending
        {
            return None;
        }
        let command = resolve_plan_row_command(&row)?;
        Some(PlanHostEvidenceAction {
            plan_row: row.id,
            command,
            working_directory: row
                .command
                .as_ref()
                .map(|command| command.working_directory.clone())
                .unwrap_or_else(|| ".".into()),
            candidate_commit: output.commit.clone(),
            matrix_artifact: matrix_ref.path.clone(),
        })
    })
}

/// Use the same exact host handoff for status, direct resume, and queue claim.
pub(crate) fn pending_owned_host_resume_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<PlanHostEvidenceAction>> {
    if attempt.status != work_model::AttemptStatus::NeedsUser
        || !matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::WriterCompletion | work_model::PauseKind::Interrupted)
        )
    {
        return Ok(None);
    }
    if attempt.pause_kind == Some(work_model::PauseKind::Interrupted) {
        // Ordinary initial Writers and unfinished continuations keep their normal
        // retry path. Recognize host-only work before imposing its prerequisites.
        if crate::lifecycle_recovery::interrupted_plan_host_writer(attempt).is_none() {
            return Ok(None);
        }
        let Some(contract) = attempt
            .writer_completion_contract
            .as_ref()
            .filter(|contract| {
                contract.requirements.iter().any(|row| {
                    row.responsibility == Some(work_model::CompletionResponsibility::Host)
                })
            })
        else {
            return Ok(None);
        };
        let writer = attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .expect("recognized continuation has a completed Writer");
        let matrix = crate::recovery_contracts::authoritative_writer_completion_matrix(
            project_root,
            attempt,
            writer,
        )?
        .context("interrupted source Writer has no completion matrix")?;
        let findings = crate::recovery_contracts::writer_completion_findings(project_root, writer)?;
        if !matrix
            .status_against(contract, &writer.id, &findings)
            .map_err(anyhow::Error::msg)?
            .is_complete()
        {
            return Ok(None);
        }
    }
    let Some(action) = required_host_verification_pending(project_root, attempt)? else {
        return Ok(None);
    };
    if attempt.pause_kind == Some(work_model::PauseKind::Interrupted) {
        crate::lifecycle_recovery::inspect_interrupted_plan_host_recovery(
            project_root,
            attempt,
            &action.plan_row,
        )?;
    } else if let Some((writer, _, matrix)) =
        crate::recovery_contracts::owned_host_completion_matrix(project_root, attempt)?
    {
        let findings =
            crate::recovery_contracts::writer_completion_findings(project_root, &writer)?;
        if !matrix
            .status_against(
                attempt
                    .writer_completion_contract
                    .as_ref()
                    .expect("owned contract"),
                &writer.id,
                &findings,
            )
            .map_err(anyhow::Error::msg)?
            .is_complete()
        {
            return Ok(None);
        }
    }
    Ok(Some(action))
}

pub(crate) fn ensure_owned_host_resume_ready(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<()> {
    let mut settled;
    let attempt =
        if crate::recovery_contracts::validate_owned_plan_evidence_review(project_root, attempt)? {
            settled = attempt.clone();
            settled
                .evidence_recoveries
                .iter_mut()
                .find(|recovery| recovery.state == EvidenceRecoveryState::Reviewing)
                .expect("validated active receipt")
                .state = EvidenceRecoveryState::Complete;
            &settled
        } else {
            attempt
        };
    if let Some(action) = pending_owned_host_resume_action(project_root, attempt)? {
        bail!(
            "candidate {} requires host action {}: {}; attach the evidence before resume: fluent attempt evidence attach {} {} --candidate {} --evidence-file <PATH> --plan-row {}",
            action.candidate_commit,
            action.plan_row,
            action.command,
            attempt.work_item_id,
            attempt.id,
            action.candidate_commit,
            action.plan_row
        );
    }
    Ok(())
}

/// Keep every explicitly assigned host check required until its exact evidence
/// and targeted Tests Review pass. Errors also block advancement.
pub(crate) fn required_host_verification_pending(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<PlanHostEvidenceAction>> {
    let Some((writer, source, mut matrix)) =
        crate::recovery_contracts::owned_host_completion_matrix(project_root, attempt)?
    else {
        return Ok(None);
    };
    apply_completed_plan_host_evidence(project_root, attempt, &writer, &source, &mut matrix);
    let row = matrix.rows.iter().find(|row| {
        row.responsibility == Some(work_model::CompletionResponsibility::Host)
            && row.state != WriterCompletionState::Complete
    });
    row.map(|row| {
        Ok(PlanHostEvidenceAction {
            plan_row: row.id.clone(),
            working_directory: row
                .command
                .as_ref()
                .map(|command| command.working_directory.clone())
                .unwrap_or_else(|| ".".into()),
            command: resolve_plan_row_command(row)
                .context("required host check has no exact command")?,
            candidate_commit: writer
                .output
                .as_ref()
                .expect("validated candidate")
                .commit
                .clone(),
            matrix_artifact: source.path.clone(),
        })
    })
    .transpose()
}

/// Resolve one exact executable command from a Plan row without changing the
/// approved planning text. New Attempts carry a structured command captured at
/// planning time. Older matrices have no such field, so the compatibility path
/// accepts one unambiguous command-shaped candidate across their text fields.
fn resolve_plan_row_command(row: &WriterCompletionRow) -> Option<String> {
    resolve_plan_row_command_with_source(row).map(|resolved| resolved.command)
}

fn resolve_plan_row_command_with_source(row: &WriterCompletionRow) -> Option<ResolvedPlanCommand> {
    if let Some(command) = row.command.as_ref() {
        if command.source_row != row.id {
            return None;
        }
        validate_focused_command(&command.command).ok()?;
        return Some(ResolvedPlanCommand {
            command: command.command.clone(),
            source: format!("structured:{}", command.source_row),
        });
    }
    let mut candidates = row
        .verification_refs
        .iter()
        .flat_map(|text| {
            extract_plan_command_candidates(text)
                .into_iter()
                .map(|command| (command, "legacy:verification".to_string()))
                .collect::<Vec<_>>()
        })
        .chain(
            extract_plan_command_candidates(&row.requirement)
                .into_iter()
                .map(|command| (command, "legacy:requirement".to_string())),
        )
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    let command = candidates.first().map(|(command, _)| command.clone())?;
    if candidates.iter().any(|candidate| candidate.0 != command) {
        return None;
    }
    let mut sources = candidates
        .into_iter()
        .map(|(_, source)| source)
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();
    Some(ResolvedPlanCommand {
        command,
        source: sources.join("+"),
    })
}

fn plan_command_binding_matches(
    row: &WriterCompletionRow,
    command: &str,
    source: Option<&str>,
) -> bool {
    let Some(resolved) = resolve_plan_row_command_with_source(row) else {
        return false;
    };
    resolved.command == command && source.is_none_or(|source| source == resolved.source)
}

/// Return a host-materialized completion matrix only after the evidence review
/// that accepted it has passed. Before that review, the artifact remains an
/// input to the reviewer but cannot satisfy the Writer completion gate.
pub(crate) fn completed_plan_host_matrix_ref(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
) -> Option<ArtifactRef> {
    // Explicit ownership keeps external claims out of the Writer artifact. The
    // readiness gate overlays the accepted evidence from its bound source.
    if attempt
        .writer_completion_contract
        .as_ref()
        .is_some_and(|contract| {
            contract
                .requirements
                .iter()
                .any(|row| row.responsibility.is_some())
        })
    {
        return None;
    }
    attempt.artifacts.iter().rev().find_map(|artifact| {
        if artifact.producer_id != PLAN_HOST_EVIDENCE_PRODUCER
            || !artifact.path.ends_with("/writer-completion.json")
        {
            return None;
        }
        let binding_path = Path::new(&artifact.path).parent()?.join("binding.json");
        let binding_bytes = fs::read(project_root.join(&binding_path)).ok()?;
        let binding: PlanHostEvidenceBinding = serde_json::from_slice(&binding_bytes).ok()?;
        if binding.schema_version != PLAN_HOST_EVIDENCE_SCHEMA_VERSION
            || binding.work_item_id != attempt.work_item_id
            || binding.attempt_id != attempt.id
            || binding.writer_task_id != writer.id
            || binding.candidate_commit
                != writer
                    .output
                    .as_ref()
                    .map(|output| output.commit.as_str())?
            || binding.result != EvidenceResult::Pass
        {
            return None;
        }
        let expected_source = writer_completion_matrix_ref(project_root, attempt, writer)?;
        if binding.source_matrix.path != expected_source.path {
            return None;
        }
        let source_bytes = fs::read(project_root.join(&binding.source_matrix.path)).ok()?;
        if format!("sha256:{:x}", Sha256::digest(&source_bytes)) != binding.source_matrix.digest {
            return None;
        }
        let source_matrix: WriterCompletionMatrix = serde_json::from_slice(&source_bytes).ok()?;
        let source_row = source_matrix
            .rows
            .iter()
            .find(|row| row.id == binding.plan_row)?;
        if source_matrix.task_id != writer.id
            || source_row.kind != WriterCompletionRequirementKind::Plan
            || source_row.state != WriterCompletionState::Pending
            || source_row.requirement != binding.requirement
            || !plan_command_binding_matches(
                source_row,
                &binding.command,
                binding.command_source.as_deref(),
            )
        {
            return None;
        }
        let evidence_bytes = fs::read(project_root.join(&binding.evidence.path)).ok()?;
        if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != binding.evidence.digest {
            return None;
        }
        if snapshot_path(&attempt.work_item_id, &attempt.id, &binding.evidence.digest).ok()?
            != binding.evidence.path
        {
            return None;
        }
        let evidence = parse_evidence_document(&evidence_bytes).ok()?;
        if evidence.result != EvidenceResult::Pass
            || evidence.check != binding.command
            || evidence.working_directory != binding.working_directory
        {
            return None;
        }
        let recovery = attempt.evidence_recoveries.iter().find(|recovery| {
            recovery.candidate_commit == binding.candidate_commit
                && recovery.attachment.snapshot_path == binding.evidence.path
                && recovery.attachment.digest == binding.evidence.digest
                && recovery.targets.iter().any(|target| {
                    target.role == "tests"
                        && target.prior_review_artifact == binding_path.to_string_lossy()
                        && target.review_task_id.as_ref().is_some_and(|task_id| {
                            attempt.tasks.iter().any(|task| {
                                task.id == *task_id
                                    && task.kind == TaskKind::Review
                                    && task.role == "tests"
                                    && task.status == TaskStatus::Complete
                                    && read_verdict(project_root, task) == Some(Verdict::Pass)
                            })
                        })
                })
        })?;
        if recovery.state != EvidenceRecoveryState::Complete {
            return None;
        }
        let matrix_bytes = fs::read(project_root.join(&artifact.path)).ok()?;
        let matrix: WriterCompletionMatrix = serde_json::from_slice(&matrix_bytes).ok()?;
        (matrix.task_id == writer.id
            && matrix.rows.iter().any(|row| {
                row.id == binding.plan_row && row.state == WriterCompletionState::Complete
            }))
        .then_some(artifact.clone())
    })
}

/// Overlay accepted plan-host evidence onto the source Writer matrix. The
/// overlay is only an in-memory view; the immutable Writer artifact is never
/// rewritten. A later Writer-completion gate can therefore recognize the same
/// host-owned completion without inventing a source commit.
pub(crate) fn apply_completed_plan_host_evidence(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
    source_matrix: &ArtifactRef,
    matrix: &mut WriterCompletionMatrix,
) -> bool {
    let Some(output) = writer.output.as_ref() else {
        return false;
    };
    let mut changed = false;
    for artifact in attempt.artifacts.iter().filter(|artifact| {
        artifact.producer_id == PLAN_HOST_EVIDENCE_PRODUCER
            && artifact.path.ends_with("/binding.json")
    }) {
        let Ok(binding_bytes) = fs::read(project_root.join(&artifact.path)) else {
            continue;
        };
        let Ok(binding) = serde_json::from_slice::<PlanHostEvidenceBinding>(&binding_bytes) else {
            continue;
        };
        if binding.schema_version != PLAN_HOST_EVIDENCE_SCHEMA_VERSION
            || binding.work_item_id != attempt.work_item_id
            || binding.attempt_id != attempt.id
            || binding.writer_task_id != writer.id
            || binding.candidate_commit != output.commit
            || binding.result != EvidenceResult::Pass
        {
            continue;
        }
        let source_matrix_value = match crate::recovery_contracts::plan_host_source_matrix(
            project_root,
            attempt,
            writer,
            source_matrix,
            &binding.source_matrix,
            &binding.plan_row,
        ) {
            Ok(source) => source,
            Err(_) => continue,
        };
        let Some(source_row) = source_matrix_value
            .rows
            .iter()
            .find(|row| row.id == binding.plan_row)
        else {
            continue;
        };
        if source_row.kind != WriterCompletionRequirementKind::Plan
            || source_row.state != WriterCompletionState::Pending
            || source_row.requirement != binding.requirement
            || !plan_command_binding_matches(
                source_row,
                &binding.command,
                binding.command_source.as_deref(),
            )
        {
            continue;
        }
        let evidence_bytes = match fs::read(project_root.join(&binding.evidence.path)) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != binding.evidence.digest {
            continue;
        }
        let Ok(evidence) = parse_evidence_document(&evidence_bytes) else {
            continue;
        };
        if evidence.result != EvidenceResult::Pass
            || evidence.check != binding.command
            || evidence.working_directory != binding.working_directory
        {
            continue;
        }
        let Some(recovery) = attempt.evidence_recoveries.iter().find(|recovery| {
            recovery.candidate_commit == binding.candidate_commit
                && recovery.attachment.snapshot_path == binding.evidence.path
                && recovery.attachment.digest == binding.evidence.digest
                && recovery.state == EvidenceRecoveryState::Complete
                && recovery.targets.iter().any(|target| {
                    target.role == "tests"
                        && target.prior_review_artifact == artifact.path
                        && target.review_task_id.as_ref().is_some_and(|task_id| {
                            attempt.tasks.iter().any(|task| {
                                task.id == *task_id
                                    && task.kind == TaskKind::Review
                                    && task.role == "tests"
                                    && task.status == TaskStatus::Complete
                                    && read_verdict(project_root, task) == Some(Verdict::Pass)
                            })
                        })
                })
        }) else {
            continue;
        };
        let _ = recovery;
        let Some(row) = matrix.rows.iter_mut().find(|row| {
            row.id == binding.plan_row && row.kind == WriterCompletionRequirementKind::Plan
        }) else {
            continue;
        };
        if resolve_plan_row_command(row).as_deref() != Some(binding.command.as_str())
            || row.requirement != binding.requirement
        {
            continue;
        }
        if row.state != WriterCompletionState::Complete
            || row.implementation_evidence != vec![artifact.path.clone()]
            || row.verification
                != vec![WriterVerificationClaim {
                    command: binding.command.clone(),
                    result: WriterVerificationResult::Pass,
                }]
        {
            row.state = WriterCompletionState::Complete;
            row.implementation_evidence = vec![artifact.path.clone()];
            row.verification = vec![WriterVerificationClaim {
                command: binding.command.clone(),
                result: WriterVerificationResult::Pass,
            }];
            changed = true;
        }
    }
    changed
}

pub(crate) use crate::recovery_contracts::writer_completion_matrix_ref;

fn resolve_requested_value(
    name: &str,
    declared: Option<&str>,
    supplied: Option<&str>,
) -> Result<String> {
    match (declared, supplied) {
        (Some(declared), Some(supplied)) if declared != supplied => {
            bail!("supplied focused evidence {name} does not match the failed review")
        }
        (Some(declared), _) => Ok(declared.to_string()),
        (None, Some(supplied)) if !supplied.trim().is_empty() => Ok(supplied.to_string()),
        _ => bail!(
            "failed review has no valid Evidence-Command and Evidence-Working-Directory; supply the missing focused evidence {name} explicitly"
        ),
    }
}

pub(crate) fn validate_focused_command(command: &str) -> Result<()> {
    anyhow::ensure!(
        !command.trim().is_empty() && command.len() <= MAX_FOCUSED_COMMAND_BYTES,
        "focused evidence command must be between 1 and {MAX_FOCUSED_COMMAND_BYTES} bytes"
    );
    anyhow::ensure!(
        !command.contains(['\n', '\r', '\0']),
        "focused evidence command must occupy one line"
    );
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Unquoted,
        Single,
        Double,
    }
    let mut quote = Quote::Unquoted;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((_, character)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Quote::Single {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (Quote::Unquoted, '\'') => quote = Quote::Single,
            (Quote::Unquoted, '"') => quote = Quote::Double,
            (Quote::Single, '\'') => quote = Quote::Unquoted,
            (Quote::Double, '"') => quote = Quote::Unquoted,
            (Quote::Unquoted, ';' | '&' | '|' | '(' | ')' | '>' | '<') => {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token {character:?} is not allowed"
                )
            }
            (Quote::Unquoted | Quote::Double, '`') => {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token \"`\" is not allowed"
                )
            }
            (Quote::Unquoted | Quote::Double, '$')
                if chars.peek().is_some_and(|(_, next)| *next == '(') =>
            {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token \"$(\" is not allowed"
                )
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        quote == Quote::Unquoted && !escaped,
        "focused evidence command contains an unterminated quote or escape"
    );
    Ok(())
}

fn canonical_candidate_workspace(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect candidate workspace {}", path.display()))?;
    anyhow::ensure!(metadata.is_dir(), "candidate workspace must be a directory");
    path.canonicalize()
        .with_context(|| format!("canonicalize candidate workspace {}", path.display()))
}

pub(crate) fn resolve_working_directory(candidate: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    let canonical_spelling = relative == Path::new(".")
        || relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    anyhow::ensure!(
        !relative.is_absolute() && canonical_spelling,
        "focused evidence working directory must be a canonical relative candidate path"
    );
    let joined = candidate.join(relative);
    let resolved = joined.canonicalize().with_context(|| {
        format!(
            "resolve focused evidence working directory {}",
            joined.display()
        )
    })?;
    anyhow::ensure!(
        resolved.starts_with(candidate) && resolved.is_dir(),
        "focused evidence working directory must stay inside the candidate"
    );
    Ok(resolved)
}

pub(crate) fn create_managed_directory_chain(
    project_root: &Path,
    relative: &Path,
) -> Result<PathBuf> {
    anyhow::ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "managed evidence directory must use normal relative components"
    );
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            unreachable!("validated normal component");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("managed evidence directory must not traverse a symlink")
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("managed evidence path component must be a directory")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)
                .with_context(|| {
                    format!("create managed evidence directory {}", current.display())
                })?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

pub(crate) struct FocusedExecution {
    pub exit_code: i32,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RustupEnvironment {
    home: Option<PathBuf>,
    toolchain: Option<std::ffi::OsString>,
}

fn host_rustup_environment(source_home: &Path) -> RustupEnvironment {
    let configured_home = std::env::var_os("RUSTUP_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = configured_home
        .unwrap_or_else(|| source_home.join(".rustup"))
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir());
    let toolchain = std::env::var_os("RUSTUP_TOOLCHAIN").filter(|value| !value.is_empty());
    RustupEnvironment { home, toolchain }
}

fn focused_evidence_candidate_boundaries(
    candidate_workspace: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let common_git_dir = crate::worktree::git_common_dir(candidate_workspace)?;
    let common_git_dir = common_git_dir.canonicalize().with_context(|| {
        format!(
            "canonicalize focused evidence Git metadata {}",
            common_git_dir.display()
        )
    })?;
    Ok((
        vec![candidate_workspace.to_path_buf(), common_git_dir.clone()],
        vec![candidate_workspace.to_path_buf(), common_git_dir],
    ))
}

pub(crate) fn execute_focused_command(
    candidate_workspace: &Path,
    working_directory: &Path,
    run_dir: &Path,
    command: &str,
    timeout: Duration,
    resolver: &ContentResolver,
) -> Result<FocusedExecution> {
    let scratch = run_dir.join("scratch");
    let home = scratch.join("home");
    let temporary = scratch.join("tmp");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&temporary)?;
    let cargo_home = scratch.join("cargo-home");
    crate::writer_cache::prepare_private_cargo_dependency_cache(
        &cargo_home,
        crate::writer_cache::canonical_cargo_home_from_env().as_deref(),
    )?;
    let source_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("focused evidence requires the host HOME path for toolchain confinement")?;
    let rustup = host_rustup_environment(&source_home);
    let denied_agent_state = [".claude", ".codex", ".agents"]
        .into_iter()
        .map(|path| source_home.join(path))
        .collect::<Vec<_>>();
    let (mut readable_roots, candidate_denied_writes) =
        focused_evidence_candidate_boundaries(candidate_workspace)?;
    let mut denied_writes = denied_agent_state.clone();
    denied_writes.extend(candidate_denied_writes);
    denied_writes.extend([
        source_home.join(".local/state"),
        source_home.join(".claude.json"),
        source_home.join(".claude.json.lock"),
    ]);
    if let Some(rustup_home) = &rustup.home {
        denied_writes.push(rustup_home.clone());
        readable_roots.push(rustup_home.clone());
    }
    let profile = crate::os::render_profile_common_only_with_denied_access(
        resolver,
        &source_home.to_string_lossy(),
        &[scratch.clone()],
        &readable_roots,
        &denied_agent_state,
        &denied_writes,
    )?;
    crate::os::preflight_profile(&profile)?;

    let stdout_path = run_dir.join("stdout.log");
    let stderr_path = run_dir.join("stderr.log");
    let stdout = create_exclusive_file(&stdout_path)?;
    let stderr = create_exclusive_file(&stderr_path)?;
    let mut process = crate::os::command_under_profile(&profile, Path::new("/bin/sh"));
    process
        .arg("-c")
        .arg(command)
        .current_dir(working_directory)
        .env_clear()
        .env("HOME", &home)
        // Foundation-backed tools can resolve the account home without HOME.
        // Keep their user caches in the same private tree as shell-native tools.
        .env("CFFIXED_USER_HOME", &home)
        .env("TMPDIR", &temporary)
        .env("XDG_CACHE_HOME", scratch.join("xdg-cache"))
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TARGET_DIR", scratch.join("cargo-target"))
        .env("CLANG_MODULE_CACHE_PATH", scratch.join("clang-modules"))
        .env("SWIFT_MODULE_CACHE_PATH", scratch.join("swift-modules"))
        .env(
            "SWIFTPM_MODULECACHE_OVERRIDE",
            scratch.join("swiftpm-modules"),
        )
        .env("FLUENT_EVIDENCE_SCRATCH", &scratch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(rustup_home) = &rustup.home {
        process.env("RUSTUP_HOME", rustup_home);
    }
    if let Some(toolchain) = &rustup.toolchain {
        process.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    for name in ["PATH", "DEVELOPER_DIR", "SDKROOT", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let transcript = run_dir.join("stdout.log");
    crate::execution::configure_identity_lock(&mut process, &transcript)?;
    let mut child = process
        .spawn()
        .with_context(|| format!("spawn focused evidence command: {command}"))?;
    let mut execution = crate::execution::ExecutionHeartbeat::start(&transcript, child.id())?;
    let stdout_pipe = child.stdout.take().context("missing verification stdout")?;
    let stderr_pipe = child.stderr.take().context("missing verification stderr")?;
    let stdout_capture = std::thread::spawn(move || capture_focused_log(stdout_pipe, stdout));
    let stderr_capture = std::thread::spawn(move || capture_focused_log(stderr_pipe, stderr));
    let waited = crate::coder::wait_owned_process_tree(child, timeout);
    execution.settle()?;
    stdout_capture
        .join()
        .map_err(|_| anyhow::anyhow!("verification stdout capture panicked"))??;
    stderr_capture
        .join()
        .map_err(|_| anyhow::anyhow!("verification stderr capture panicked"))??;
    let (natural_exit_code, timed_out, disposition) =
        waited.map_err(|error| anyhow::anyhow!(error))?;
    anyhow::ensure!(
        disposition.is_verified_swept(),
        "focused evidence process cleanup was not confirmed: {disposition:?}"
    );
    let exit_code = if timed_out { 124 } else { natural_exit_code };
    Ok(FocusedExecution { exit_code })
}

fn capture_focused_log(mut source: impl Read, mut target: fs::File) -> Result<()> {
    let mut retained = 0;
    let mut buffer = [0; 8192];
    loop {
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        let keep = length.min((crate::verification::MAX_LOG_BYTES - retained) as usize);
        target.write_all(&buffer[..keep])?;
        retained += keep as u64;
    }
    target.sync_all()?;
    Ok(())
}

fn create_exclusive_file(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create host evidence file {}", path.display()))
}

pub(crate) fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_exclusive_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn attach(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    candidate_commit: &str,
    evidence_file: &Path,
    review_artifacts: &[String],
    recheck_artifact: Option<&str>,
) -> Result<EvidenceRecovery> {
    anyhow::ensure!(
        (recheck_artifact.is_some() && review_artifacts.is_empty())
            || (recheck_artifact.is_none() && !review_artifacts.is_empty()),
        "name either failed review artifacts or one failed-command recheck artifact"
    );
    if fs::symlink_metadata(evidence_file)
        .with_context(|| format!("inspect evidence file {}", evidence_file.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("evidence file must not be a symlink");
    }
    let bytes = fs::read(evidence_file)
        .with_context(|| format!("read evidence file {}", evidence_file.display()))?;
    let document = parse_evidence_document(&bytes)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let snapshot_path = snapshot_path(work_item_id, attempt_id, &digest)?;
    let input_identity = format!(
        "{}\0{}",
        recheck_artifact.unwrap_or("review"),
        review_artifacts.join("\0")
    );
    let operation = crate::work_model::RecoveryOperationRequest::new(
        if recheck_artifact.is_some() {
            "host-evidence-recheck-attachment"
        } else {
            "host-evidence-review-attachment"
        },
        work_item_id,
        attempt_id,
        Some(candidate_commit.to_string()),
        None,
        Some(digest.clone()),
        None,
        None,
        Some(format!(
            "sha256:{:x}",
            Sha256::digest(input_identity.as_bytes())
        )),
    );

    // Share the land boundary with Attempt execution and landing.  Acquire it
    // before the model lock so the frontier is revalidated only after no
    // concurrent operation can publish a candidate transition.
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire land lock before attaching host evidence")?;

    let recovery = store.mutate_work_item(work_item_id, |item| {
        let _ = item.validate_recovery_operation(project_root, &operation)?;
        let existing = item
            .attempts
            .iter()
            .find(|a| a.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        if let Some(recheck_artifact) = recheck_artifact
            && let Some(prior) = existing_recheck_recovery(
                project_root,
                existing,
                candidate_commit,
                &digest,
                &snapshot_path,
                recheck_artifact,
            )
        {
            return Ok(prior.clone());
        }
        if recheck_artifact.is_none()
            && review_artifacts.len() == 1
            && review_artifacts[0].ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
            && let Some(prior) = existing_candidate_gate_recovery(
                project_root,
                existing,
                candidate_commit,
                &digest,
                &snapshot_path,
                &review_artifacts[0],
            )
        {
            return Ok(prior.clone());
        }
        let recheck_frontier = recheck_artifact
            .map(|artifact| {
                validate_recheck_evidence_frontier(
                    project_root,
                    item,
                    attempt_id,
                    candidate_commit,
                    artifact,
                    &document,
                )
            })
            .transpose()?;
        let candidate_gate_frontier = if recheck_frontier.is_none()
            && review_artifacts.len() == 1
            && review_artifacts[0].ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE)
        {
            Some(validate_candidate_gate_evidence_frontier(
                project_root,
                item,
                attempt_id,
                candidate_commit,
                &review_artifacts[0],
                &document,
            )?)
        } else {
            None
        };
        let prior_reviews = match (&recheck_frontier, &candidate_gate_frontier) {
            (Some(frontier), _) => vec![frontier.prior_tests_review.clone()],
            (None, Some(frontier)) => vec![frontier.prior_tests_review.clone()],
            (None, None) => validate_evidence_frontier(
                project_root,
                item,
                attempt_id,
                candidate_commit,
                review_artifacts,
            )?,
        };
        let interrupted_correction_id = (recheck_frontier.is_none()
            && candidate_gate_frontier.is_none())
        .then(|| {
            interrupted_evidence_correction(existing, candidate_commit, review_artifacts)
                .map(|task| task.id.clone())
        })
        .flatten();
        let target_artifacts = match (&recheck_frontier, &candidate_gate_frontier) {
            (Some(frontier), _) => vec![frontier.binding_path.clone()],
            (None, Some(frontier)) => vec![frontier.binding_path.clone()],
            (None, None) => review_artifacts.to_vec(),
        };
        if let Some(prior) = existing.evidence_recoveries.iter().find(|prior| {
            prior.candidate_commit == candidate_commit
                && prior.attachment.digest == digest
                && prior.attachment.snapshot_path == snapshot_path
                && prior.targets.len() == prior_reviews.len()
                && prior.targets.iter().all(|target| {
                    prior_reviews
                        .iter()
                        .any(|review| review.role == target.role)
                        && target_artifacts
                            .iter()
                            .any(|path| path == &target.prior_review_artifact)
                })
        }) {
            return Ok(prior.clone());
        }
        let attachment_was_used = existing.evidence_recoveries.iter().any(|prior| {
            prior.candidate_commit == candidate_commit
                && prior.attachment.digest == digest
                && prior.attachment.snapshot_path == snapshot_path
        });
        let recovery_token = if attachment_was_used {
            let frontier = target_artifacts.join("\0");
            let frontier_digest = format!("{:x}", Sha256::digest(frontier.as_bytes()));
            format!("{}-{}", &digest[7..19], &frontier_digest[..12])
        } else {
            digest[7..19].to_string()
        };
        // Publish only after the lock-held frontier check. A rejected request
        // therefore cannot leave a host-owned artifact behind.
        publish_snapshot(project_root, &snapshot_path, &bytes).map_err(|_| {
            WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            }
        })?;
        if let Some(frontier) = &recheck_frontier {
            publish_snapshot(
                project_root,
                &frontier.binding_path,
                &frontier.binding_bytes,
            )
            .map_err(|_| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        }
        if let Some(frontier) = &candidate_gate_frontier {
            publish_snapshot(
                project_root,
                &frontier.binding_path,
                &frontier.binding_bytes,
            )
            .map_err(|_| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        }
        let attempt = item
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt_id)
            .expect("validated Attempt must remain present");
        let roles = prior_reviews
            .iter()
            .map(|task| task.role.clone())
            .collect::<Vec<_>>();
        let recovery = attempt.attach_evidence_recovery(EvidenceRecovery {
            id: format!("host-evidence-{recovery_token}"),
            candidate_commit: candidate_commit.to_string(),
            attachment: EvidenceAttachment {
                snapshot_path: snapshot_path.clone(),
                digest: digest.clone(),
            },
            targets: roles
                .iter()
                .map(|role| EvidenceReviewTarget {
                    role: role.clone(),
                    prior_review_artifact: if recheck_frontier.is_some()
                        || candidate_gate_frontier.is_some()
                    {
                        target_artifacts.first().cloned().unwrap_or_default()
                    } else {
                        review_artifacts
                            .iter()
                            .find(|path| {
                                prior_reviews.iter().any(|task| {
                                    task.role == *role
                                        && review_artifact_path(task).as_deref()
                                            == Some(path.as_str())
                                })
                            })
                            .cloned()
                            .unwrap_or_default()
                    },
                    review_task_id: None,
                    review_digest: None,
                })
                .collect(),
            state: EvidenceRecoveryState::Reviewing,
            created_at: work_model::now_iso8601(),
            code_change_classification: None,
        })?;
        if let Some(task_id) = interrupted_correction_id
            && let Some(task) = attempt.tasks.iter_mut().find(|task| task.id == task_id)
        {
            work_model::set_task_terminal(task, TaskStatus::Superseded);
        }
        for role in roles {
            let prior = prior_reviews
                .iter()
                .find(|task| task.role == role)
                .cloned()
                .unwrap();
            let task_id = format!("{attempt_id}-evidence-{recovery_token}-{role}");
            if !attempt.tasks.iter().any(|task| task.id == task_id) {
                let mut inputs = prior.input_artifacts.clone();
                if let Some(frontier) = &recheck_frontier {
                    inputs.push(ArtifactRef {
                        producer_id: frontier.binding.recheck_task_id.clone(),
                        path: frontier.binding_path.clone(),
                    });
                    inputs.extend(frontier.correction_artifacts.clone());
                }
                if let Some(frontier) = &candidate_gate_frontier {
                    inputs.push(ArtifactRef {
                        producer_id: "candidate-gate".to_string(),
                        path: frontier.binding_path.clone(),
                    });
                }
                inputs.push(ArtifactRef {
                    producer_id: "host-evidence".to_string(),
                    path: snapshot_path.clone(),
                });
                inputs.push(ArtifactRef {
                    producer_id: prior.id.clone(),
                    path: review_artifact_path(&prior).unwrap(),
                });
                let prior_review_artifact = recheck_frontier
                    .as_ref()
                    .map(|frontier| frontier.binding_path.clone())
                    .or_else(|| {
                        candidate_gate_frontier
                            .as_ref()
                            .map(|frontier| frontier.binding_path.clone())
                    })
                    .unwrap_or_else(|| review_artifact_path(&prior).unwrap());
                attempt.tasks.push(Task {
                    id: task_id.clone(),
                    kind: TaskKind::Review,
                    status: TaskStatus::Planned,
                    role: role.clone(),
                    instructions: None,
                    work_item_id: work_item_id.to_string(),
                    attempt_id: Some(attempt_id.to_string()),
                    workspace_access: prior.workspace_access,
                    artifact_area: Some(work_model::TaskArtifactArea {
                        path: work_model::work_artifact_path(work_item_id, attempt_id, &task_id),
                    }),
                    review_context: prior.review_context.clone(),
                    verification_context: None,
                    evidence_review_context: Some(EvidenceReviewContext {
                        recovery_id: recovery.id.clone(),
                        candidate_commit: candidate_commit.to_string(),
                        attachment: recovery.attachment.clone(),
                        prior_review_artifact,
                    }),
                    input_artifacts: inputs,
                    depends_on: None,
                    output: None,
                    created_at: Some(work_model::now_iso8601()),
                    started_at: None,
                    completed_at: None,
                });
            }
            if let Some(target) = attempt
                .evidence_recoveries
                .iter_mut()
                .find(|entry| entry.id == recovery.id)
                .and_then(|entry| entry.targets.iter_mut().find(|target| target.role == role))
            {
                target.review_task_id = Some(task_id);
            }
        }
        attempt.status = work_model::AttemptStatus::Reviewing;
        attempt.review_state = Some(work_model::AttemptReviewState::NotReviewed);
        attempt.pause_kind = None;
        attempt.pause_recovery = None;
        let next_action = crate::work_status::derive_attempt_recovery_next_action(
            Some(attempt),
            Some(project_root),
        );
        attempt.record_recovery_operation(&operation, next_action)?;
        Ok(attempt
            .evidence_recoveries
            .iter()
            .find(|entry| entry.id == recovery.id)
            .cloned()
            .expect("attached recovery remains present"))
    })?;
    let _ = document; // validation intentionally happens before publication.
    Ok(recovery)
}

/// Attach evidence for a required Plan row whose verification must run on an
/// authorized host. Unlike ordinary focused evidence, this frontier has no
/// failed Review artifact yet: the completed Writer is paused before review
/// planning because the Writer cannot access the host runtime. The evidence
/// binding therefore creates the first targeted Tests Review directly against
/// the unchanged candidate.
#[allow(clippy::too_many_arguments)]
pub fn attach_plan_row(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    candidate_commit: &str,
    plan_row: &str,
    evidence_file: &Path,
) -> Result<EvidenceRecovery> {
    if fs::symlink_metadata(evidence_file)
        .with_context(|| format!("inspect evidence file {}", evidence_file.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("evidence file must not be a symlink");
    }
    let bytes = fs::read(evidence_file)
        .with_context(|| format!("read evidence file {}", evidence_file.display()))?;
    let document = parse_evidence_document(&bytes)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let snapshot_path = snapshot_path(work_item_id, attempt_id, &digest)?;
    let plan_row = plan_row.trim().to_string();
    anyhow::ensure!(
        !plan_row.is_empty() && plan_row.len() <= MAX_FOCUSED_COMMAND_BYTES,
        "plan row must name one completion-matrix row"
    );
    let operation = crate::work_model::RecoveryOperationRequest::new(
        "host-evidence-plan-row-attachment",
        work_item_id,
        attempt_id,
        Some(candidate_commit.to_string()),
        None,
        Some(digest.clone()),
        None,
        None,
        Some(format!(
            "sha256:{:x}",
            Sha256::digest(format!("{plan_row}\0{digest}").as_bytes())
        )),
    );

    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire land lock before attaching Plan-row host evidence")?;
    let recovery = store.mutate_work_item_from_external(
        work_item_id,
        format!("attach authorized-host evidence for Plan row {plan_row}"),
        Some(snapshot_path.clone()),
        |item| {
            let historical = item.attempts.iter_mut().find(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound { id: attempt_id.to_string() })?;
            if crate::recovery_contracts::validate_owned_plan_evidence_review(project_root, historical)
                .map_err(|error| WorkModelError::HostEvidenceRecoveryIneligible { attempt_id: attempt_id.to_string(), reason: format!("{error:#}") })? {
                historical.evidence_recoveries.iter_mut().find(|recovery| recovery.state == EvidenceRecoveryState::Reviewing)
                    .expect("validated active receipt").state = EvidenceRecoveryState::Complete;
            }
            let attempt = item
                .attempts
                .iter()
                .find(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound {
                    id: attempt_id.to_string(),
                })?;
            let _ = item.validate_recovery_operation(project_root, &operation)?;
            let ineligible = |reason: &str| WorkModelError::HostEvidenceRecoveryIneligible {
                attempt_id: attempt_id.to_string(),
                reason: reason.to_string(),
            };
            if item
                .attempts
                .last()
                .is_none_or(|current| current.id != attempt_id)
            {
                return Err(ineligible(
                    "Plan-row host evidence requires the current Attempt",
                ));
            }
            if item
                .merge_candidates
                .iter()
                .any(|candidate| candidate.attempt_id == attempt_id)
            {
                return Err(ineligible(
                    "Plan-row host evidence is not eligible after a Merge Candidate exists",
                ));
            }
            if attempt
                .tasks
                .iter()
                .any(|task| task.status == TaskStatus::Executing)
            {
                return Err(ineligible("the Attempt has an executing Task"));
            }
            for task in &attempt.tasks {
                if crate::lease::is_leased(&crate::lease::task_lock_path(
                    project_root, work_item_id, &task.id,
                )) {
                    return Err(ineligible("the Attempt has a live Task owner"));
                }
                if let Some(area) = &task.artifact_area {
                    if crate::execution::execution_is_live(
                        &crate::execution::record_path_for_artifact(project_root, &area.path),
                    ).map_err(|error| ineligible(&error.to_string()))? {
                        return Err(ineligible("the Attempt has a live execution owner"));
                    }
                }
            }
            crate::recovery_contracts::owned_host_completion_matrix(project_root, attempt)
                .map_err(|error| ineligible(&format!("{error:#}")))?;
            let interrupted_writer = crate::lifecycle_recovery::inspect_interrupted_plan_host_recovery(
                project_root, attempt, &plan_row,
            ).map_err(|error| ineligible(&format!("{error:#}")))?;
            let writer = attempt
                .tasks
                .iter()
                .rev()
                .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
                .cloned()
                .ok_or_else(|| ineligible("the Attempt has no completed Writer candidate"))?;
            let output = writer
                .output
                .as_ref()
                .ok_or_else(|| ineligible("the completed Writer has no candidate output"))?;
            if output.commit != candidate_commit {
                return Err(ineligible(&format!(
                    "candidate {candidate_commit} does not match the latest Writer candidate {}",
                    output.commit
                )));
            }
            if !candidate_workspace_is_clean_at(project_root, output, candidate_commit) {
                return Err(ineligible(
                    "the candidate worktree must be clean before attaching host evidence",
                ));
            }
            let matrix_ref = writer_completion_matrix_ref(project_root, attempt, &writer)
                .ok_or_else(|| ineligible("the completed Writer has no completion matrix"))?;
            let matrix_path = project_root.join(&matrix_ref.path);
            let matrix_bytes = fs::read(&matrix_path).map_err(|error| {
                ineligible(&format!(
                    "read Writer completion matrix {}: {error}",
                    matrix_path.display()
                ))
            })?;
            let mut matrix: WriterCompletionMatrix = serde_json::from_slice(&matrix_bytes)
                .map_err(|error| ineligible(&format!("completion matrix is malformed: {error}")))?;
            if matrix.task_id != writer.id {
                return Err(ineligible(
                    "completion matrix is not bound to the latest Writer",
                ));
            }
            if let Some(contract) = attempt.writer_completion_contract.as_ref() {
                let findings =
                    crate::recovery_contracts::writer_completion_findings(project_root, &writer)
                        .map_err(|error| {
                            ineligible(&format!("read Writer completion findings: {error:#}"))
                        })?;
                matrix
                    .status_against(contract, &writer.id, &findings)
                    .map_err(|reason| {
                        ineligible(&format!("completion matrix is invalid: {reason}"))
                    })?;
            }
            let row = matrix
                .rows
                .iter()
                .find(|row| row.id == plan_row)
                .ok_or_else(|| ineligible(&format!("completion matrix has no row {plan_row:?}")))?;
            if row.kind != WriterCompletionRequirementKind::Plan {
                return Err(ineligible(&format!(
                    "row {plan_row:?} is not a required Plan row"
                )));
            }
            if row.responsibility.is_some() && row.responsibility != Some(work_model::CompletionResponsibility::Host) {
                return Err(ineligible("the approved obligation is not host-owned"));
            }
            if row.state != WriterCompletionState::Pending {
                return Err(ineligible(&format!(
                    "Plan row {plan_row:?} is already complete"
                )));
            }
            let resolved_command = resolve_plan_row_command_with_source(row).ok_or_else(|| {
                ineligible(&format!(
                    "Plan row {plan_row:?} must contain exactly one unambiguous host verification command"
                ))
            })?;
            let command = resolved_command.command.clone();
            if document.check != command {
                return Err(ineligible(&format!(
                    "evidence check {:?} does not match Plan row command {:?}",
                    document.check, command
                )));
            }
            if row.responsibility == Some(work_model::CompletionResponsibility::Host)
                && row.command.as_ref().is_none_or(|command| command.working_directory != document.working_directory)
            {
                return Err(ineligible("evidence working directory does not match the approved host command"));
            }
            let source_matrix_digest = format!("sha256:{:x}", Sha256::digest(&matrix_bytes));
            let binding = PlanHostEvidenceBinding {
                schema_version: PLAN_HOST_EVIDENCE_SCHEMA_VERSION,
                work_item_id: work_item_id.to_string(),
                attempt_id: attempt_id.to_string(),
                writer_task_id: writer.id.clone(),
                candidate_commit: candidate_commit.to_string(),
                plan_row: plan_row.clone(),
                requirement: row.requirement.clone(),
                command: command.clone(),
                command_source: Some(resolved_command.source),
                working_directory: document.working_directory.clone(),
                source_matrix: BoundRecheckArtifact {
                    path: matrix_ref.path.clone(),
                    digest: source_matrix_digest,
                },
                evidence: BoundRecheckArtifact {
                    path: snapshot_path.clone(),
                    digest: digest.clone(),
                },
                result: document.result.clone(),
                run_at: document.run_at.clone(),
            };
            let binding_bytes = serde_json::to_vec_pretty(&binding).map_err(|error| {
                ineligible(&format!("serialize host-evidence binding: {error}"))
            })?;
            let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
            let evidence_slug = digest
                .strip_prefix("sha256:")
                .unwrap_or(digest.as_str())
                .chars()
                .take(16)
                .collect::<String>();
            let row_slug = plan_row_slug(&plan_row);
            let directory = format!(
                "plan-host-evidence/{}/{row_slug}-{evidence_slug}",
                writer.id
            );
            let binding_path = work_artifact_path(
                work_item_id,
                attempt_id,
                &format!("{directory}/binding.json"),
            );
            let matrix_path = work_artifact_path(
                work_item_id,
                attempt_id,
                &format!("{directory}/writer-completion.json"),
            );
            let recovery_id = format!(
                "plan-host-evidence-{}-{}",
                &binding_digest
                    .strip_prefix("sha256:")
                    .unwrap_or(binding_digest.as_str())[..16],
                row_slug
            );
            if let Some(existing) = attempt.evidence_recoveries.iter().find(|recovery| {
                recovery.id == recovery_id
                    || (recovery.candidate_commit == candidate_commit
                        && recovery.attachment.digest == digest
                        && recovery.targets.iter().any(|target| {
                            target.role == "tests" && target.prior_review_artifact == binding_path
                        }))
            }) {
                if row.responsibility.is_some() && existing.state != EvidenceRecoveryState::Reviewing {
                    return Err(ineligible("the recorded Plan evidence transition is no longer eligible for attachment replay"));
                }
                crate::recovery_contracts::validate_owned_plan_evidence_review(project_root, attempt)
                    .map_err(|error| ineligible(&format!("{error:#}")))?;
                return Ok(existing.clone());
            }

            // Idempotent retries of the exact attachment are allowed after the
            // first invocation has moved the Attempt into targeted review. A
            // different attachment or Plan row must still begin at the original
            // Writer-completion pause, so it cannot silently hijack a live
            // recovery frontier.
            if interrupted_writer.is_none()
                && !(attempt.status == work_model::AttemptStatus::NeedsUser
                    && attempt.pause_kind == Some(work_model::PauseKind::WriterCompletion)
                    && attempt.review_state == Some(work_model::AttemptReviewState::NotReviewed))
            {
                return Err(ineligible(
                    "the Attempt is not paused at Writer completion for a pending Plan row",
                ));
            }

            publish_snapshot(project_root, &snapshot_path, &bytes)
                .map_err(|error| ineligible(&format!("publish host evidence: {error:#}")))?;
            publish_snapshot(project_root, &binding_path, &binding_bytes).map_err(|error| {
                ineligible(&format!("publish host-evidence binding: {error:#}"))
            })?;

            let host_matrix_ref = if document.result == EvidenceResult::Pass {
                if let Some(row) = matrix.rows.iter_mut().find(|row| row.id == plan_row) {
                    row.state = WriterCompletionState::Complete;
                    row.implementation_evidence = vec![binding_path.clone()];
                    row.verification = vec![WriterVerificationClaim {
                        command: command.clone(),
                        result: WriterVerificationResult::Pass,
                    }];
                }
                let host_matrix_bytes = serde_json::to_vec_pretty(&matrix).map_err(|error| {
                    ineligible(&format!("serialize host completion matrix: {error}"))
                })?;
                publish_snapshot(project_root, &matrix_path, &host_matrix_bytes).map_err(
                    |error| ineligible(&format!("publish host completion matrix: {error:#}")),
                )?;
                Some(ArtifactRef {
                    producer_id: PLAN_HOST_EVIDENCE_PRODUCER.to_string(),
                    path: matrix_path.clone(),
                })
            } else {
                None
            };

            let recovery = EvidenceRecovery {
                id: recovery_id,
                candidate_commit: candidate_commit.to_string(),
                attachment: EvidenceAttachment {
                    snapshot_path: snapshot_path.clone(),
                    digest: digest.clone(),
                },
                targets: vec![EvidenceReviewTarget {
                    role: "tests".to_string(),
                    prior_review_artifact: binding_path.clone(),
                    review_task_id: None,
                    review_digest: None,
                }],
                state: EvidenceRecoveryState::Reviewing,
                created_at: work_model::now_iso8601(),
                code_change_classification: None,
            };
            if let Some(task_id) = interrupted_writer {
                let attempt = item.attempts.iter_mut()
                    .find(|attempt| attempt.id == attempt_id).expect("validated Attempt");
                let writer = attempt.tasks.iter_mut()
                    .find(|task| task.id == task_id).expect("validated stopped Writer");
                work_model::set_task_terminal(writer, TaskStatus::Superseded);
            }
            let recovery = item
                .attempts
                .iter_mut()
                .find(|candidate| candidate.id == attempt_id)
                .expect("validated Attempt remains present")
                .attach_evidence_recovery(recovery)?;
            let task_ids = item
                .add_next_review_tasks(attempt_id, &["tests"])
                .map_err(|error| ineligible(&format!("plan targeted Tests Review: {error}")))?;
            let task_id = task_ids
                .first()
                .cloned()
                .ok_or_else(|| ineligible("Plan-row evidence did not create a Tests Review"))?;
            let attempt = item
                .attempts
                .iter_mut()
                .find(|candidate| candidate.id == attempt_id)
                .expect("validated Attempt remains present");
            let evidence_ref = ArtifactRef {
                producer_id: "host-evidence".to_string(),
                path: snapshot_path.clone(),
            };
            let binding_ref = ArtifactRef {
                producer_id: PLAN_HOST_EVIDENCE_PRODUCER.to_string(),
                path: binding_path.clone(),
            };
            for artifact in [evidence_ref.clone(), binding_ref.clone()] {
                if !attempt.artifacts.contains(&artifact) {
                    attempt.artifacts.push(artifact.clone());
                }
            }
            if let Some(host_matrix_ref) = host_matrix_ref.clone()
                && !attempt.artifacts.contains(&host_matrix_ref)
            {
                attempt.artifacts.push(host_matrix_ref);
            }
            let review = attempt
                .tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .ok_or_else(|| ineligible("created Tests Review disappeared"))?;
            for artifact in [evidence_ref, binding_ref] {
                if !review.input_artifacts.contains(&artifact) {
                    review.input_artifacts.push(artifact);
                }
            }
            if let Some(host_matrix_ref) = host_matrix_ref {
                if !review.input_artifacts.contains(&host_matrix_ref) {
                    review.input_artifacts.push(host_matrix_ref);
                }
            }
            review.evidence_review_context = Some(EvidenceReviewContext {
                recovery_id: recovery.id.clone(),
                candidate_commit: candidate_commit.to_string(),
                attachment: recovery.attachment.clone(),
                prior_review_artifact: binding_path,
            });
            let target = attempt
                .evidence_recoveries
                .iter_mut()
                .find(|entry| entry.id == recovery.id)
                .and_then(|entry| {
                    entry
                        .targets
                        .iter_mut()
                        .find(|target| target.role == "tests")
                })
                .ok_or_else(|| ineligible("Plan-row evidence recovery target disappeared"))?;
            target.review_task_id = Some(task_id);
            attempt.pause_kind = None;
            attempt.completed_at = None;
            let next_action = crate::work_status::derive_attempt_recovery_next_action(
                Some(attempt),
                Some(project_root),
            );
            attempt.record_recovery_operation(&operation, next_action)?;
            Ok(attempt
                .evidence_recoveries
                .iter()
                .find(|entry| entry.id == recovery.id)
                .cloned()
                .expect("attached Plan-row recovery remains present"))
        },
    )?;
    Ok(recovery)
}

fn plan_row_slug(row: &str) -> String {
    let slug = row
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "plan-row".to_string()
    } else {
        slug.to_string()
    }
}

fn existing_recheck_recovery<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
    evidence_digest: &str,
    evidence_snapshot_path: &str,
    recheck_artifact: &str,
) -> Option<&'a EvidenceRecovery> {
    attempt.evidence_recoveries.iter().find_map(|recovery| {
        if recovery.candidate_commit != candidate_commit
            || recovery.attachment.digest != evidence_digest
            || recovery.attachment.snapshot_path != evidence_snapshot_path
            || recovery.targets.len() != 1
        {
            return None;
        }
        let binding_path = &recovery.targets[0].prior_review_artifact;
        let binding_bytes = fs::read(project_root.join(binding_path)).ok()?;
        let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
        if snapshot_path(&attempt.work_item_id, &attempt.id, &binding_digest)
            .ok()?
            .as_str()
            != binding_path
            || format!(
                "sha256:{:x}",
                Sha256::digest(fs::read(project_root.join(evidence_snapshot_path)).ok()?)
            ) != evidence_digest
        {
            return None;
        }
        let binding: FailedCommandRecheckEvidenceBinding =
            serde_json::from_slice(&binding_bytes).ok()?;
        let recheck = attempt
            .tasks
            .iter()
            .find(|task| task.id == binding.recheck_task_id)?;
        let expected = recheck
            .artifact_area
            .as_ref()
            .map(|area| format!("{}/failed-command-recheck.json", area.path));
        (binding.candidate_commit == candidate_commit
            && expected.as_deref() == Some(recheck_artifact))
        .then_some(recovery)
    })
}

fn existing_candidate_gate_recovery<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
    evidence_digest: &str,
    evidence_snapshot_path: &str,
    gate_artifact: &str,
) -> Option<&'a EvidenceRecovery> {
    attempt.evidence_recoveries.iter().find_map(|recovery| {
        if recovery.candidate_commit != candidate_commit
            || recovery.attachment.digest != evidence_digest
            || recovery.attachment.snapshot_path != evidence_snapshot_path
            || recovery.targets.len() != 1
        {
            return None;
        }
        let binding_path = &recovery.targets[0].prior_review_artifact;
        let binding_bytes = fs::read(project_root.join(binding_path)).ok()?;
        let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
        if snapshot_path(&attempt.work_item_id, &attempt.id, &binding_digest)
            .ok()?
            .as_str()
            != binding_path
            || format!(
                "sha256:{:x}",
                Sha256::digest(fs::read(project_root.join(evidence_snapshot_path)).ok()?)
            ) != evidence_digest
        {
            return None;
        }
        let binding: CandidateGateEvidenceBinding = serde_json::from_slice(&binding_bytes).ok()?;
        if binding.schema_version != 1
            || binding.candidate_commit != candidate_commit
            || binding.gate_artifact.path != gate_artifact
        {
            return None;
        }
        let current_gate_bytes = fs::read(project_root.join(gate_artifact)).ok()?;
        let current_gate_digest = format!("sha256:{:x}", Sha256::digest(&current_gate_bytes));
        let original_frontier_is_unchanged = current_gate_digest == binding.gate_artifact.digest;
        let completed_frontier_now_passes = recovery.state == EvidenceRecoveryState::Complete
            && serde_json::from_slice::<crate::candidate_gate::CandidateGateEvidence>(
                &current_gate_bytes,
            )
            .ok()
            .is_some_and(|gate| {
                gate.candidate_commit == candidate_commit
                    && gate.phase == crate::candidate_gate::CandidateGatePhase::FinalTester
                    && gate.disposition == crate::candidate_gate::CandidateGateDisposition::Passed
            });
        (original_frontier_is_unchanged || completed_frontier_now_passes).then_some(recovery)
    })
}

fn validate_candidate_gate_evidence_frontier(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate_commit: &str,
    gate_artifact: &str,
    document: &EvidenceDocument,
) -> Result<ValidatedCandidateGateFrontier, WorkModelError> {
    let rejected = || WorkModelError::HostEvidenceRecoveryIneligible {
        attempt_id: attempt_id.to_string(),
        reason: "the supplied evidence does not match the immutable candidate-gate frontier"
            .to_string(),
    };
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(rejected)?;
    if attempt.status != work_model::AttemptStatus::NeedsUser
        || !matches!(
            attempt.pause_kind,
            Some(
                work_model::PauseKind::RoundCap
                    | work_model::PauseKind::CandidateGate
                    | work_model::PauseKind::HostEvidence
            )
        )
        || attempt.review_state != Some(work_model::AttemptReviewState::Failed)
        || attempt
            .tasks
            .iter()
            .any(|task| task.status == TaskStatus::Executing)
        || item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id && candidate.candidate_commit == candidate_commit
        })
        || document.working_directory != "."
    {
        return Err(rejected());
    }
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .ok_or_else(rejected)?;
    if writer.commit != candidate_commit
        || !candidate_workspace_is_clean_at(project_root, writer, candidate_commit)
        || !attempt
            .artifacts
            .iter()
            .any(|artifact| artifact.path == gate_artifact)
    {
        return Err(rejected());
    }
    let gate_path = project_root.join(gate_artifact);
    let gate_bytes = fs::read(&gate_path).map_err(|_| rejected())?;
    let gate: crate::candidate_gate::CandidateGateEvidence =
        serde_json::from_slice(&gate_bytes).map_err(|_| rejected())?;
    let requested =
        crate::candidate_gate::requested_host_evidence_command(&gate).ok_or_else(rejected)?;
    if gate.candidate_commit != candidate_commit || document.check != requested {
        return Err(rejected());
    }
    let prior_tests_review = latest_passing_tests_review(project_root, attempt, candidate_commit)
        .cloned()
        .ok_or_else(rejected)?;
    let binding = CandidateGateEvidenceBinding {
        schema_version: 1,
        candidate_commit: candidate_commit.to_string(),
        gate_artifact: BoundRecheckArtifact {
            path: gate_artifact.to_string(),
            digest: format!("sha256:{:x}", Sha256::digest(&gate_bytes)),
        },
        gate_evidence: gate,
        command: requested,
        working_directory: ".".to_string(),
    };
    let binding_bytes = serde_json::to_vec_pretty(&binding).map_err(|_| rejected())?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    let binding_path =
        snapshot_path(item.id.as_str(), attempt_id, &binding_digest).map_err(|_| rejected())?;
    Ok(ValidatedCandidateGateFrontier {
        prior_tests_review,
        binding_path,
        binding_bytes,
    })
}

pub(crate) fn validate_evidence_frontier(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate_commit: &str,
    review_artifacts: &[String],
) -> Result<Vec<Task>, WorkModelError> {
    let rejected = || WorkModelError::HostEvidenceRecoveryIneligible {
        attempt_id: attempt_id.to_string(),
        reason: "the supplied evidence does not match an eligible failed-review frontier"
            .to_string(),
    };
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(rejected)?;
    if review_artifacts.is_empty()
        || review_artifacts.len()
            != review_artifacts
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        || attempt
            .tasks
            .iter()
            .any(|task| task.status == TaskStatus::Executing)
        || item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id && candidate.candidate_commit == candidate_commit
        })
    {
        return Err(rejected());
    }
    let completed_writer = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|(index, task)| task.output.as_ref().map(|output| (index, output)))
        .ok_or_else(rejected)?;
    if completed_writer.1.commit != candidate_commit
        || !candidate_workspace_is_clean_at(project_root, completed_writer.1, candidate_commit)
    {
        return Err(rejected());
    }
    let is_legacy = attempt.is_evidence_only_recoverable(candidate_commit, review_artifacts);
    let is_failed_review_pause = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::RoundCap | work_model::PauseKind::Uncertain)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let is_paused_evidence_recovery = attempt.status == work_model::AttemptStatus::NeedsUser
        && attempt.pause_kind == Some(work_model::PauseKind::Uncertain)
        && attempt.evidence_recoveries.last().is_some_and(|recovery| {
            recovery.state == EvidenceRecoveryState::NeedsEvidence
                && recovery.candidate_commit == candidate_commit
                && attempt.tasks.iter().any(|task| {
                    task.evidence_review_context
                        .as_ref()
                        .is_some_and(|context| {
                            context.recovery_id == recovery.id
                                && recovery
                                    .targets
                                    .iter()
                                    .any(|target| target.role == task.role)
                        })
                })
        });
    let interrupted_correction =
        interrupted_evidence_correction(attempt, candidate_commit, review_artifacts).filter(
            |task| {
                !crate::lease::is_leased(&crate::lease::task_lock_path(
                    project_root,
                    &attempt.work_item_id,
                    &task.id,
                ))
            },
        );
    if !(attempt.status == work_model::AttemptStatus::Reviewing
        || is_failed_review_pause
        || is_legacy
        || is_paused_evidence_recovery
        || interrupted_correction.is_some())
    {
        return Err(rejected());
    }
    let mut selected = Vec::new();
    for path in review_artifacts {
        let (index, task) = attempt
            .tasks
            .iter()
            .enumerate()
            .find(|(_, task)| review_artifact_path(task).as_deref() == Some(path.as_str()))
            .ok_or_else(rejected)?;
        if task.kind != TaskKind::Review
            || task.status != TaskStatus::Complete
            || task
                .review_context
                .as_ref()
                .map(|context| context.candidate_commit.as_str())
                != Some(candidate_commit)
            || !matches!(read_verdict(project_root, task), Some(Verdict::Fail))
            || index <= completed_writer.0
            || attempt.tasks.iter().skip(index + 1).any(|later| {
                later.kind == TaskKind::Review
                    && later.role == task.role
                    && later.status == TaskStatus::Complete
            })
            || selected.iter().any(|prior: &Task| prior.role == task.role)
            || (is_paused_evidence_recovery
                && !task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        attempt.evidence_recoveries.last().is_some_and(|recovery| {
                            context.recovery_id == recovery.id
                                && recovery.state == EvidenceRecoveryState::NeedsEvidence
                        })
                    }))
        {
            return Err(rejected());
        }
        selected.push(task.clone());
    }
    Ok(selected)
}

/// Resolve an output-free corrective Writer that was interrupted after an
/// evidence-targeted reviewer requested a code change.
///
/// The operator may replace the failed evidence only when the interrupted
/// Writer is the final Task, its exact inputs are the selected evidence Review,
/// and the prior recovery record binds that Review to the unchanged candidate.
/// This keeps ordinary interrupted Writers on their normal resume path.
fn interrupted_evidence_correction<'a>(
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
    review_artifacts: &[String],
) -> Option<&'a Task> {
    if attempt.status != work_model::AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(work_model::PauseKind::Interrupted)
        || attempt.review_state != Some(work_model::AttemptReviewState::Failed)
        || review_artifacts.is_empty()
    {
        return None;
    }
    let writer = attempt.tasks.last()?;
    if writer.kind != TaskKind::Write
        || writer.status != TaskStatus::NeedsUser
        || writer.output.is_some()
        || writer.input_artifacts.len() != review_artifacts.len()
    {
        return None;
    }
    let selected_reviews = writer
        .input_artifacts
        .iter()
        .map(|input| {
            attempt.tasks.iter().find(|task| {
                task.id == input.producer_id
                    && task.kind == TaskKind::Review
                    && task.status == TaskStatus::Complete
                    && review_artifact_path(task).as_deref() == Some(input.path.as_str())
                    && review_artifacts.contains(&input.path)
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if selected_reviews.len() != review_artifacts.len()
        || !review_artifacts.iter().all(|path| {
            writer
                .input_artifacts
                .iter()
                .any(|input| input.path == *path)
        })
    {
        return None;
    }
    attempt.evidence_recoveries.iter().rev().find(|recovery| {
        recovery.state == EvidenceRecoveryState::CodeChange
            && recovery.candidate_commit == candidate_commit
            && recovery.targets.len() == selected_reviews.len()
            && recovery.targets.iter().all(|target| {
                target.review_task_id.as_deref().is_some_and(|task_id| {
                    selected_reviews.iter().any(|review| {
                        review.id == task_id
                            && review.role == target.role
                            && review
                                .evidence_review_context
                                .as_ref()
                                .is_some_and(|context| {
                                    context.recovery_id == recovery.id
                                        && context.prior_review_artifact
                                            == target.prior_review_artifact
                                })
                    })
                })
            })
    })?;
    Some(writer)
}

/// Resolve the failed-command recheck that currently needs host evidence.
///
/// The canonical shape ends with the completed recheck. Fluent 0.2.0 could also
/// schedule one corrective Writer while capacity remained, give it the recheck
/// artifacts, accept a no-change declaration, and then terminalize the Attempt
/// after rejecting that same recheck as a source. Admit only that exact historical
/// shape so the public evidence command can repair it without rewriting history.
fn recoverable_failed_recheck<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
) -> Option<&'a Task> {
    if let Some(recheck) = attempt.tasks.last().filter(|task| {
        task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && task.is_failed_command_recheck()
            && task
                .review_context
                .as_ref()
                .is_some_and(|context| context.candidate_commit == candidate_commit)
    }) {
        return Some(recheck);
    }

    let writer = attempt.tasks.last()?;
    let output = writer.output.as_ref()?;
    if writer.kind != TaskKind::Write
        || writer.status != TaskStatus::Complete
        || output.no_change.is_none()
        || output.commit != candidate_commit
    {
        return None;
    }
    let result_sources = writer
        .input_artifacts
        .iter()
        .filter(|artifact| artifact.path.ends_with("/tester-results.json"))
        .collect::<Vec<_>>();
    let [result_source] = result_sources.as_slice() else {
        return None;
    };
    let recheck = attempt
        .tasks
        .iter()
        .rev()
        .skip(1)
        .find(|task| task.id == result_source.producer_id)?;
    if recheck.kind != TaskKind::Tester
        || recheck.status != TaskStatus::Complete
        || !recheck.is_failed_command_recheck()
        || recheck
            .review_context
            .as_ref()
            .is_none_or(|context| context.candidate_commit != candidate_commit)
    {
        return None;
    }
    let expected_results = recheck
        .artifact_area
        .as_ref()
        .map(|area| format!("{}/tester-results.json", area.path));
    if expected_results.as_deref() != Some(result_source.path.as_str()) {
        return None;
    }
    let correction =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .ok()?;
    let expected = correction
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    let actual = writer
        .input_artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    (expected.len() == correction.len()
        && actual.len() == writer.input_artifacts.len()
        && expected == actual)
        .then_some(recheck)
}

fn validate_recheck_evidence_frontier(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate_commit: &str,
    recheck_artifact: &str,
    document: &EvidenceDocument,
) -> Result<ValidatedRecheckFrontier, WorkModelError> {
    let rejected = || WorkModelError::HostEvidenceRecoveryIneligible {
        attempt_id: attempt_id.to_string(),
        reason:
            "the supplied evidence does not match the immutable failed-command recheck frontier"
                .to_string(),
    };
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(rejected)?;
    let paused_for_evidence = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::HostEvidence | work_model::PauseKind::RoundCap)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let legacy_terminal = attempt.status == work_model::AttemptStatus::Complete
        && attempt.pause_kind.is_none()
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    if !(paused_for_evidence || legacy_terminal)
        || attempt.has_round_cap_task_blocker()
        || item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id && candidate.candidate_commit == candidate_commit
        })
        || document.working_directory != "."
    {
        return Err(rejected());
    }
    let (_writer_index, writer_output) = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|(index, task)| task.output.as_ref().map(|output| (index, output)))
        .ok_or_else(rejected)?;
    if writer_output.commit != candidate_commit
        || !candidate_workspace_is_clean_at(project_root, writer_output, candidate_commit)
    {
        return Err(rejected());
    }
    let recheck =
        recoverable_failed_recheck(project_root, attempt, candidate_commit).ok_or_else(rejected)?;
    let area = recheck.artifact_area.as_ref().ok_or_else(rejected)?;
    let expected_manifest = format!("{}/failed-command-recheck.json", area.path);
    if recheck_artifact != expected_manifest {
        return Err(rejected());
    }
    let correction_artifacts =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .map_err(|_| rejected())?;
    let manifest_path = project_root.join(&expected_manifest);
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
    let results_artifact = correction_artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("/tester-results.json"))
        .ok_or_else(rejected)?;
    let results_path = project_root.join(&results_artifact.path);
    let results: crate::tester::TesterResults =
        serde_json::from_slice(&fs::read(&results_path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
    let [command_index] = manifest.failed_command_indexes.as_slice() else {
        return Err(rejected());
    };
    let [recheck_command] = results.commands.as_slice() else {
        return Err(rejected());
    };
    let exact_recheck_command = document.check == recheck_command.command;
    if !exact_recheck_command {
        if document.result == EvidenceResult::Pass {
            return Err(WorkModelError::HostEvidenceRecoveryIneligible {
                attempt_id: attempt_id.to_string(),
                reason: format!(
                    "passing evidence check {:?} does not match the failed-command recheck {:?}; rerun the exact command before claiming a pass",
                    document.check, recheck_command.command
                ),
            });
        }
        if validate_focused_command(&document.check).is_err() {
            return Err(WorkModelError::HostEvidenceRecoveryIneligible {
                attempt_id: attempt_id.to_string(),
                reason: format!(
                    "failed diagnostic evidence check {:?} must name one direct command",
                    document.check
                ),
            });
        }
    }
    if results.error.is_some()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &results_path,
            None,
            0,
            &recheck_command.command,
        ) != 0
        || recheck_command.exit_code == 0
        || recheck_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
    {
        return Err(rejected());
    }
    let source = recheck.input_artifacts.first().ok_or_else(rejected)?;
    if source.path != manifest.source_results {
        return Err(rejected());
    }
    let source_path = project_root.join(&source.path);
    let source_bytes = fs::read(&source_path).map_err(|_| rejected())?;
    let source_results: crate::tester::TesterResults =
        serde_json::from_slice(&source_bytes).map_err(|_| rejected())?;
    let source_command = source_results
        .commands
        .get(*command_index)
        .ok_or_else(rejected)?;
    if source_results.candidate_commit.as_deref() != Some(candidate_commit)
        || source_results.error.is_some()
        || source_command.exit_code == 0
        || source_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_command.command != recheck_command.command
        || source_command.test_harness != recheck_command.test_harness
        || source_results.summary.fail
            != source_results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            *command_index,
            &source_command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
    {
        return Err(rejected());
    }
    let prior_tests_review = latest_passing_tests_review(project_root, attempt, candidate_commit)
        .cloned()
        .ok_or_else(rejected)?;
    let bound_artifact = |artifact: &ArtifactRef| -> Result<BoundRecheckArtifact, WorkModelError> {
        let bytes = fs::read(project_root.join(&artifact.path)).map_err(|_| rejected())?;
        Ok(BoundRecheckArtifact {
            path: artifact.path.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
        })
    };
    let binding = FailedCommandRecheckEvidenceBinding {
        schema_version: 1,
        candidate_commit: candidate_commit.to_string(),
        recheck_task_id: recheck.id.clone(),
        source_results: BoundRecheckArtifact {
            path: source.path.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(&source_bytes)),
        },
        recheck_artifacts: correction_artifacts
            .iter()
            .map(bound_artifact)
            .collect::<Result<Vec<_>, _>>()?,
        command_index: *command_index,
        command: recheck_command.command.clone(),
        evidence_check: Some(document.check.clone()),
        working_directory: ".".to_string(),
    };
    let binding_bytes = serde_json::to_vec_pretty(&binding).map_err(|_| rejected())?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    let binding_path =
        snapshot_path(item.id.as_str(), attempt_id, &binding_digest).map_err(|_| rejected())?;
    Ok(ValidatedRecheckFrontier {
        prior_tests_review,
        binding,
        binding_path,
        binding_bytes,
        correction_artifacts,
    })
}

pub(crate) fn failed_recheck_evidence_recovery_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<String> {
    let paused_for_evidence = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::HostEvidence | work_model::PauseKind::RoundCap)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let legacy_terminal = attempt.status == work_model::AttemptStatus::Complete
        && attempt.pause_kind.is_none()
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    if !(paused_for_evidence || legacy_terminal) {
        return None;
    }
    pending_failed_recheck_evidence_recovery_action(project_root, attempt)
}

pub(crate) fn pending_failed_recheck_evidence_recovery_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<String> {
    let writer_output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)?
        .output
        .as_ref()?;
    let candidate = writer_output.commit.as_str();
    if !candidate_workspace_is_clean_at(project_root, writer_output, candidate)
        || latest_passing_tests_review(project_root, attempt, candidate).is_none()
    {
        return None;
    }
    let recheck = recoverable_failed_recheck(project_root, attempt, candidate)?;
    let area = recheck.artifact_area.as_ref()?;
    let manifest_path = format!("{}/failed-command-recheck.json", area.path);
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(project_root.join(&manifest_path)).ok()?).ok()?;
    let results: crate::tester::TesterResults = serde_json::from_slice(
        &fs::read(project_root.join(&area.path).join("tester-results.json")).ok()?,
    )
    .ok()?;
    let [command_index] = manifest.failed_command_indexes.as_slice() else {
        return None;
    };
    let [recheck_command] = results.commands.as_slice() else {
        return None;
    };
    let source = recheck.input_artifacts.first()?;
    let source_path = project_root.join(&source.path);
    let source_results: crate::tester::TesterResults =
        serde_json::from_slice(&fs::read(&source_path).ok()?).ok()?;
    let source_command = source_results.commands.get(*command_index)?;
    if manifest.candidate_commit != candidate
        || manifest.source_results != source.path
        || results.candidate_commit.as_deref() != Some(candidate)
        || results.error.is_some()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &project_root.join(&area.path).join("tester-results.json"),
            None,
            0,
            &recheck_command.command,
        ) != 0
        || recheck_command.exit_code == 0
        || recheck_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_results.candidate_commit.as_deref() != Some(candidate)
        || source_results.error.is_some()
        || source_command.exit_code == 0
        || source_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_command.command != recheck_command.command
        || source_command.test_harness != recheck_command.test_harness
        || source_results.summary.fail
            != source_results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            *command_index,
            &source_command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
        || crate::tester_evidence::failed_command_recheck_correction_artifacts(
            project_root,
            recheck,
        )
        .is_err()
    {
        return None;
    }
    Some(format!(
        "fluent attempt evidence attach {} {} --candidate {} --evidence-file <path> --recheck-artifact {}",
        attempt.work_item_id, attempt.id, candidate, manifest_path
    ))
}

pub(crate) fn completed_recheck_evidence_source(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<PathBuf> {
    attempt
        .evidence_recoveries
        .iter()
        .rev()
        .find_map(|recovery| {
            completed_recheck_evidence_source_for_recovery(project_root, attempt, recovery)
        })
}

fn completed_recheck_evidence_source_for_recovery(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Option<PathBuf> {
    if recovery.state != EvidenceRecoveryState::Complete
        || recovery.targets.len() != 1
        || recovery.targets[0].role != "tests"
    {
        return None;
    }
    let target = &recovery.targets[0];
    let binding_bytes = fs::read(project_root.join(&target.prior_review_artifact)).ok()?;
    let binding: FailedCommandRecheckEvidenceBinding =
        serde_json::from_slice(&binding_bytes).ok()?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    if binding.schema_version != 1
        || binding.candidate_commit != recovery.candidate_commit
        || snapshot_path(
            attempt.work_item_id.as_str(),
            attempt.id.as_str(),
            &binding_digest,
        )
        .ok()
        .as_deref()
            != Some(target.prior_review_artifact.as_str())
        || attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref())
            .map(|output| output.commit.as_str())
            != Some(recovery.candidate_commit.as_str())
    {
        return None;
    }
    let evidence_bytes = fs::read(project_root.join(&recovery.attachment.snapshot_path)).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest {
        return None;
    }
    let evidence = parse_evidence_document(&evidence_bytes).ok()?;
    let evidence_check = binding
        .evidence_check
        .as_deref()
        .unwrap_or(binding.command.as_str());
    if evidence.result != EvidenceResult::Pass
        || evidence.check != evidence_check
        || evidence_check != binding.command
        || evidence.working_directory != binding.working_directory
    {
        return None;
    }
    let review_task = target.review_task_id.as_ref().and_then(|id| {
        attempt.tasks.iter().find(|task| {
            task.id == *id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        context.recovery_id == recovery.id
                            && context.candidate_commit == recovery.candidate_commit
                            && context.prior_review_artifact == target.prior_review_artifact
                    })
        })
    })?;
    if read_verdict(project_root, review_task) != Some(Verdict::Pass) {
        return None;
    }
    let recheck = attempt.tasks.iter().find(|task| {
        task.id == binding.recheck_task_id
            && task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && task.is_failed_command_recheck()
    })?;
    let current_artifacts =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .ok()?;
    if current_artifacts.len() != binding.recheck_artifacts.len()
        || !current_artifacts
            .iter()
            .zip(&binding.recheck_artifacts)
            .all(|(current, bound)| {
                current.path == bound.path
                    && fs::read(project_root.join(&current.path)).is_ok_and(|bytes| {
                        format!("sha256:{:x}", Sha256::digest(&bytes)) == bound.digest
                    })
            })
    {
        return None;
    }
    let source_path = project_root.join(&binding.source_results.path);
    let source_bytes = fs::read(&source_path).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&source_bytes)) != binding.source_results.digest {
        return None;
    }
    let source: crate::tester::TesterResults = serde_json::from_slice(&source_bytes).ok()?;
    let command = source.commands.get(binding.command_index)?;
    if source.candidate_commit.as_deref() != Some(recovery.candidate_commit.as_str())
        || source.error.is_some()
        || command.exit_code == 0
        || command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || command.command != binding.command
        || source.summary.fail
            != source
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            binding.command_index,
            &command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
    {
        return None;
    }
    Some(source_path)
}

/// Resolve command-shaped Test references whose host execution was bound to a
/// rejected final candidate gate and accepted by a targeted Tests review.
pub(crate) fn completed_candidate_gate_evidence_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<String> {
    let latest_candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    let Some(latest_candidate) = latest_candidate else {
        return Vec::new();
    };
    let mut commands = attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| {
            if recovery.state != EvidenceRecoveryState::Complete
                || recovery.candidate_commit != latest_candidate
                || recovery.targets.len() != 1
                || recovery.targets[0].role != "tests"
            {
                return None;
            }
            let target = &recovery.targets[0];
            let binding_bytes = fs::read(project_root.join(&target.prior_review_artifact)).ok()?;
            let binding: CandidateGateEvidenceBinding =
                serde_json::from_slice(&binding_bytes).ok()?;
            let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
            if binding.schema_version != 1
                || binding.candidate_commit != recovery.candidate_commit
                || snapshot_path(
                    attempt.work_item_id.as_str(),
                    attempt.id.as_str(),
                    &binding_digest,
                )
                .ok()
                .as_deref()
                    != Some(target.prior_review_artifact.as_str())
            {
                return None;
            }
            if binding.gate_evidence.candidate_commit != recovery.candidate_commit
                || crate::candidate_gate::requested_host_evidence_command(&binding.gate_evidence)
                    .as_deref()
                    != Some(binding.command.as_str())
            {
                return None;
            }
            let evidence_bytes =
                fs::read(project_root.join(&recovery.attachment.snapshot_path)).ok()?;
            if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest
            {
                return None;
            }
            let evidence = parse_evidence_document(&evidence_bytes).ok()?;
            if evidence.result != EvidenceResult::Pass
                || evidence.check != binding.command
                || evidence.working_directory != binding.working_directory
            {
                return None;
            }
            let review_task_id = target.review_task_id.as_ref()?;
            let review = attempt.tasks.iter().find(|task| {
                task.id == *review_task_id
                    && task.kind == TaskKind::Review
                    && task.role == "tests"
                    && task.status == TaskStatus::Complete
                    && task
                        .evidence_review_context
                        .as_ref()
                        .is_some_and(|context| {
                            context.recovery_id == recovery.id
                                && context.candidate_commit == recovery.candidate_commit
                                && context.attachment == recovery.attachment
                                && context.prior_review_artifact == target.prior_review_artifact
                        })
            })?;
            (read_verdict(project_root, review) == Some(Verdict::Pass)).then_some(binding.command)
        })
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

/// Resolve focused Tests Review commands whose candidate-bound host evidence
/// was accepted by the targeted follow-up Review. Older Attempts can carry
/// this evidence before their final Tester, so candidate gating must credit
/// the exact command as well as the Tester settlement logic.
pub(crate) fn completed_review_evidence_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<String> {
    let latest_candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    let Some(latest_candidate) = latest_candidate else {
        return Vec::new();
    };
    let mut commands = attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| {
            if recovery.state != EvidenceRecoveryState::Complete
                || recovery.candidate_commit != latest_candidate
            {
                return None;
            }
            let evidence_bytes =
                fs::read(project_root.join(&recovery.attachment.snapshot_path)).ok()?;
            if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest
            {
                return None;
            }
            let evidence = parse_evidence_document(&evidence_bytes).ok()?;
            if evidence.result != EvidenceResult::Pass {
                return None;
            }

            recovery.targets.iter().find_map(|target| {
                if target.role != "tests" {
                    return None;
                }
                let prior_review =
                    fs::read_to_string(project_root.join(&target.prior_review_artifact)).ok()?;
                let request = review::extract_focused_evidence_request(&prior_review)?;
                if evidence.check != request.command
                    || request.working_directory != "."
                    || evidence.working_directory != request.working_directory
                {
                    return None;
                }
                let prior_task_is_bound = attempt.tasks.iter().any(|task| {
                    task.kind == TaskKind::Review
                        && task.role == "tests"
                        && task.status == TaskStatus::Complete
                        && task
                            .review_context
                            .as_ref()
                            .map(|context| context.candidate_commit.as_str())
                            == Some(latest_candidate)
                        && review_artifact_path(task).as_deref()
                            == Some(target.prior_review_artifact.as_str())
                        && read_verdict(project_root, task) == Some(Verdict::Fail)
                });
                let targeted_review_passed = target.review_task_id.as_ref().is_some_and(|id| {
                    attempt.tasks.iter().any(|task| {
                        task.id == *id
                            && task.kind == TaskKind::Review
                            && task.role == "tests"
                            && task.status == TaskStatus::Complete
                            && task
                                .evidence_review_context
                                .as_ref()
                                .is_some_and(|context| {
                                    context.recovery_id == recovery.id
                                        && context.candidate_commit == recovery.candidate_commit
                                        && context.attachment == recovery.attachment
                                        && context.prior_review_artifact
                                            == target.prior_review_artifact
                                })
                            && read_verdict(project_root, task) == Some(Verdict::Pass)
                    })
                });
                (prior_task_is_bound && targeted_review_passed).then_some(request.command)
            })
        })
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

pub(crate) fn pending_candidate_gate_evidence_recovery_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<CandidateGateEvidenceAction> {
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)?
        .output
        .as_ref()?;
    if !candidate_workspace_is_clean_at(project_root, writer, &writer.commit)
        || latest_passing_tests_review(project_root, attempt, &writer.commit).is_none()
    {
        return None;
    }
    attempt.artifacts.iter().rev().find_map(|artifact| {
        if !artifact.path.ends_with(crate::candidate_gate::CANDIDATE_GATE_FILE) {
            return None;
        }
        let gate: crate::candidate_gate::CandidateGateEvidence = serde_json::from_slice(
            &fs::read(project_root.join(&artifact.path)).ok()?,
        )
        .ok()?;
        let command = crate::candidate_gate::requested_host_evidence_command(&gate)?;
        if gate.candidate_commit != writer.commit
            || completed_candidate_gate_evidence_commands(project_root, attempt)
                .iter()
                .any(|accepted| accepted == &command)
        {
            return None;
        }
        Some(CandidateGateEvidenceAction {
            command,
            gate_artifact: artifact.path.clone(),
            attach_command: format!(
                "fluent attempt evidence attach {} {} --candidate {} --evidence-file <path> --review-artifact {}",
                attempt.work_item_id, attempt.id, writer.commit, artifact.path
            ),
        })
    })
}

/// Return true when one otherwise-unexplained final Tester command is exactly
/// the focused command already accepted for this candidate by a targeted Tests
/// review. This is the pre-Tester evidence path: the canonical Tester still
/// runs the complete suite, but a host-only command failure must not send the
/// unchanged candidate through a Writer and failed-command recheck again.
pub(crate) fn completed_review_evidence_covers_tester_failure(
    project_root: &Path,
    attempt: &work_model::Attempt,
    tester_results_path: &Path,
    baseline_path: Option<&Path>,
) -> bool {
    let results: crate::tester::TesterResults = match fs::read(tester_results_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(results) => results,
        None => return false,
    };
    let Some(candidate) = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str())
    else {
        return false;
    };
    if results.candidate_commit.as_deref() != Some(candidate)
        || results.error.is_some()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::unexplained_failed_tester_commands(
            tester_results_path,
            baseline_path,
        ) != 1
    {
        return false;
    }
    let mut failed = results
        .commands
        .iter()
        .enumerate()
        .filter(|(_, command)| command.exit_code != 0);
    let Some((command_index, command)) = failed.next() else {
        return false;
    };
    if failed.next().is_some()
        || command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || crate::tester_evidence::introduced_tester_failures_except_command(
            tester_results_path,
            baseline_path,
            command_index,
            &command.command,
        ) != 0
    {
        return false;
    }

    completed_review_evidence_commands(project_root, attempt)
        .iter()
        .any(|accepted| accepted == &command.command)
}

fn candidate_workspace_is_clean_at(
    project_root: &Path,
    output: &crate::work_model::TaskOutput,
    candidate: &str,
) -> bool {
    crate::recovery_contracts::candidate_workspace_is_clean_at(project_root, output, candidate)
}

fn snapshot_path(work: &str, attempt: &str, digest: &str) -> Result<String> {
    crate::recovery_contracts::snapshot_path(work, attempt, digest)
}

fn review_artifact_path(task: &Task) -> Option<String> {
    crate::recovery_contracts::review_artifact_path(task)
}

fn read_verdict(project_root: &Path, task: &Task) -> Option<Verdict> {
    fs::read_to_string(project_root.join(review_artifact_path(task)?))
        .ok()
        .map(|text| review::extract_verdict(&text))
}

fn latest_passing_tests_review<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate: &str,
) -> Option<&'a Task> {
    attempt
        .tasks
        .iter()
        .rev()
        .find(|task| {
            task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task
                    .review_context
                    .as_ref()
                    .map(|context| context.candidate_commit.as_str())
                    == Some(candidate)
        })
        .filter(|task| read_verdict(project_root, task) == Some(Verdict::Pass))
}

use crate::work_model::publish_recovery_snapshot as publish_snapshot;

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_gate_evidence_fixture(
        command: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        WorkModelStore,
        String,
        String,
        PathBuf,
    ) {
        let project = tempfile::TempDir::new().unwrap();
        let workspace = tempfile::TempDir::new().unwrap();
        crate::git::run(
            workspace.path(),
            &["init", "-q"],
            "initialize evidence fixture",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["config", "user.name", "Test Author"],
            "configure fixture author",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["config", "user.email", "test@example.com"],
            "configure fixture email",
        )
        .unwrap();
        fs::write(workspace.path().join("README.md"), "base\n").unwrap();
        crate::git::run(
            workspace.path(),
            &["add", "README.md"],
            "stage fixture base",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["commit", "-q", "-m", "Create fixture"],
            "commit fixture base",
        )
        .unwrap();
        let base = crate::git::run_stdout(
            workspace.path(),
            &["rev-parse", "HEAD"],
            "read fixture base",
        )
        .unwrap();
        fs::create_dir_all(workspace.path().join("tests")).unwrap();
        fs::write(
            workspace.path().join("tests/readiness.py"),
            "#!/usr/bin/env python3\n",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["add", "tests/readiness.py"],
            "stage candidate",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["commit", "-q", "-m", "Add readiness check"],
            "commit candidate",
        )
        .unwrap();
        let candidate = crate::git::run_stdout(
            workspace.path(),
            &["rev-parse", "HEAD"],
            "read fixture candidate",
        )
        .unwrap();

        let store = WorkModelStore::new(project.path());
        let mut item = work_model::WorkItem::planned("work-1", "Attach gate evidence");
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(work_model::TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: workspace.path().display().to_string(),
            source_branch: "main".to_string(),
            base_commit: Some(base.clone()),
            commit: candidate.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        item.add_review_tasks("attempt-1", &["tests"]).unwrap();
        let review = item.attempts[0]
            .tasks
            .iter_mut()
            .find(|task| task.kind == TaskKind::Review)
            .unwrap();
        work_model::set_task_terminal(review, TaskStatus::Complete);
        let review_path = format!("{}/review.md", review.artifact_area.as_ref().unwrap().path);
        fs::create_dir_all(project.path().join(&review_path).parent().unwrap()).unwrap();
        fs::write(project.path().join(&review_path), "Verdict: pass\n").unwrap();

        let gate_path = work_model::work_artifact_path(
            "work-1",
            "attempt-1",
            "attempt-1-tester-final-candidate-gate",
        ) + "/candidate-gate.json";
        let gate = crate::candidate_gate::CandidateGateEvidence {
            schema_version: crate::candidate_gate::CANDIDATE_GATE_SCHEMA_VERSION,
            phase: crate::candidate_gate::CandidateGatePhase::FinalTester,
            disposition: crate::candidate_gate::CandidateGateDisposition::Rejected,
            base_commit: base,
            candidate_commit: candidate.clone(),
            tester_results_path: Some("tester-results.json".to_string()),
            metadata_rewrite: None,
            checks: vec![crate::candidate_gate::CandidateCheckEvidence {
                name: "tester-reference-evidence".to_string(),
                status: crate::candidate_gate::CandidateCheckStatus::Failed,
                diagnostics: vec![format!("no passing Tester result matches {command}")],
                log_path: None,
            }],
        };
        fs::create_dir_all(project.path().join(&gate_path).parent().unwrap()).unwrap();
        fs::write(
            project.path().join(&gate_path),
            serde_json::to_vec_pretty(&gate).unwrap(),
        )
        .unwrap();
        item.attempts[0].artifacts.push(ArtifactRef {
            producer_id: "candidate-gate".to_string(),
            path: gate_path.clone(),
        });
        item.attempts[0].review_state = Some(work_model::AttemptReviewState::Failed);
        work_model::suspend_attempt(&mut item.attempts[0], work_model::PauseKind::HostEvidence);
        store.create_work_item(&item).unwrap();

        let evidence_path = project.path().join("evidence.json");
        let document = EvidenceDocument {
            schema_version: 1,
            producer: "authorized host".to_string(),
            check: command.to_string(),
            working_directory: ".".to_string(),
            result: EvidenceResult::Pass,
            run_at: "2026-08-25T12:00:00Z".to_string(),
            output: "passed on the authorized host".to_string(),
        };
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        (
            project,
            workspace,
            store,
            candidate,
            gate_path,
            evidence_path,
        )
    }

    fn plan_host_evidence_fixture(
        command: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        WorkModelStore,
        String,
        PathBuf,
    ) {
        plan_host_evidence_fixture_with_row(command, "Host check passed", command)
    }

    fn plan_host_evidence_fixture_with_row(
        command: &str,
        deliverable: &str,
        verification: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        WorkModelStore,
        String,
        PathBuf,
    ) {
        let project = tempfile::TempDir::new().unwrap();
        let workspace = tempfile::TempDir::new().unwrap();
        crate::git::run(
            workspace.path(),
            &["init", "-q", "-b", "main"],
            "initialize Plan-row evidence fixture",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["config", "user.name", "Test Author"],
            "configure fixture author",
        )
        .unwrap();
        crate::git::run(
            workspace.path(),
            &["config", "user.email", "test@example.com"],
            "configure fixture email",
        )
        .unwrap();
        fs::write(workspace.path().join("README.md"), "base\n").unwrap();
        fs::create_dir_all(workspace.path().join("tests")).unwrap();
        fs::write(
            workspace.path().join("tests/readiness.py"),
            "#!/usr/bin/env python3\n",
        )
        .unwrap();
        crate::git::run(workspace.path(), &["add", "."], "stage Plan-row fixture").unwrap();
        crate::git::run(
            workspace.path(),
            &["commit", "-q", "-m", "Add readiness check"],
            "commit Plan-row fixture",
        )
        .unwrap();
        let candidate = crate::git::run_stdout(
            workspace.path(),
            &["rev-parse", "HEAD"],
            "read Plan-row fixture candidate",
        )
        .unwrap();

        let mut item = work_model::WorkItem::planned("work-1", "Attach Plan-row evidence");
        item.planning_context = Some(work_model::PlanningContext {
            plan: Some(format!(
                "| # | State reached | Behaviors | Verification | Req? |\n\
                 |---|---------------|-----------|--------------|------|\n\
                 | 6 | {deliverable} | | {verification} | Yes |\n"
            )),
            ..Default::default()
        });
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(work_model::TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: workspace.path().display().to_string(),
            source_branch: "main".to_string(),
            base_commit: None,
            commit: candidate.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        let writer_id = writer.id.clone();
        let matrix_path = format!(
            "{}/{}",
            writer.artifact_area.as_ref().unwrap().path,
            crate::recovery_contracts::WRITER_COMPLETION_MATRIX_FILE
        );
        let contract = item.attempts[0]
            .writer_completion_contract
            .as_ref()
            .unwrap()
            .clone();
        let matrix = WriterCompletionMatrix::skeleton(&contract, &writer_id, &[], None).unwrap();
        fs::create_dir_all(project.path().join(&matrix_path).parent().unwrap()).unwrap();
        fs::write(
            project.path().join(&matrix_path),
            serde_json::to_vec_pretty(&matrix).unwrap(),
        )
        .unwrap();
        item.attempts[0].status = work_model::AttemptStatus::NeedsUser;
        item.attempts[0].pause_kind = Some(work_model::PauseKind::WriterCompletion);
        item.attempts[0].review_state = Some(work_model::AttemptReviewState::NotReviewed);
        let store = WorkModelStore::new(project.path());
        store.create_work_item(&item).unwrap();

        let evidence_path = project.path().join("host-evidence.json");
        let document = EvidenceDocument {
            schema_version: 1,
            producer: "authorized host".to_string(),
            check: command.to_string(),
            working_directory: ".".to_string(),
            result: EvidenceResult::Pass,
            run_at: "2026-08-25T12:00:00Z".to_string(),
            output: "passed on the authorized host".to_string(),
        };
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        (project, workspace, store, candidate, evidence_path)
    }

    /// Build the same pre-review Plan-row state with the candidate registered
    /// in the Work Item's own Git repository. Source-correction recovery has a
    /// stronger clean-tree check than initial host attachment, so this fixture
    /// exercises that public boundary rather than using a detached test repo.
    fn managed_plan_host_evidence_fixture(
        command: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        WorkModelStore,
        String,
        PathBuf,
    ) {
        let project = tempfile::TempDir::new().unwrap();
        let evidence_dir = tempfile::TempDir::new().unwrap();
        crate::git::run(
            project.path(),
            &["init", "-q", "-b", "main"],
            "initialize managed Plan-row evidence fixture",
        )
        .unwrap();
        crate::git::run(
            project.path(),
            &["config", "user.name", "Test Author"],
            "configure managed fixture author",
        )
        .unwrap();
        crate::git::run(
            project.path(),
            &["config", "user.email", "test@example.com"],
            "configure managed fixture email",
        )
        .unwrap();
        fs::write(project.path().join(".gitignore"), ".fluent/\n").unwrap();
        fs::write(project.path().join("README.md"), "base\n").unwrap();
        fs::create_dir_all(project.path().join("tests")).unwrap();
        fs::write(
            project.path().join("tests/readiness.py"),
            "#!/usr/bin/env python3\n",
        )
        .unwrap();
        crate::git::run(project.path(), &["add", "."], "stage managed fixture").unwrap();
        crate::git::run(
            project.path(),
            &["commit", "-q", "-m", "Add readiness check"],
            "commit managed fixture",
        )
        .unwrap();
        let candidate = crate::git::run_stdout(
            project.path(),
            &["rev-parse", "HEAD"],
            "read managed fixture candidate",
        )
        .unwrap();

        let mut item = work_model::WorkItem::planned("work-1", "Attach managed Plan-row evidence");
        item.planning_context = Some(work_model::PlanningContext {
            plan: Some(format!(
                "| # | State reached | Behaviors | Verification | Req? |\n\
                 |---|---------------|-----------|--------------|------|\n\
                 | 6 | Host check | | {command} | Yes |\n"
            )),
            ..Default::default()
        });
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(work_model::TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: project.path().display().to_string(),
            source_branch: "main".to_string(),
            base_commit: None,
            commit: candidate.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        let writer_id = writer.id.clone();
        let matrix_path = format!(
            "{}/{}",
            writer.artifact_area.as_ref().unwrap().path,
            crate::recovery_contracts::WRITER_COMPLETION_MATRIX_FILE
        );
        let contract = item.attempts[0]
            .writer_completion_contract
            .as_ref()
            .unwrap()
            .clone();
        let matrix = WriterCompletionMatrix::skeleton(&contract, &writer_id, &[], None).unwrap();
        fs::create_dir_all(project.path().join(&matrix_path).parent().unwrap()).unwrap();
        fs::write(
            project.path().join(&matrix_path),
            serde_json::to_vec_pretty(&matrix).unwrap(),
        )
        .unwrap();
        item.attempts[0].status = work_model::AttemptStatus::NeedsUser;
        item.attempts[0].pause_kind = Some(work_model::PauseKind::WriterCompletion);
        item.attempts[0].review_state = Some(work_model::AttemptReviewState::NotReviewed);
        let store = WorkModelStore::new(project.path());
        store.create_work_item(&item).unwrap();

        let evidence_path = evidence_dir.path().join("host-evidence.json");
        let document = EvidenceDocument {
            schema_version: 1,
            producer: "authorized host".to_string(),
            check: command.to_string(),
            working_directory: ".".to_string(),
            result: EvidenceResult::Pass,
            run_at: "2026-08-25T12:00:00Z".to_string(),
            output: "passed on the authorized host".to_string(),
        };
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        (project, evidence_dir, store, candidate, evidence_path)
    }

    #[test]
    fn evidence_document_requires_versioned_complete_host_claim() {
        let accepted = br#"{"schema_version":1,"producer":"host","check":"fluent tester check","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"ok"}"#;
        assert_eq!(
            parse_evidence_document(accepted).unwrap().result,
            EvidenceResult::Pass
        );

        let missing_check = br#"{"schema_version":1,"producer":"host","check":"","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"ok"}"#;
        assert!(parse_evidence_document(missing_check).is_err());
    }

    #[test]
    fn pending_host_verification_blocks_readiness() {
        let command = "bash tests/host.sh";
        let (project, workspace, store, candidate, evidence_path) =
            plan_host_evidence_fixture(command);
        let mut item = store.read_work_item("work-1").unwrap();
        let attempt = &mut item.attempts[0];
        attempt
            .writer_completion_contract
            .as_mut()
            .unwrap()
            .requirements[0]
            .responsibility = Some(work_model::CompletionResponsibility::Host);
        let writer = &attempt.tasks[0];
        let matrix_ref = writer_completion_matrix_ref(project.path(), attempt, writer).unwrap();
        let mut matrix: WriterCompletionMatrix =
            serde_json::from_slice(&fs::read(project.path().join(&matrix_ref.path)).unwrap())
                .unwrap();
        matrix.rows[0].responsibility = Some(work_model::CompletionResponsibility::Host);
        fs::write(
            project.path().join(&matrix_ref.path),
            serde_json::to_vec_pretty(&matrix).unwrap(),
        )
        .unwrap();
        store.write_work_item(&item).unwrap();
        assert!(
            required_host_verification_pending(project.path(), &item.attempts[0])
                .unwrap()
                .is_some()
        );
        let recovery = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            "plan:step-6",
            &evidence_path,
        )
        .unwrap();
        let mut item = store.read_work_item("work-1").unwrap();
        assert!(
            required_host_verification_pending(project.path(), &item.attempts[0])
                .unwrap()
                .is_some(),
            "attachment alone cannot pass a required check"
        );
        let attempt = &mut item.attempts[0];
        let review = attempt
            .tasks
            .iter_mut()
            .find(|task| Some(&task.id) == recovery.targets[0].review_task_id.as_ref())
            .unwrap();
        let review_path = project
            .path()
            .join(&review.artifact_area.as_ref().unwrap().path)
            .join("review.md");
        fs::create_dir_all(review_path.parent().unwrap()).unwrap();
        fs::write(&review_path, "Verdict: pass\n").unwrap();
        work_model::set_task_terminal(review, TaskStatus::Complete);
        attempt.evidence_recoveries[0].state = EvidenceRecoveryState::Complete;
        assert!(
            required_host_verification_pending(project.path(), attempt)
                .unwrap()
                .is_none()
        );
        fs::write(&review_path, "Verdict: fail\n").unwrap();
        assert!(
            required_host_verification_pending(project.path(), attempt)
                .unwrap()
                .is_some()
        );
        fs::write(&review_path, "Verdict: pass\n").unwrap();
        let snapshot = project.path().join(&recovery.attachment.snapshot_path);
        let saved = fs::read(&snapshot).unwrap();
        fs::write(&snapshot, b"changed evidence").unwrap();
        assert!(
            required_host_verification_pending(project.path(), attempt)
                .unwrap()
                .is_some()
        );
        fs::write(&snapshot, saved).unwrap();
        fs::write(workspace.path().join("README.md"), "dirty\n").unwrap();
        assert!(required_host_verification_pending(project.path(), attempt).is_err());
    }

    #[test]
    fn plan_row_host_evidence_binds_unchanged_candidate_and_is_idempotent() {
        let command = "python3 tests/readiness.py";
        let (project, _workspace, store, candidate, evidence_path) =
            plan_host_evidence_fixture(command);

        let before = store.read_work_item("work-1").unwrap();
        let action = pending_plan_host_evidence_action(project.path(), &before.attempts[0])
            .expect("a pending Plan row advertises an authorized-host recovery");
        assert_eq!(action.plan_row, "plan:step-6");
        assert_eq!(action.command, command);
        assert_eq!(action.candidate_commit, candidate);
        let status_action = crate::work_status::attempt_recovery_next_action(
            Some(&before.attempts[0]),
            Some(project.path()),
        )
        .expect("status should expose the Plan-row attachment command");
        assert!(status_action.starts_with(&format!(
            "fluent attempt evidence attach work-1 attempt-1 --candidate {candidate}"
        )));
        assert!(status_action.ends_with("--evidence-file <path> --plan-row plan:step-6"));

        assert!(
            attach_plan_row(
                project.path(),
                &store,
                "work-1",
                "attempt-1",
                "stale-candidate",
                &action.plan_row,
                &evidence_path,
            )
            .is_err(),
            "host evidence must remain candidate-bound"
        );

        let recovery = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &action.plan_row,
            &evidence_path,
        )
        .unwrap();
        let after = store.read_work_item("work-1").unwrap();
        assert_eq!(
            after.attempts[0].status,
            work_model::AttemptStatus::Reviewing
        );
        assert_eq!(
            after.attempts[0]
                .tasks
                .iter()
                .filter(|task| task.kind == TaskKind::Write)
                .count(),
            1
        );
        assert_eq!(after.attempts[0].evidence_recoveries.len(), 1);
        assert_eq!(after.attempts[0].recovery_operations.len(), 1);
        assert_eq!(
            after.attempts[0].recovery_operations[0].operation_kind,
            "host-evidence-plan-row-attachment"
        );
        assert_eq!(recovery.targets.len(), 1);
        let review_id = recovery.targets[0].review_task_id.clone().unwrap();
        assert!(after.attempts[0].tasks.iter().any(|task| {
            task.id == review_id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Planned
        }));

        let repeated = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &action.plan_row,
            &evidence_path,
        )
        .unwrap();
        assert_eq!(repeated, recovery);
        let repeated_item = store.read_work_item("work-1").unwrap();
        assert_eq!(repeated_item.attempts[0].evidence_recoveries.len(), 1);
        assert_eq!(repeated_item.attempts[0].recovery_operations.len(), 1);
        assert_eq!(
            repeated_item.attempts[0]
                .tasks
                .iter()
                .filter(|task| task.id == review_id)
                .count(),
            1
        );

        store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                let review = attempt
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == review_id)
                    .unwrap();
                let review_path = review.artifact_area.as_ref().unwrap().path.clone();
                fs::create_dir_all(project.path().join(&review_path)).unwrap();
                fs::write(
                    project.path().join(&review_path).join("review.md"),
                    "Verdict: pass\n",
                )
                .unwrap();
                work_model::set_task_terminal(review, TaskStatus::Complete);
                attempt.evidence_recoveries[0].state = EvidenceRecoveryState::Complete;
                Ok(())
            })
            .unwrap();

        let completed = store.read_work_item("work-1").unwrap();
        let attempt = &completed.attempts[0];
        let writer = attempt
            .tasks
            .iter()
            .find(|task| task.kind == TaskKind::Write)
            .unwrap();
        let host_matrix = completed_plan_host_matrix_ref(project.path(), attempt, writer)
            .expect("a passing targeted Tests Review unlocks the host completion matrix");
        let source = writer_completion_matrix_ref(project.path(), attempt, writer).unwrap();
        let source_bytes = fs::read(project.path().join(&source.path)).unwrap();
        let mut projected: WriterCompletionMatrix = serde_json::from_slice(&source_bytes).unwrap();
        assert!(apply_completed_plan_host_evidence(
            project.path(),
            attempt,
            writer,
            &source,
            &mut projected,
        ));
        assert_eq!(
            projected
                .rows
                .iter()
                .find(|row| row.id == action.plan_row)
                .unwrap()
                .state,
            WriterCompletionState::Complete
        );
        assert!(host_matrix.path.ends_with("writer-completion.json"));
        assert_eq!(
            fs::read(project.path().join(&source.path)).unwrap(),
            source_bytes,
            "the immutable Writer matrix is not rewritten by host recovery"
        );
    }

    #[test]
    fn plan_row_host_evidence_resolves_command_from_deliverable_prose() {
        let command = "DGX_BRING_UP_LIVE=1 ./tests/dgx-bring-up-smoke.sh";
        let deliverable = format!("Run {command} unchanged on spark1.");
        let verification = "The live smoke preserves isolation and reports content-free evidence.";
        let (project, _workspace, store, candidate, evidence_path) =
            plan_host_evidence_fixture_with_row(command, &deliverable, verification);

        let before = store.read_work_item("work-1").unwrap();
        let action = pending_plan_host_evidence_action(project.path(), &before.attempts[0])
            .expect("the command in the approved Deliverable is recoverable");
        assert_eq!(action.command, command);
        assert_eq!(action.plan_row, "plan:step-6");

        let recovery = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &action.plan_row,
            &evidence_path,
        )
        .expect("the extracted command should bind unchanged-candidate evidence");
        assert_eq!(recovery.candidate_commit, candidate);
        assert_eq!(recovery.targets.len(), 1);

        let binding: serde_json::Value = serde_json::from_slice(
            &fs::read(
                project
                    .path()
                    .join(&recovery.targets[0].prior_review_artifact),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            binding["command-source"], "structured:plan:step-6",
            "the evidence record identifies the structured command source"
        );

        let review_id = recovery.targets[0].review_task_id.clone().unwrap();
        store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                let review = attempt
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == review_id)
                    .unwrap();
                let review_path = review.artifact_area.as_ref().unwrap().path.clone();
                fs::create_dir_all(project.path().join(&review_path)).unwrap();
                fs::write(
                    project.path().join(&review_path).join("review.md"),
                    "Verdict: pass\n",
                )
                .unwrap();
                work_model::set_task_terminal(review, TaskStatus::Complete);
                attempt.evidence_recoveries[0].state = EvidenceRecoveryState::Complete;
                Ok(())
            })
            .unwrap();

        let completed = store.read_work_item("work-1").unwrap();
        let attempt = &completed.attempts[0];
        let writer = attempt
            .tasks
            .iter()
            .find(|task| task.kind == TaskKind::Write)
            .unwrap();
        let source = writer_completion_matrix_ref(project.path(), attempt, writer).unwrap();
        let source_bytes = fs::read(project.path().join(&source.path)).unwrap();
        let mut projected: WriterCompletionMatrix = serde_json::from_slice(&source_bytes).unwrap();
        assert!(apply_completed_plan_host_evidence(
            project.path(),
            attempt,
            writer,
            &source,
            &mut projected,
        ));
        assert_eq!(
            projected
                .rows
                .iter()
                .find(|row| row.id == action.plan_row)
                .unwrap()
                .state,
            WriterCompletionState::Complete
        );
    }

    #[test]
    fn plan_row_failed_review_with_legacy_not_reviewed_state_can_start_correction() {
        let command = "DGX_BRING_UP_LIVE=1 ./tests/dgx-bring-up-smoke.sh";
        let (project, _evidence_dir, store, candidate, evidence_path) =
            managed_plan_host_evidence_fixture(command);

        let mut document: EvidenceDocument =
            serde_json::from_slice(&fs::read(&evidence_path).unwrap()).unwrap();
        document.result = EvidenceResult::Fail;
        document.output = "the authorized host reported a network mismatch".to_string();
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        let before = store.read_work_item("work-1").unwrap();
        let action = pending_plan_host_evidence_action(project.path(), &before.attempts[0])
            .expect("the Plan row should advertise its host command");
        let recovery = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &action.plan_row,
            &evidence_path,
        )
        .unwrap();
        let review_id = recovery.targets[0]
            .review_task_id
            .clone()
            .expect("Plan-row attachment creates a targeted Tests Review");
        let review_path = store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                let review = attempt
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == review_id)
                    .unwrap();
                let path = format!("{}/review.md", review.artifact_area.as_ref().unwrap().path);
                fs::create_dir_all(project.path().join(&path).parent().unwrap()).unwrap();
                fs::write(
                    project.path().join(&path),
                    "Verdict: fail\nDisposition: evidence-needed\n",
                )
                .unwrap();
                work_model::set_task_terminal(review, TaskStatus::Complete);
                attempt.status = work_model::AttemptStatus::NeedsUser;
                attempt.pause_kind = Some(work_model::PauseKind::Uncertain);
                // This is the persisted shape produced by the affected older
                // Plan-row recovery path.
                attempt.review_state = Some(work_model::AttemptReviewState::NotReviewed);
                attempt.evidence_recoveries[0].state = EvidenceRecoveryState::NeedsEvidence;
                Ok(path)
            })
            .unwrap();
        let evidence_digest = recovery.attachment.digest.clone();
        let resolution = "Authorize a corrective Writer for the failed authorized-host smoke.";

        let first = classify_for_code_change(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &review_path,
            &evidence_digest,
            resolution,
        )
        .expect("the exact Plan-row recovery should be classifiable");
        assert_eq!(first.writer_task_id, "attempt-1-write-2");

        let after_first = store.read_work_item("work-1").unwrap();
        let attempt = &after_first.attempts[0];
        assert_eq!(attempt.status, work_model::AttemptStatus::Planned);
        assert_eq!(
            attempt.review_state,
            Some(work_model::AttemptReviewState::Failed)
        );
        assert_eq!(
            attempt.evidence_recoveries[0].state,
            EvidenceRecoveryState::CodeChange
        );
        assert!(
            attempt
                .tasks
                .iter()
                .any(|task| { task.id == review_id && task.status == TaskStatus::Complete })
        );
        assert!(attempt.tasks.iter().any(|task| {
            task.id == first.writer_task_id
                && task.kind == TaskKind::Write
                && task.status == TaskStatus::Planned
                && task.input_artifacts.iter().any(|artifact| {
                    artifact.producer_id == review_id && artifact.path == review_path
                })
        }));
        assert!(project.path().join(&first.audit_artifact).is_file());
        crate::recovery_contracts::validate_code_change_writer_start(
            project.path(),
            &after_first,
            "attempt-1",
            &first.writer_task_id,
        )
        .expect("the recovered Writer must pass normal queue admission");

        // Repeating the exact public command must return the original Writer
        // and audit without appending another Task or changing the evidence,
        // Review, or candidate history.
        let snapshot = serde_json::to_vec(&after_first).unwrap();
        let second = classify_for_code_change(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &review_path,
            &evidence_digest,
            resolution,
        )
        .expect("classification replay should be idempotent");
        assert_eq!(second, first);
        assert_eq!(
            serde_json::to_vec(&store.read_work_item("work-1").unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn structured_plan_command_wins_over_explanatory_text() {
        let row = WriterCompletionRow {
            responsibility: None,
            id: "plan:step-6".to_string(),
            kind: WriterCompletionRequirementKind::Plan,
            source: "plan.md#step-6".to_string(),
            requirement: "Run python3 tests/old-check.py unchanged on spark1.".to_string(),
            verification_refs: vec![
                "The check preserves isolation and reports content-free evidence.".to_string(),
            ],
            command: Some(work_model::PlanCommand {
                command: "python3 tests/fluent_merge_readiness.py".to_string(),
                working_directory: ".".to_string(),
                environment: std::collections::BTreeMap::new(),
                source_row: "plan:step-6".to_string(),
            }),
            state: WriterCompletionState::Pending,
            implementation_evidence: Vec::new(),
            verification: Vec::new(),
            applicable_constraints: Vec::new(),
        };

        assert_eq!(
            resolve_plan_row_command(&row).as_deref(),
            Some("python3 tests/fluent_merge_readiness.py"),
            "host evidence must use the command captured at planning time"
        );
        assert_eq!(
            resolve_plan_row_command_with_source(&row),
            Some(ResolvedPlanCommand {
                command: "python3 tests/fluent_merge_readiness.py".to_string(),
                source: "structured:plan:step-6".to_string(),
            })
        );

        let mut mismatched = row;
        mismatched.command.as_mut().unwrap().source_row = "plan:step-5".to_string();
        assert!(resolve_plan_row_command_with_source(&mismatched).is_none());
    }

    #[test]
    fn legacy_plan_command_resolution_records_which_text_field_supplied_it() {
        let row = WriterCompletionRow {
            responsibility: None,
            id: "plan:step-6".to_string(),
            kind: WriterCompletionRequirementKind::Plan,
            source: "plan.md#step-6".to_string(),
            requirement: "Run python3 tests/readiness.py unchanged on spark1.".to_string(),
            verification_refs: vec!["The smoke reports content-free evidence.".to_string()],
            command: None,
            state: WriterCompletionState::Pending,
            implementation_evidence: Vec::new(),
            verification: Vec::new(),
            applicable_constraints: Vec::new(),
        };

        assert_eq!(
            resolve_plan_row_command_with_source(&row),
            Some(ResolvedPlanCommand {
                command: "python3 tests/readiness.py".to_string(),
                source: "legacy:requirement".to_string(),
            })
        );
    }

    #[test]
    fn plan_row_host_evidence_resolves_inline_command_and_rejects_ambiguity() {
        let command = "DGX_BRING_UP_LIVE=1 ./tests/dgx-bring-up-smoke.sh";
        let inline_verification = format!("The exact `{command}` runs on spark1.");
        let (project, _workspace, store, _candidate, _evidence_path) =
            plan_host_evidence_fixture_with_row(command, "Host smoke", &inline_verification);
        let before = store.read_work_item("work-1").unwrap();
        assert_eq!(
            pending_plan_host_evidence_action(project.path(), &before.attempts[0])
                .unwrap()
                .command,
            command
        );

        let ambiguous = format!("Run {command}; then run python3 tests/other.py.");
        let (project, _workspace, store, _candidate, _evidence_path) =
            plan_host_evidence_fixture_with_row(command, "Host smoke", &ambiguous);
        let before = store.read_work_item("work-1").unwrap();
        assert!(
            pending_plan_host_evidence_action(project.path(), &before.attempts[0]).is_none(),
            "multiple executable references must remain an explicit ambiguity"
        );
    }

    #[test]
    fn plan_row_host_evidence_rejects_wrong_command_without_mutation() {
        let command = "python3 tests/readiness.py";
        let (project, _workspace, store, candidate, evidence_path) =
            plan_host_evidence_fixture(command);
        let mut document = parse_evidence_document(&fs::read(&evidence_path).unwrap()).unwrap();
        document.check = "python3 tests/another-check.py".to_string();
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        let error = attach_plan_row(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            "plan:step-6",
            &evidence_path,
        )
        .expect_err("a Plan-row attachment must name the exact approved command");
        assert!(
            error
                .to_string()
                .contains("does not match Plan row command")
        );
        let unchanged = store.read_work_item("work-1").unwrap();
        assert!(unchanged.attempts[0].evidence_recoveries.is_empty());
        assert_eq!(
            unchanged.attempts[0].status,
            work_model::AttemptStatus::NeedsUser
        );
        assert_eq!(
            pending_plan_host_evidence_action(project.path(), &unchanged.attempts[0])
                .unwrap()
                .command,
            command
        );
    }

    #[test]
    fn focused_request_requires_missing_values_and_rejects_disagreement() {
        assert!(resolve_requested_value("command", None, None).is_err());
        assert!(
            resolve_requested_value(
                "command",
                Some("swift test --filter one"),
                Some("swift test --filter two")
            )
            .is_err()
        );
        assert_eq!(
            resolve_requested_value(
                "command",
                Some("swift test --filter one"),
                Some("swift test --filter one")
            )
            .unwrap(),
            "swift test --filter one"
        );
    }

    #[test]
    fn focused_command_allows_quoted_harness_expressions() {
        validate_focused_command(
            "cargo nextest run -E 'test(failed_split_write_retries_from_durable_transaction)'",
        )
        .unwrap();
        validate_focused_command("printf '(literal)'").unwrap();
    }

    #[test]
    fn focused_command_rejects_active_shell_control() {
        for command in [
            "printf one && printf two",
            "printf $(whoami)",
            "printf \"$(whoami)\"",
            "(printf one)",
            "printf one > output",
            "printf 'unterminated",
        ] {
            assert!(
                validate_focused_command(command).is_err(),
                "must reject {command:?}"
            );
        }
    }

    #[test]
    fn focused_evidence_includes_linked_git_metadata_in_read_and_deny_boundaries() {
        let root = tempfile::TempDir::new().unwrap();
        let main = root.path().join("main");
        let candidate = root.path().join("candidate");
        fs::create_dir_all(&main).unwrap();
        crate::git::run(&main, &["init", "-q", "-b", "main"], "init test repo").unwrap();
        crate::git::run(
            &main,
            &["config", "user.name", "Test User"],
            "configure test repo",
        )
        .unwrap();
        crate::git::run(
            &main,
            &["config", "user.email", "test@example.com"],
            "configure test repo",
        )
        .unwrap();
        fs::write(main.join("README.md"), "candidate\n").unwrap();
        crate::git::run(&main, &["add", "README.md"], "stage test candidate").unwrap();
        crate::git::run(
            &main,
            &["commit", "-q", "-m", "Add candidate"],
            "commit test candidate",
        )
        .unwrap();
        crate::git::run(
            &main,
            &["worktree", "add", "-q", candidate.to_str().unwrap()],
            "create linked candidate",
        )
        .unwrap();
        let candidate = candidate.canonicalize().unwrap();
        let common = main.join(".git").canonicalize().unwrap();

        let (readable, denied_writes) = focused_evidence_candidate_boundaries(&candidate).unwrap();

        assert_eq!(readable, [candidate.clone(), common.clone()]);
        assert_eq!(denied_writes, [candidate, common]);
    }

    #[test]
    fn evidence_snapshot_is_exact_and_idempotent() {
        let root = tempfile::TempDir::new().unwrap();
        let bytes = b"exact host output\n";
        publish_snapshot(
            root.path(),
            ".fluent/work/artifacts/work/a/host-evidence/digest.json",
            bytes,
        )
        .unwrap();
        publish_snapshot(
            root.path(),
            ".fluent/work/artifacts/work/a/host-evidence/digest.json",
            bytes,
        )
        .unwrap();
        assert_eq!(
            fs::read(
                root.path()
                    .join(".fluent/work/artifacts/work/a/host-evidence/digest.json")
            )
            .unwrap(),
            bytes
        );
        assert!(
            publish_snapshot(
                root.path(),
                ".fluent/work/artifacts/work/a/host-evidence/digest.json",
                b"different"
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_gate_evidence_is_candidate_bound_reviewed_and_idempotent() {
        let command = "python3 tests/readiness.py";
        let (project, _workspace, store, candidate, gate_path, evidence_path) =
            candidate_gate_evidence_fixture(command);

        assert!(
            attach(
                project.path(),
                &store,
                "work-1",
                "attempt-1",
                "stale-candidate",
                &evidence_path,
                std::slice::from_ref(&gate_path),
                None,
            )
            .is_err()
        );

        let recovery = attach(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &evidence_path,
            std::slice::from_ref(&gate_path),
            None,
        )
        .unwrap();

        let original_gate = fs::read(project.path().join(&gate_path)).unwrap();
        let mut changed_gate: serde_json::Value = serde_json::from_slice(&original_gate).unwrap();
        changed_gate["checks"][0]["diagnostics"] = serde_json::json!([
            "no passing Tester result matches python3 tests/a_different_check.py"
        ]);
        fs::write(
            project.path().join(&gate_path),
            serde_json::to_vec_pretty(&changed_gate).unwrap(),
        )
        .unwrap();
        assert!(
            attach(
                project.path(),
                &store,
                "work-1",
                "attempt-1",
                &candidate,
                &evidence_path,
                std::slice::from_ref(&gate_path),
                None,
            )
            .is_err(),
            "an old attachment must not match a different rejected gate at the same path"
        );
        fs::write(project.path().join(&gate_path), &original_gate).unwrap();

        let repeated = attach(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &evidence_path,
            std::slice::from_ref(&gate_path),
            None,
        )
        .unwrap();
        assert_eq!(repeated, recovery);

        let attached = store.read_work_item("work-1").unwrap();
        let attempt = &attached.attempts[0];
        assert_eq!(attempt.status, work_model::AttemptStatus::Reviewing);
        assert_eq!(attempt.evidence_recoveries.len(), 1);
        assert_eq!(attempt.recovery_operations.len(), 1);
        assert_eq!(
            attempt.recovery_operations[0].operation_kind,
            "host-evidence-review-attachment"
        );
        let review_id = recovery.targets[0].review_task_id.as_ref().unwrap();
        let review = attempt
            .tasks
            .iter()
            .find(|task| task.id == *review_id)
            .unwrap();
        assert_eq!(review.status, TaskStatus::Planned);
        assert_eq!(review.role, "tests");
        assert_eq!(
            review
                .evidence_review_context
                .as_ref()
                .unwrap()
                .candidate_commit,
            candidate
        );

        store
            .mutate_work_item("work-1", |item| {
                let attempt = &mut item.attempts[0];
                let review = attempt
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == *review_id)
                    .unwrap();
                work_model::set_task_terminal(review, TaskStatus::Complete);
                let artifact = review.artifact_area.as_ref().unwrap().path.clone();
                fs::create_dir_all(project.path().join(&artifact)).unwrap();
                fs::write(
                    project.path().join(artifact).join("review.md"),
                    "Verdict: pass\n",
                )
                .unwrap();
                attempt.evidence_recoveries[0].state = EvidenceRecoveryState::Complete;
                Ok(())
            })
            .unwrap();

        // A later candidate-gate run may replace the original mutable path.
        // The accepted command remains derived from the immutable binding.
        let passed_gate = crate::candidate_gate::CandidateGateEvidence {
            disposition: crate::candidate_gate::CandidateGateDisposition::Passed,
            checks: vec![],
            ..serde_json::from_slice(&original_gate).unwrap()
        };
        fs::write(
            project.path().join(&gate_path),
            serde_json::to_vec_pretty(&passed_gate).unwrap(),
        )
        .unwrap();
        let completed = store.read_work_item("work-1").unwrap();
        assert_eq!(
            completed_candidate_gate_evidence_commands(project.path(), &completed.attempts[0]),
            vec![command.to_string()]
        );
        assert_eq!(
            attach(
                project.path(),
                &store,
                "work-1",
                "attempt-1",
                &candidate,
                &evidence_path,
                std::slice::from_ref(&gate_path),
                None,
            )
            .unwrap(),
            completed.attempts[0].evidence_recoveries[0]
        );
    }

    #[test]
    fn failed_candidate_gate_host_check_reaches_targeted_tests_review() {
        let command = "python3 tests/readiness.py";
        let (project, _workspace, store, candidate, gate_path, evidence_path) =
            candidate_gate_evidence_fixture(command);
        let mut document = parse_evidence_document(&fs::read(&evidence_path).unwrap()).unwrap();
        document.result = EvidenceResult::Fail;
        document.output = "public readiness workflow failed".to_string();
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        let recovery = attach(
            project.path(),
            &store,
            "work-1",
            "attempt-1",
            &candidate,
            &evidence_path,
            std::slice::from_ref(&gate_path),
            None,
        )
        .expect("a failed authorized-host result still needs targeted Tests review");

        let item = store.read_work_item("work-1").unwrap();
        let attempt = &item.attempts[0];
        assert_eq!(attempt.status, work_model::AttemptStatus::Reviewing);
        assert_eq!(recovery.state, EvidenceRecoveryState::Reviewing);
        let targeted_id = recovery.targets[0].review_task_id.as_ref().unwrap();
        assert!(attempt.tasks.iter().any(|task| {
            task.id == *targeted_id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Planned
        }));
        assert!(
            completed_candidate_gate_evidence_commands(project.path(), attempt).is_empty(),
            "failed evidence must never satisfy the candidate gate"
        );
    }

    #[test]
    fn stale_evidence_frontier_is_rejected_without_mutation() {
        let root = tempfile::TempDir::new().unwrap();
        let mut item = work_model::WorkItem::planned("work-1", "Attach host evidence");
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(work_model::TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: root.path().display().to_string(),
            source_branch: "main".to_string(),
            base_commit: None,
            commit: "old-candidate".to_string(),
            no_change: None,
            learner_canonicalization: None,
        });

        let before = item.clone();
        assert!(
            validate_evidence_frontier(
                root.path(),
                &item,
                "attempt-1",
                "new-candidate",
                &["review.md".to_string()],
            )
            .is_err()
        );
        assert_eq!(item, before);
    }

    #[cfg(unix)]
    #[test]
    fn evidence_snapshot_rejects_symlinked_managed_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(root.path().join(".fluent/work")).unwrap();
        symlink(outside.path(), root.path().join(".fluent/work/artifacts")).unwrap();

        assert!(
            publish_snapshot(
                root.path(),
                ".fluent/work/artifacts/work/a/host-evidence/digest.json",
                b"exact host output\n",
            )
            .is_err()
        );
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
