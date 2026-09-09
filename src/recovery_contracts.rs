//! Shared admission and completion contracts for recovery-bound Writers.
//!
//! This boundary is intentionally independent from the host-evidence producer
//! and task executor. Queue admission, direct execution, and host recovery all
//! consume the same immutable checks without introducing a module cycle.

use crate::check_applicability::resolve_check_applicability;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
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

/// Validate candidate and evidence facts for a continuation recognized by the
/// lifecycle layer. This boundary never invokes its lifecycle callers.
pub(crate) fn validate_interrupted_plan_host_recovery(
    project_root: &Path,
    attempt: &work_model::Attempt,
    row_id: &str,
    canceled: &Task,
) -> Result<()> {
    let (writer, _, matrix) = owned_host_completion_matrix(project_root, attempt)?
        .context("interrupted recovery requires approved host ownership")?;
    let contract = attempt
        .writer_completion_contract
        .as_ref()
        .expect("owned matrix has a contract");
    let findings = writer_completion_findings(project_root, &writer)?;
    anyhow::ensure!(
        matrix
            .status_against(contract, &writer.id, &findings)
            .map_err(anyhow::Error::msg)?
            .is_complete(),
        "Writer obligations remain incomplete"
    );
    anyhow::ensure!(
        matrix.rows.iter().any(|row| row.id == row_id
            && row.responsibility == Some(work_model::CompletionResponsibility::Host)
            && row.state == work_model::WriterCompletionState::Pending),
        "pending obligation is not approved host work"
    );
    let output = writer.output.as_ref().expect("validated source output");
    anyhow::ensure!(
        candidate_workspace_is_clean_at_managed(project_root, output, &output.commit),
        "interrupted host recovery needs the unchanged managed candidate"
    );
    let selected = writer_completion_matrix_ref(project_root, attempt, &writer)
        .context("source Writer has no matrix")?;
    let preserved = validated_ownership_approval(project_root, attempt)?
        .filter(|audit| audit.matrix_artifact == selected)
        .map(|audit| audit.preview.source_matrix)
        .unwrap_or(selected);
    anyhow::ensure!(
        canceled.input_artifacts.contains(&preserved),
        "canceled Writer does not name the preserved source completion evidence"
    );
    for task in &attempt.tasks {
        anyhow::ensure!(
            task.id == canceled.id
                || matches!(task.status, TaskStatus::Complete | TaskStatus::Superseded),
            "unrelated unresolved Task {}",
            task.id
        );
        anyhow::ensure!(
            !crate::lease::is_leased(&crate::lease::task_lock_path(
                project_root,
                &attempt.work_item_id,
                &task.id
            )),
            "Task {} has a live owner",
            task.id
        );
        if let Some(area) = &task.artifact_area {
            anyhow::ensure!(
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path
                ))?,
                "Task {} still owns a live execution",
                task.id
            );
        }
    }
    Ok(())
}

/// Revalidate active Plan evidence before foreground resume, queue admission,
/// scheduler claim, and Reviewer launch. Return whether every targeted Plan
/// review passed and its active receipt can settle without crediting a later Writer.
/// Legacy attachment semantics remain intact.
pub(crate) fn validate_owned_plan_evidence_review(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<bool> {
    validate_owned_plan_evidence_review_at(
        project_root,
        attempt,
        crate::check_applicability::evaluation_time(),
    )
}

fn validate_owned_plan_evidence_review_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let Some(recovery) = attempt
        .active_evidence_recovery()
        .filter(|recovery| recovery.state == EvidenceRecoveryState::Reviewing)
    else {
        return Ok(false);
    };
    if let Some(check) = &recovery.required_check {
        validate_required_check_binding(project_root, attempt, check)?;
        let output = attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref())
            .context("required check candidate is missing")?;
        let clean = if check.source.is_canonical_tester() {
            candidate_workspace_is_clean_at_managed(project_root, output, &check.candidate_commit)
        } else {
            candidate_workspace_is_clean_at(project_root, output, &check.candidate_commit)
        };
        anyhow::ensure!(
            clean,
            "required check candidate is changed, dirty, or no longer managed"
        );
        let (attachment, bytes) =
            bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
        anyhow::ensure!(
            attachment.digest == recovery.attachment.digest,
            "required check evidence digest changed"
        );
        let document = parse_evidence_document(&bytes)?;
        validate_check_applicability_at(&check.applicability, &document, now)?;
        anyhow::ensure!(
            recovery.candidate_commit == check.candidate_commit
                && document.check == check.command
                && document.working_directory == check.working_directory,
            "required check result no longer matches its candidate or command"
        );
        anyhow::ensure!(
            recovery.targets.len() == 1,
            "required check needs one targeted Tests Review"
        );
        let target = &recovery.targets[0];
        let (prior, bytes) = bound_recovery_artifact(project_root, &target.prior_review_artifact)?;
        match &check.source {
            work_model::RequiredCheckSource::CanonicalTester { .. } => anyhow::ensure!(
                serde_json::from_slice::<work_model::RequiredCheck>(&bytes)? == *check
                    && prior.path
                        == snapshot_path(&attempt.work_item_id, &attempt.id, &prior.digest)?,
                "required check source snapshot digest changed"
            ),
            work_model::RequiredCheckSource::ReviewRequested { request, .. } => {
                anyhow::ensure!(prior == *request, "review request digest changed")
            }
        }
        let reviewer = attempt
            .tasks
            .iter()
            .find(|task| Some(task.id.as_str()) == target.review_task_id.as_deref())
            .context("required check reviewer is missing")?;
        let context = reviewer
            .evidence_review_context
            .as_ref()
            .context("required check review context is missing")?;
        anyhow::ensure!(
            target.role == "tests"
                && reviewer.kind == TaskKind::Review
                && reviewer.role == "tests"
                && context.recovery_id == recovery.id
                && context.candidate_commit == check.candidate_commit
                && context.attachment == recovery.attachment
                && context.prior_review_artifact == target.prior_review_artifact,
            "required check reviewer binding changed"
        );
    }
    let mut completed = !recovery.targets.is_empty();
    for target in &recovery.targets {
        if !attempt.artifacts.iter().any(|artifact| {
            artifact.producer_id == "plan-host-evidence"
                && artifact.path == target.prior_review_artifact
        }) {
            completed = false;
            continue;
        }
        let binding: work_model::PlanHostEvidenceBinding = serde_json::from_slice(&fs::read(
            resolve_input_artifact_path(project_root, &target.prior_review_artifact)?,
        )?)?;
        // Keep current candidate admission strict, but validate each receipt
        // against the Writer it actually reviewed, including completed history.
        let Some((current_writer, _, current_matrix)) =
            owned_host_completion_matrix(project_root, attempt)?
        else {
            completed = false;
            continue;
        };
        let writer = attempt
            .tasks
            .iter()
            .find(|task| {
                task.id == binding.writer_task_id
                    && task.kind == TaskKind::Write
                    && task.status == TaskStatus::Complete
            })
            .context("host evidence source Writer disappeared")?;
        let source = writer_completion_matrix_ref(project_root, attempt, writer)
            .context("host evidence source matrix disappeared")?;
        let matrix = plan_host_source_matrix(
            project_root,
            attempt,
            writer,
            &source,
            &binding.source_matrix,
            &binding.plan_row,
        )?;
        let row = matrix
            .rows
            .iter()
            .find(|row| row.id == binding.plan_row)
            .context("host evidence row disappeared")?;
        if row.responsibility.is_none() {
            completed = false;
            continue;
        }
        anyhow::ensure!(
            row.responsibility == Some(work_model::CompletionResponsibility::Host),
            "evidence row is not host-owned"
        );
        let output = writer
            .output
            .as_ref()
            .context("host evidence source output disappeared")?;
        let command = row.command.as_ref().context("host command disappeared")?;
        let current_row = current_matrix
            .rows
            .iter()
            .find(|current| current.id == row.id)
            .context("current host requirement disappeared")?;
        anyhow::ensure!(
            current_writer
                .output
                .as_ref()
                .is_some_and(|output| output.commit == binding.candidate_commit)
                && current_row.command == row.command
                && current_row.requirement == row.requirement
                && current_row.verification_refs == row.verification_refs,
            "current host requirement or candidate changed"
        );
        anyhow::ensure!(
            binding.applicability.definition == command.applicability,
            "host check applicability changed"
        );
        anyhow::ensure!(
            binding.schema_version == 1
                && binding.work_item_id == attempt.work_item_id
                && binding.attempt_id == attempt.id
                && binding.writer_task_id == writer.id
                && binding.candidate_commit == output.commit
                && recovery.candidate_commit == output.commit
                && binding.requirement == row.requirement
                && binding.command == command.command
                && binding.working_directory == command.working_directory
                && binding.evidence.path == recovery.attachment.snapshot_path
                && binding.evidence.digest == recovery.attachment.digest,
            "host evidence no longer matches its candidate, row, command, or source"
        );
        for artifact in [&binding.source_matrix, &binding.evidence] {
            let bytes = fs::read(resolve_input_artifact_path(project_root, &artifact.path)?)?;
            anyhow::ensure!(
                format!("sha256:{:x}", Sha256::digest(&bytes)) == artifact.digest,
                "host evidence digest changed: {}",
                artifact.path
            );
        }
        let evidence = parse_evidence_document(&fs::read(resolve_input_artifact_path(
            project_root,
            &binding.evidence.path,
        )?)?)?;
        anyhow::ensure!(
            evidence.check == binding.command
                && evidence.result == binding.result
                && evidence.working_directory == binding.working_directory
                && evidence.run_at == binding.run_at,
            "host evidence document changed"
        );
        validate_check_applicability_in_workspace_at(
            &candidate_workspace_path(project_root, current_writer.output.as_ref().unwrap()),
            &binding.applicability,
            &evidence,
            now,
        )?;
        let reviewer = target
            .review_task_id
            .as_ref()
            .and_then(|id| attempt.tasks.iter().find(|task| task.id == *id))
            .context("targeted reviewer disappeared")?;
        let context = reviewer
            .evidence_review_context
            .as_ref()
            .context("targeted review lost its evidence binding")?;
        anyhow::ensure!(
            context.recovery_id == recovery.id
                && context.candidate_commit == output.commit
                && context.attachment == recovery.attachment
                && context.prior_review_artifact == target.prior_review_artifact,
            "targeted review binding changed"
        );
        completed &= binding.result == work_model::EvidenceResult::Pass
            && reviewer.kind == TaskKind::Review
            && reviewer.role == "tests"
            && reviewer.status == TaskStatus::Complete
            && reviewer.artifact_area.as_ref().is_some_and(|area| {
                fs::read_to_string(project_root.join(&area.path).join("review.md"))
                    .is_ok_and(|text| review::extract_verdict(&text) == review::Verdict::Pass)
            });
    }
    Ok(completed)
}

/// Record the exact passing Review bytes when Plan evidence settles.
pub(crate) fn complete_owned_plan_evidence_review(
    project_root: &Path,
    attempt: &mut work_model::Attempt,
) -> Result<bool> {
    if !validate_owned_plan_evidence_review(project_root, attempt)? {
        return Ok(false);
    }
    let recovery = attempt
        .active_evidence_recovery()
        .context("validated Plan recovery disappeared")?;
    let recovery_id = recovery.id.clone();
    let digests = recovery
        .targets
        .iter()
        .map(|target| {
            let task = attempt
                .tasks
                .iter()
                .find(|task| Some(task.id.as_str()) == target.review_task_id.as_deref())
                .context("validated Plan reviewer disappeared")?;
            let path = review_artifact_path(task).context("Plan Review artifact is missing")?;
            let (bound, bytes) = bound_recovery_artifact(project_root, &path)?;
            anyhow::ensure!(
                review::extract_verdict(std::str::from_utf8(&bytes)?) == review::Verdict::Pass,
                "Plan Review no longer passes"
            );
            Ok(bound.digest)
        })
        .collect::<Result<Vec<_>>>()?;
    let recovery = attempt
        .evidence_recoveries
        .iter_mut()
        .find(|entry| entry.id == recovery_id)
        .unwrap();
    for (target, digest) in recovery.targets.iter_mut().zip(digests) {
        target.review_digest = Some(digest);
    }
    recovery.state = EvidenceRecoveryState::Complete;
    Ok(true)
}

/// Validate completed Plan review authority without changing its history.
pub(crate) fn plan_evidence_review_is_accepted(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    target: &work_model::EvidenceReviewTarget,
    binding: &work_model::PlanHostEvidenceBinding,
) -> bool {
    let validate = || -> Result<()> {
        let task = attempt
            .tasks
            .iter()
            .find(|task| Some(task.id.as_str()) == target.review_task_id.as_deref())
            .context("Plan reviewer is missing")?;
        let context = task
            .evidence_review_context
            .as_ref()
            .context("Plan review context is missing")?;
        anyhow::ensure!(
            task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && context.recovery_id == recovery.id
                && context.candidate_commit == binding.candidate_commit
                && context.attachment == recovery.attachment
                && context.prior_review_artifact == target.prior_review_artifact,
            "Plan review binding changed"
        );
        let (bound, bytes) = bound_recovery_artifact(
            project_root,
            &review_artifact_path(task).context("Plan Review artifact is missing")?,
        )?;
        anyhow::ensure!(
            match &target.review_digest {
                Some(digest) => *digest == bound.digest,
                // Older default-code Plan receipts did not record a review digest.
                // Policy-bearing receipts require complete proof from their reader.
                None => binding.applicability.is_default(),
            },
            "accepted Plan Review proof is missing or changed"
        );
        anyhow::ensure!(
            review::extract_verdict(std::str::from_utf8(&bytes)?) == review::Verdict::Pass,
            "accepted Plan Review no longer passes"
        );
        Ok(())
    };
    validate().is_ok()
}

/// Verify the candidate and stopped owners before an operator adopts a saved
/// responsibility split. Approval itself never retries a Writer.
pub(crate) fn inspect_ownership_adoption(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
) -> Result<()> {
    let attempt = item
        .attempts
        .last()
        .filter(|attempt| attempt.id == attempt_id)
        .context("ownership adoption requires the current Attempt")?;
    anyhow::ensure!(
        attempt.status == work_model::AttemptStatus::NeedsUser
            && matches!(
                attempt.pause_kind,
                Some(work_model::PauseKind::WriterCompletion | work_model::PauseKind::Interrupted)
            ),
        "ownership adoption requires a Writer-completion or interrupted pause"
    );
    anyhow::ensure!(
        !item
            .merge_candidates
            .iter()
            .any(|candidate| candidate.attempt_id == attempt_id),
        "ownership adoption is ineligible after a Merge Candidate exists"
    );
    for task in &attempt.tasks {
        anyhow::ensure!(
            task.status != TaskStatus::Executing,
            "Task {} is executing",
            task.id
        );
        anyhow::ensure!(
            !crate::lease::is_leased(&crate::lease::task_lock_path(
                project_root,
                &item.id,
                &task.id
            )),
            "Task {} has a live owner",
            task.id
        );
        if let Some(area) = &task.artifact_area {
            anyhow::ensure!(
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path
                ))?,
                "Task {} has a live execution owner",
                task.id
            );
        }
        anyhow::ensure!(
            matches!(task.status, TaskStatus::Complete | TaskStatus::Superseded)
                || (attempt.pause_kind == Some(work_model::PauseKind::Interrupted)
                    && task.kind == TaskKind::Write
                    && task.status == TaskStatus::NeedsUser
                    && task.output.is_none()),
            "Task {} has an incompatible lifecycle state",
            task.id
        );
    }
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("ownership adoption needs a completed source candidate")?;
    anyhow::ensure!(
        candidate_workspace_is_clean_at(project_root, output, &output.commit),
        "ownership adoption requires the unchanged clean candidate {}",
        output.commit
    );
    Ok(())
}

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
    if let Some(audit) = validated_ownership_approval(project_root, attempt)?
        && artifact.producer_id == "ownership-recovery"
    {
        anyhow::ensure!(
            audit.matrix_artifact == artifact
                && audit.preview.candidate_commit == output.commit
                && audit.preview.writer_task_id == writer.id,
            "ownership mapping no longer names this Writer candidate"
        );
    }
    // An approved Approach delta leaves the original host proof bound to the
    // historical matrix. Validate those exact bytes before projecting current rows.
    let mut source_contract = contract.clone();
    for audit_ref in attempt.artifacts.iter().rev().filter(|entry| {
        entry.producer_id == "planning-approach-recovery" && entry.path.ends_with("/revision.json")
    }) {
        let revision: work_model::ApproachRevisionAudit = serde_json::from_slice(&fs::read(
            resolve_input_artifact_path(project_root, &audit_ref.path)?,
        )?)?;
        if let Some(source) = revision
            .source_completion
            .filter(|source| source.artifact == artifact)
        {
            anyhow::ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == source.sha256,
                "approved Approach source completion evidence changed"
            );
            source_contract = source.contract;
            break;
        }
    }
    let findings = writer_completion_findings(project_root, writer)?;
    matrix
        .status_against(&source_contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    if &source_contract != contract {
        matrix = work_model::WriterCompletionMatrix::skeleton(
            contract,
            &writer.id,
            &findings,
            Some(&matrix),
        )
        .map_err(anyhow::Error::msg)?;
    }
    matrix
        .repair_writer_claims_against(contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    Ok(Some((writer.clone(), artifact, matrix)))
}

/// Recheck the immutable approval before any consumer uses its completion.
fn validated_ownership_approval(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<work_model::OwnershipApproval>> {
    let Some(audit_ref) = attempt.artifacts.iter().rev().find(|artifact| {
        artifact.producer_id == "ownership-recovery" && artifact.path.ends_with("/approval.json")
    }) else {
        validate_planning_revisions(project_root, attempt, None, 0)?;
        return Ok(None);
    };
    let audit: work_model::OwnershipApproval = serde_json::from_slice(&fs::read(
        resolve_input_artifact_path(project_root, &audit_ref.path)?,
    )?)?;
    let preview = &audit.preview;
    anyhow::ensure!(
        preview.work_item_id == attempt.work_item_id && preview.attempt_id == attempt.id,
        "ownership approval belongs to another Attempt"
    );
    let source_bytes = fs::read(resolve_input_artifact_path(
        project_root,
        &preview.source_matrix.path,
    )?)?;
    anyhow::ensure!(
        format!("sha256:{:x}", Sha256::digest(&source_bytes)) == preview.source_matrix_digest,
        "approved source completion evidence changed"
    );
    let source: work_model::WriterCompletionMatrix = serde_json::from_slice(&source_bytes)?;
    validate_preserved_completion_repair(project_root, attempt, &preview.source_matrix)?;
    let (approved_contract, approved_matrix) = preview
        .proposal
        .apply(&preview.original_contract, &source)
        .map_err(anyhow::Error::msg)?;
    let adopted: work_model::WriterCompletionMatrix = serde_json::from_slice(&fs::read(
        resolve_input_artifact_path(project_root, &audit.matrix_artifact.path)?,
    )?)?;
    anyhow::ensure!(
        adopted == approved_matrix,
        "approved completion mapping changed"
    );
    let writer = attempt
        .tasks
        .iter()
        .find(|task| task.id == preview.writer_task_id)
        .context("approved source Writer disappeared")?;
    anyhow::ensure!(
        source.task_id == writer.id
            && adopted.task_id == writer.id
            && writer
                .output
                .as_ref()
                .is_some_and(|output| output.commit == preview.candidate_commit),
        "ownership approval no longer names its source Writer candidate"
    );
    let start = attempt
        .artifacts
        .iter()
        .position(|artifact| artifact == audit_ref)
        .expect("registered ownership approval")
        + 1;
    validate_planning_revisions(project_root, attempt, Some(approved_contract), start)?;
    Ok(Some(audit))
}

/// Check all recorded planning deltas before admitting another revision.
pub(crate) fn validate_planning_completion_history(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<()> {
    validated_ownership_approval(project_root, attempt).map(|_| ())
}

/// Revalidate planning approvals for adopted and explicitly owned Attempts.
fn validate_planning_revisions(
    project_root: &Path,
    attempt: &work_model::Attempt,
    mut contract: Option<work_model::WriterCompletionContract>,
    start: usize,
) -> Result<()> {
    for artifact in attempt.artifacts.iter().skip(start).filter(|artifact| {
        matches!(
            artifact.producer_id.as_str(),
            "planning-reference-recovery" | "planning-approach-recovery"
        ) && artifact.path.ends_with("/revision.json")
    }) {
        if artifact.producer_id == "planning-approach-recovery" {
            let bytes = fs::read(resolve_input_artifact_path(project_root, &artifact.path)?)?;
            let revision: work_model::ApproachRevisionAudit = serde_json::from_slice(&bytes)?;
            if let Some(prior) = &revision.prior_contract {
                if let Some(expected) = &contract {
                    anyhow::ensure!(
                        expected == prior,
                        "approved Approach source contract changed"
                    );
                } else {
                    contract = Some(prior.clone());
                }
            }
            let Some(prior) = contract.as_ref() else {
                // Legacy unowned revisions predate captured source contracts.
                continue;
            };
            let digest = format!("{:x}", Sha256::digest(&bytes));
            anyhow::ensure!(
                revision.schema_version == 1
                    && revision.work_item_id == attempt.work_item_id
                    && revision.attempt_id == attempt.id
                    && !revision.resolution.trim().is_empty()
                    && format!("{:x}", Sha256::digest(revision.resolution.as_bytes()))
                        == revision.resolution_sha256
                    && format!("{:x}", Sha256::digest(revision.prior_approach.as_bytes()))
                        == revision.prior_approach_sha256
                    && format!(
                        "{:x}",
                        Sha256::digest(revision.replacement_approach.as_bytes())
                    ) == revision.replacement_approach_sha256
                    && attempt.recovery_operations.iter().any(|operation| {
                        operation.operation_kind == "planning-approach-revision"
                            && operation.work_item_id == attempt.work_item_id
                            && operation.attempt_id == attempt.id
                            && operation.candidate_commit.as_ref()
                                == Some(&revision.candidate_commit)
                            && operation.task_id.as_ref() == Some(&revision.source_review_task_id)
                            && operation.input_digest.as_ref() == Some(&digest)
                    }),
                "Approach revision has no matching recorded approval"
            );
            anyhow::ensure!(
                attempt
                    .tasks
                    .iter()
                    .any(|task| task.id == revision.source_review_task_id
                        && task.kind == TaskKind::Review
                        && task.role == "architecture"
                        && task.review_context.as_ref().is_some_and(|context| context
                            .candidate_commit
                            == revision.candidate_commit)),
                "approved Approach source review candidate changed"
            );
            if let Some(source) = &revision.source_completion {
                anyhow::ensure!(
                    &source.contract == prior,
                    "approved Approach source contract changed"
                );
                let bytes = fs::read(resolve_input_artifact_path(
                    project_root,
                    &source.artifact.path,
                )?)?;
                anyhow::ensure!(
                    format!("{:x}", Sha256::digest(&bytes)) == source.sha256,
                    "approved Approach source completion evidence changed"
                );
                let matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
                let writer = attempt
                    .tasks
                    .iter()
                    .find(|task| {
                        task.id == matrix.task_id
                            && task.kind == TaskKind::Write
                            && task
                                .output
                                .as_ref()
                                .is_some_and(|output| output.commit == revision.candidate_commit)
                    })
                    .context("approved Approach source Writer candidate changed")?;
                let findings = writer_completion_findings(project_root, writer)?;
                matrix
                    .status_against(prior, &writer.id, &findings)
                    .map_err(anyhow::Error::msg)?;
            }
            contract = Some(
                prior
                    .with_revised_approach(&revision.prior_approach, &revision.replacement_approach)
                    .map_err(anyhow::Error::msg)?,
            );
            continue;
        }
        let revision: work_model::TestReferenceRevisionAudit = serde_json::from_slice(&fs::read(
            resolve_input_artifact_path(project_root, &artifact.path)?,
        )?)?;
        if let Some(source) = &revision.source_completion {
            if let Some(expected) = &contract {
                anyhow::ensure!(
                    expected == &source.contract,
                    "approved planning source contract changed"
                );
            } else {
                contract = Some(source.contract.clone());
            }
            let bytes = fs::read(resolve_input_artifact_path(
                project_root,
                &source.artifact.path,
            )?)?;
            anyhow::ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == source.sha256,
                "approved planning source completion evidence changed"
            );
            let matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
            let writer = attempt
                .tasks
                .iter()
                .find(|task| {
                    task.id == matrix.task_id
                        && task.kind == TaskKind::Write
                        && task
                            .output
                            .as_ref()
                            .is_some_and(|output| output.commit == revision.candidate_commit)
                })
                .context("approved planning source Writer candidate changed")?;
            let findings = writer_completion_findings(project_root, writer)?;
            matrix
                .status_against(&source.contract, &writer.id, &findings)
                .map_err(anyhow::Error::msg)?;
        }
        let Some(approved_contract) = contract.as_mut() else {
            // Older unowned recoveries have no captured source contract.
            continue;
        };
        let revision_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&revision)?));
        anyhow::ensure!(
            revision.schema_version == 1
                && revision.work_item_id == attempt.work_item_id
                && revision.attempt_id == attempt.id
                && !revision.resolution.trim().is_empty()
                && format!("{:x}", Sha256::digest(revision.resolution.as_bytes()))
                    == revision.resolution_sha256
                && attempt.recovery_operations.iter().any(|operation| {
                    operation.operation_kind == "planning-test-reference-revision"
                        && operation.work_item_id == attempt.work_item_id
                        && operation.attempt_id == attempt.id
                        && operation.candidate_commit.as_ref() == Some(&revision.candidate_commit)
                        && operation.input_digest.as_ref() == Some(&revision_digest)
                }),
            "Test-reference revision has no matching recorded approval"
        );
        let mut changed = std::collections::HashSet::new();
        anyhow::ensure!(
            !revision.changes.is_empty(),
            "Test-reference revision has no changes"
        );
        for change in &revision.changes {
            let row = approved_contract
                .requirements
                .iter_mut()
                .find(|row| row.id == change.requirement_id)
                .context("approved Test-reference requirement disappeared")?;
            anyhow::ensure!(
                changed.insert(&change.requirement_id)
                    && row.kind == work_model::WriterCompletionRequirementKind::Behavior
                    && row.verification_refs == change.old_references
                    && !change.new_references.is_empty(),
                "approved Test-reference delta changed"
            );
            row.verification_refs = change.new_references.clone();
        }
        anyhow::ensure!(
            attempt.artifacts.contains(&ArtifactRef {
                producer_id: "planning-reference-recovery".into(),
                path: revision.completion_matrix_artifact.clone(),
            }),
            "approved Test-reference matrix is not registered"
        );
        let bytes = fs::read(resolve_input_artifact_path(
            project_root,
            &revision.completion_matrix_artifact,
        )?)?;
        anyhow::ensure!(
            revision.completion_matrix_sha256.as_ref()
                == Some(&format!("{:x}", Sha256::digest(&bytes))),
            "approved Test-reference completion evidence changed"
        );
        let matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
        let writer = attempt
            .tasks
            .iter()
            .find(|task| {
                task.id == matrix.task_id
                    && task.kind == TaskKind::Write
                    && task
                        .output
                        .as_ref()
                        .is_some_and(|output| output.commit == revision.candidate_commit)
            })
            .context("approved Test-reference source Writer candidate changed")?;
        let findings = writer_completion_findings(project_root, writer)?;
        matrix
            .status_against(approved_contract, &writer.id, &findings)
            .map_err(anyhow::Error::msg)?;
    }
    if let Some(contract) = contract {
        anyhow::ensure!(
            attempt.writer_completion_contract.as_ref() == Some(&contract),
            "approved responsibility contract changed"
        );
    }
    Ok(())
}

/// Validate the saved repair chain without rewriting the original matrices.
fn validate_preserved_completion_repair(
    project_root: &Path,
    attempt: &work_model::Attempt,
    artifact: &ArtifactRef,
) -> Result<()> {
    let mut current = artifact.clone();
    let mut seen = std::collections::HashSet::new();
    while current.producer_id == "writer-completion-repair" {
        anyhow::ensure!(
            seen.insert(current.path.clone()),
            "completion repair source cycle"
        );
        let parent = Path::new(&current.path)
            .parent()
            .context("repair has no directory")?;
        let audit_name = if parent.file_name().is_some_and(|name| name == "format") {
            "format-repair.json"
        } else {
            "repair.json"
        };
        let audit_path = parent.join(audit_name).to_string_lossy().into_owned();
        anyhow::ensure!(
            attempt
                .artifacts
                .iter()
                .any(
                    |reference| reference.producer_id == "writer-completion-repair"
                        && reference.path == audit_path
                ),
            "approved completion source has no recorded repair audit"
        );
        let audit: serde_json::Value = serde_json::from_slice(&fs::read(
            resolve_input_artifact_path(project_root, &audit_path)?,
        )?)?;
        let source = audit["source-artifact"]
            .as_str()
            .context("repair has no source artifact")?;
        let source_bytes = fs::read(resolve_input_artifact_path(project_root, source)?)?;
        let repaired_bytes = fs::read(resolve_input_artifact_path(project_root, &current.path)?)?;
        anyhow::ensure!(
            audit["schema-version"] == 1
                && audit["repaired-artifact"] == current.path
                && audit["source-sha256"] == format!("{:x}", Sha256::digest(&source_bytes))
                && audit["repaired-sha256"] == format!("{:x}", Sha256::digest(&repaired_bytes)),
            "approved completion repair provenance changed"
        );
        let Some(prior) = attempt.artifacts.iter().find(|reference| {
            reference.path == source && reference.producer_id == "writer-completion-repair"
        }) else {
            break;
        };
        current = prior.clone();
    }
    Ok(())
}

pub use crate::work_model::{EvidenceDocument, EvidenceResult};

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
    fn recovery_contracts_remain_independent_of_lifecycle_callers() {
        let source = include_str!("recovery_contracts.rs");
        assert!(
            !include_str!("tester.rs").contains("crate::recovery_contracts::"),
            "Tester must resolve applicability below the recovery consumer"
        );
        for consumer in ["tester", "recovery_contracts"] {
            assert!(
                !include_str!("check_applicability.rs").contains(&format!("crate::{consumer}::")),
                "shared applicability must not depend on {consumer}"
            );
        }
        for caller in [
            "lifecycle_recovery",
            "work_task_executor",
            "host_evidence",
            "queue",
        ] {
            assert!(
                !source.contains(&format!("crate::{caller}::")),
                "neutral recovery contracts must not depend on {caller}"
            );
        }
    }

    #[test]
    fn plan_host_source_requires_bound_bytes_and_recorded_lineage() {
        use super::*;
        let tmp = tempfile::tempdir().unwrap();
        let mut item = work_model::WorkItem::planned("work-1", "Retain host evidence");
        item.planning_context = Some(work_model::PlanningContext {
            plan: Some("| # | State reached | Behaviors | Verification | Responsibility | Req? |\n| 1 | Check host. | | `bash tests/host.sh` | host | Yes |\n".into()),
            ..Default::default()
        });
        item.add_initial_attempt("attempt-1").unwrap();
        let attempt = &item.attempts[0];
        let writer = &attempt.tasks[0];
        let matrix = work_model::WriterCompletionMatrix::skeleton(
            attempt.writer_completion_contract.as_ref().unwrap(),
            &writer.id,
            &[],
            None,
        )
        .unwrap();
        let artifact = writer_completion_matrix_ref(tmp.path(), attempt, writer).unwrap();
        let path = tmp.path().join(&artifact.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec_pretty(&matrix).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut bound = work_model::BoundRecheckArtifact {
            path: artifact.path.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
        };
        assert_eq!(
            plan_host_source_matrix(
                tmp.path(),
                attempt,
                writer,
                &artifact,
                &bound,
                "plan:step-1"
            )
            .unwrap(),
            matrix
        );
        fs::write(&path, [bytes.as_slice(), b"\n"].concat()).unwrap();
        assert!(
            plan_host_source_matrix(
                tmp.path(),
                attempt,
                writer,
                &artifact,
                &bound,
                "plan:step-1"
            )
            .is_err()
        );
        fs::write(&path, &bytes).unwrap();
        bound.path = format!(
            "{}/unapproved.json",
            writer.artifact_area.as_ref().unwrap().path
        );
        fs::write(tmp.path().join(&bound.path), &bytes).unwrap();
        assert!(
            plan_host_source_matrix(
                tmp.path(),
                attempt,
                writer,
                &artifact,
                &bound,
                "plan:step-1"
            )
            .is_err(),
            "matching bytes cannot authorize an unrecorded source"
        );
    }

    #[test]
    fn pending_evidence_owners_require_settled_cancellation() {
        use crate::work_model::*;
        let project = tempfile::tempdir().unwrap();
        let recovery = EvidenceRecovery {
            id: "pending".into(),
            candidate_commit: "candidate".into(),
            attachment: EvidenceAttachment {
                snapshot_path: "proof.json".into(),
                digest: "sha256:proof".into(),
            },
            targets: vec![],
            state: EvidenceRecoveryState::Reviewing,
            created_at: now_iso8601(),
            required_check: None,
            code_change_classification: None,
        };
        let mut attempt = Attempt::default();
        attempt.cancellation = Some(AttemptCancellation {
            requested_at: now_iso8601(),
            settled_at: None,
            resumed_at: None,
        });
        assert!(
            super::validate_pending_evidence_owners(project.path(), &attempt, &recovery).is_err()
        );
        attempt.cancellation.as_mut().unwrap().settled_at = Some(now_iso8601());
        super::validate_pending_evidence_owners(project.path(), &attempt, &recovery).unwrap();
        assert!(
            attempt.cancellation_requested(),
            "inspection cannot resume a stop"
        );
    }

    #[test]
    fn required_check_status_ignores_unrelated_attempts() {
        let project = tempfile::tempdir().unwrap();
        let mut item = crate::work_model::WorkItem::planned("work-1", "Initial work");
        item.add_initial_attempt("attempt-1").unwrap();
        assert!(
            super::required_check_recovery_status(project.path(), &item.attempts[0])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_manifest_requires_complete_default_commands() {
        let results: crate::tester::TesterResults = serde_json::from_value(serde_json::json!({
            "commands":[{"command":"false","test_harness":"shell-harness","exit_code":1,"duration_ms":1,"stdout_log":"commands/0-stdout.log","stderr_log":"commands/0-stderr.log"}],
            "tests":[],"summary":{"total":0,"pass":0,"fail":0,"skipped":0},"error":null
        })).unwrap();
        let config = b"commands:\n  - command: 'false'\n    test_harness: shell-harness\n";
        let manifest = super::reconstruct_legacy_manifest(config, b"extractor", &results).unwrap();
        assert!(manifest.full_run);
        assert!(manifest.run_at.is_none());
        assert!(manifest.environment.is_empty());
        let declared = b"commands:\n  - command: 'false'\n    test_harness: shell-harness\n    applicability: {comparison_refs: [main]}\n";
        assert!(
            super::reconstruct_legacy_manifest(declared, b"extractor", &results)
                .unwrap_err()
                .to_string()
                .contains("historical applicability")
        );
        let mut missing = results.clone();
        missing.commands.clear();
        assert!(super::reconstruct_legacy_manifest(config, b"extractor", &missing).is_err());
    }

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

/// Read recoveries once through the same policy for source selection and
/// Writer inheritance. An invalid approval never falls back to unsplit evidence.
fn recovered_completion_matrices(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Vec<(ArtifactRef, work_model::WriterCompletionMatrix)>> {
    let approval = validated_ownership_approval(project_root, attempt)?;
    let mut recovered = Vec::new();
    for artifact in attempt.artifacts.iter().rev().filter(|artifact| {
        matches!(
            artifact.producer_id.as_str(),
            "writer-completion-repair" | "planning-reference-recovery" | "ownership-recovery"
        ) && artifact.path.ends_with("/writer-completion.json")
    }) {
        if artifact.producer_id == "ownership-recovery" {
            let approval = approval
                .as_ref()
                .context("recovered ownership matrix has no recorded approval")?;
            // The latest approved mapping includes every retained obligation.
            // Older mappings remain history, not competing completion authority.
            if artifact != &approval.matrix_artifact {
                continue;
            }
        }
        let path = resolve_input_artifact_path(project_root, &artifact.path)?;
        let metadata = fs::metadata(&path)
            .with_context(|| format!("inspect recovered completion matrix: {}", path.display()))?;
        anyhow::ensure!(
            metadata.len() <= work_model::WRITER_COMPLETION_MATRIX_MAX_BYTES as u64,
            "recovered Writer completion matrix exceeds size limit: {}",
            path.display()
        );
        let matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parse recovered completion matrix: {}", path.display()))?;
        if artifact.producer_id == "writer-completion-repair"
            && !artifact
                .path
                .contains(&format!("/writer-completion-repairs/{}/", matrix.task_id))
        {
            continue;
        }
        recovered.push((artifact.clone(), matrix));
    }
    Ok(recovered)
}

/// Retain a host check across approved Test-reference revisions only when its
/// original bytes and exact Plan row still match the authoritative candidate.
pub(crate) fn plan_host_source_matrix(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
    current: &ArtifactRef,
    bound: &work_model::BoundRecheckArtifact,
    row_id: &str,
) -> Result<work_model::WriterCompletionMatrix> {
    anyhow::ensure!(
        writer_completion_matrix_ref(project_root, attempt, writer).as_ref() == Some(current),
        "host evidence no longer names the authoritative completion source"
    );
    let bytes = fs::read(resolve_input_artifact_path(project_root, &bound.path)?)?;
    anyhow::ensure!(
        bytes.len() <= work_model::WRITER_COMPLETION_MATRIX_MAX_BYTES
            && format!("sha256:{:x}", Sha256::digest(&bytes)) == bound.digest,
        "host evidence source completion bytes changed"
    );
    let source: work_model::WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        source.task_id == writer.id,
        "host evidence source Writer changed"
    );
    if bound.path != current.path {
        let approval = validated_ownership_approval(project_root, attempt)?;
        let mut predecessor = current.clone();
        let mut seen = std::collections::HashSet::new();
        while predecessor.path != bound.path {
            anyhow::ensure!(
                seen.insert(predecessor.path.clone()),
                "planning source cycle"
            );
            let revision = attempt
                .artifacts
                .iter()
                .rev()
                .filter(|artifact| {
                    // Only reference revisions produce replacement matrices.
                    // The shared validator above checks Approach audit history
                    // under its own schema; those audits add no matrix edge.
                    artifact.producer_id == "planning-reference-recovery"
                        && artifact.path.ends_with("/revision.json")
                })
                .map(
                    |artifact| -> Result<work_model::TestReferenceRevisionAudit> {
                        Ok(serde_json::from_slice(&fs::read(
                            resolve_input_artifact_path(project_root, &artifact.path)?,
                        )?)?)
                    },
                )
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .find(|revision| revision.completion_matrix_artifact == predecessor.path)
                .context("host evidence source is outside approved completion history")?;
            if let Some(source) = revision.source_completion {
                predecessor = source.artifact;
            } else {
                // Preserve the original adoption-matrix route for older audits.
                predecessor = approval
                    .as_ref()
                    .context("planning revision has no recorded source")?
                    .matrix_artifact
                    .clone();
            }
        }
        let current_matrix: work_model::WriterCompletionMatrix = serde_json::from_slice(
            &fs::read(resolve_input_artifact_path(project_root, &current.path)?)?,
        )?;
        let source_row = source
            .rows
            .iter()
            .find(|row| {
                row.id == row_id && row.kind == work_model::WriterCompletionRequirementKind::Plan
            })
            .context("host evidence source Plan row disappeared")?;
        anyhow::ensure!(
            current_matrix.task_id == writer.id
                && current_matrix.rows.iter().find(|row| row.id == row_id) == Some(source_row),
            "host evidence Plan obligation changed"
        );
    }
    Ok(source)
}

/// Carry matching immutable claims through already-materialized repair lineage.
pub(crate) fn authoritative_writer_completion_matrix(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
) -> Result<Option<work_model::WriterCompletionMatrix>> {
    let recovered = recovered_completion_matrices(project_root, attempt)?;
    let mut matrix =
        if let Some((_, matrix)) = recovered
            .iter()
            .find(|(_, matrix)| matrix.task_id == writer.id)
        {
            matrix.clone()
        } else {
            let Some(path) = managed_writer_completion_matrix_path(project_root, writer)? else {
                return Ok(None);
            };
            serde_json::from_slice(&fs::read(&path).with_context(|| {
                format!("read prior Writer completion matrix: {}", path.display())
            })?)
            .with_context(|| format!("parse prior Writer completion matrix: {}", path.display()))?
        };
    for (_, repaired) in &recovered {
        matrix.inherit_completed_planning_claims(repaired);
    }
    Ok(Some(matrix))
}

pub(crate) fn writer_completion_matrix_ref(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
) -> Option<ArtifactRef> {
    if let Some((artifact, _)) = recovered_completion_matrices(project_root, attempt)
        .ok()?
        .into_iter()
        .find(|(_, matrix)| matrix.task_id == writer.id)
    {
        return Some(artifact);
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

/// Evaluate canonical checks without publishing state or scheduling work.
pub(crate) struct CanonicalCheckAssessment {
    pub results_path: PathBuf,
    pub checks: Vec<work_model::RequiredCheck>,
    pub outstanding: Vec<usize>,
    pub accepted: Vec<usize>,
    pub blockers: Vec<String>,
    pub diagnoses: Vec<String>,
    /// Demonstrated assertion or harness defects retain source correction.
    pub defects: Vec<String>,
    pub reconstruction: Option<CanonicalReconstruction>,
}

pub(crate) struct CanonicalReconstruction {
    pub manifest: work_model::BoundRecheckArtifact,
    pub manifest_bytes: Vec<u8>,
    pub audit: work_model::BoundRecheckArtifact,
    pub audit_bytes: Vec<u8>,
}

impl CanonicalCheckAssessment {
    pub fn is_satisfied(&self) -> bool {
        self.outstanding.is_empty() && self.blockers.is_empty() && self.defects.is_empty()
    }
}

pub(crate) fn bound_recovery_artifact(
    project_root: &Path,
    path: &str,
) -> Result<(work_model::BoundRecheckArtifact, Vec<u8>)> {
    let resolved = validate_managed_file_path(project_root, path)?;
    let bytes = fs::read(resolved)?;
    Ok((
        work_model::BoundRecheckArtifact {
            path: path.to_string(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
        },
        bytes,
    ))
}

/// Select by candidate and configured command position; Task identities only
/// locate the immutable execution that supplies the complete-run evidence.
pub(crate) fn assess_canonical_checks(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<CanonicalCheckAssessment> {
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("no completed Writer candidate")?;
    let tester = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| {
            task.kind == TaskKind::Tester
                && task.status == TaskStatus::Complete
                && !task.is_failed_command_recheck()
                && task
                    .review_context
                    .as_ref()
                    .is_some_and(|context| context.candidate_commit == output.commit)
        })
        .context("no complete canonical Tester for the current candidate")?;
    let context = tester
        .review_context
        .as_ref()
        .context("Tester comparison context is missing")?;
    let area = &tester
        .artifact_area
        .as_ref()
        .context("Tester artifact area is missing")?
        .path;
    anyhow::ensure!(
        *area == work_model::work_artifact_path(&attempt.work_item_id, &attempt.id, &tester.id),
        "Tester artifact area is not canonical"
    );
    let (results_ref, result_bytes) =
        bound_recovery_artifact(project_root, &format!("{area}/tester-results.json"))?;
    let results: crate::tester::TesterResults = serde_json::from_slice(&result_bytes)?;
    anyhow::ensure!(
        results.candidate_commit.as_deref() == Some(output.commit.as_str()),
        "canonical Tester result has a different or missing candidate binding"
    );
    let workspace = candidate_workspace_path(project_root, output);
    let committed_bytes = |path: &str| -> Result<Vec<u8>> {
        let committed = std::process::Command::new("git")
            .current_dir(&workspace)
            .args(["show", &format!("{}:{path}", output.commit)])
            .output()?;
        anyhow::ensure!(
            committed.status.success(),
            "cannot read committed checker configuration: {path}"
        );
        Ok(committed.stdout)
    };
    let config_bytes = committed_bytes(".fluent/tester.yaml")?;
    let extractor_bytes = committed_bytes(".fluent/extract-tester-results")?;
    let original_manifest_path = format!("{area}/canonical-run.json");
    let mut reconstruction = None;
    let (manifest_ref, manifest_bytes) = match fs::symlink_metadata(
        project_root.join(&original_manifest_path),
    ) {
        Ok(_) => bound_recovery_artifact(project_root, &original_manifest_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Support one older shape: an exact candidate-bound completed full
            // Tester whose complete ordered commands prove default code checks.
            let manifest = reconstruct_legacy_manifest(&config_bytes, &extractor_bytes, &results)?;
            let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
            let directory = work_model::work_artifact_path(
                &attempt.work_item_id,
                &attempt.id,
                &format!("evidence-reconstructions/{}", tester.id),
            );
            let manifest_ref = work_model::BoundRecheckArtifact {
                path: format!("{directory}/canonical-run.json"),
                digest: format!("sha256:{:x}", Sha256::digest(&manifest_bytes)),
            };
            let audit_bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1, "work_item_id": attempt.work_item_id, "attempt_id": attempt.id,
                "candidate_commit": output.commit, "source_task_id": tester.id,
                "source_task_kind": tester.kind, "source_task_role": tester.role,
                "source_task_context": context, "source_results": results_ref,
                "configuration": {"git_object":format!("{}:.fluent/tester.yaml", output.commit), "digest":manifest.config_digest},
                "extractor": {"git_object":format!("{}:.fluent/extract-tester-results", output.commit), "digest":manifest.extractor_digest},
                "derived_manifest": manifest_ref,
                "basis": "Completed canonical Tester and exact ordered command coverage; default candidate-code applicability only"
            }))?;
            anyhow::ensure!(
                audit_bytes.len() <= MAX_EVIDENCE_BYTES,
                "legacy reconstruction audit exceeds its bound"
            );
            let audit = work_model::BoundRecheckArtifact {
                path: format!("{directory}/audit.json"),
                digest: format!("sha256:{:x}", Sha256::digest(&audit_bytes)),
            };
            for (artifact, expected) in [(&manifest_ref, &manifest_bytes), (&audit, &audit_bytes)] {
                match fs::symlink_metadata(project_root.join(&artifact.path)) {
                    Ok(_) => anyhow::ensure!(
                        bound_recovery_artifact(project_root, &artifact.path)?.1 == *expected,
                        "preserved legacy reconstruction bytes changed: {}",
                        artifact.path
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            reconstruction = Some(CanonicalReconstruction {
                manifest: manifest_ref.clone(),
                manifest_bytes: manifest_bytes.clone(),
                audit,
                audit_bytes,
            });
            (manifest_ref, manifest_bytes)
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: crate::tester::CanonicalRunManifest = serde_json::from_slice(&manifest_bytes)?;
    anyhow::ensure!(
        manifest.schema_version == 1 && manifest.full_run,
        "Tester manifest does not prove a complete canonical run"
    );
    for (path, bytes, expected) in [
        (
            ".fluent/tester.yaml",
            &config_bytes,
            &manifest.config_digest,
        ),
        (
            ".fluent/extract-tester-results",
            &extractor_bytes,
            &manifest.extractor_digest,
        ),
    ] {
        anyhow::ensure!(
            &format!("sha256:{:x}", Sha256::digest(bytes)) == expected,
            "canonical check configuration changed: {path}"
        );
        if path == ".fluent/tester.yaml" {
            let config: crate::tester::TesterConfig = serde_yaml::from_slice(bytes)?;
            anyhow::ensure!(
                config.commands == manifest.commands,
                "canonical manifest differs from configured commands"
            );
        }
    }
    let mut blockers = Vec::new();
    if results.commands.len() != manifest.commands.len() {
        blockers.push("canonical Tester is missing configured command results".into());
    }
    let mut defects = Vec::new();
    if let Some(error) = &results.error {
        defects.push(format!(
            "canonical Tester has an unresolved extractor or harness error: {}",
            error.kind
        ));
    }
    if results.summary.total != results.tests.len()
        || results.summary.pass
            != results
                .tests
                .iter()
                .filter(|test| test.status == "pass")
                .count()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || results.summary.skipped
            != results
                .tests
                .iter()
                .filter(|test| matches!(test.status.as_str(), "skipped" | "not_run"))
                .count()
    {
        blockers
            .push("canonical Tester summary does not account for its structured results".into());
    }
    let mut logs = Vec::new();
    for (index, command) in results.commands.iter().enumerate() {
        for (actual, expected) in [
            (&command.stdout_log, format!("commands/{index}-stdout.log")),
            (&command.stderr_log, format!("commands/{index}-stderr.log")),
        ] {
            anyhow::ensure!(
                actual == &expected,
                "canonical command log path does not match command {index}"
            );
            logs.push(bound_recovery_artifact(project_root, &format!("{area}/{expected}"))?.0);
        }
        if command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        {
            blockers.push(format!("command {index} has unresolved process leaks"));
        }
    }
    let reviewed = accepted_review_check_results(project_root, attempt);
    let mut checks = Vec::new();
    let mut outstanding = Vec::new();
    let mut accepted = Vec::new();
    let mut diagnoses = Vec::new();
    for (index, definition) in manifest.commands.iter().enumerate() {
        let applicability = match manifest.applicability.get(index) {
            Some(binding) => binding.clone(),
            None if definition.applicability.is_default() => {
                work_model::ResolvedCheckApplicability::default()
            }
            None => bail!("required command applicability snapshot is missing"),
        };
        anyhow::ensure!(
            applicability.definition == definition.applicability
                && resolve_check_applicability(&workspace, &definition.applicability)?
                    == applicability,
            "declared comparison ref or check applicability changed since execution"
        );
        let check = work_model::RequiredCheck {
            applicability,
            id: format!(
                "tester:{}:{index}",
                manifest
                    .config_digest
                    .strip_prefix("sha256:")
                    .context("invalid Tester configuration digest")?
            ),
            work_item_id: attempt.work_item_id.clone(),
            attempt_id: attempt.id.clone(),
            candidate_commit: output.commit.clone(),
            command: definition.command.clone(),
            working_directory: ".".into(),
            source: work_model::RequiredCheckSource::CanonicalTester {
                reconstruction: reconstruction.as_ref().map(|proof| proof.audit.clone()),
                task_id: tester.id.clone(),
                command_index: index,
                results: results_ref.clone(),
                manifest: manifest_ref.clone(),
                logs: logs.clone(),
                comparison_commit: context.base_commit.clone(),
            },
        };
        let result = results.commands.get(index);
        let matches = result.is_some_and(|command| {
            command.command == definition.command && command.test_harness == definition.test_harness
        });
        if !matches {
            blockers.push(format!(
                "command {index} does not match the canonical manifest"
            ));
        }
        let unmet_prerequisite = result.and_then(|command| match &command.failure_cause {
            Some(crate::tester::CommandFailureCause::UnmetPrerequisite { fact, required, observed })
                if matches && command.exit_code != 0 && command.duration_ms == 0
                    && matches!(fact.as_str(), "os" | "architecture")
                    && definition.applicability.environment.get(fact) == Some(required)
                    && manifest.environment.get(index).and_then(|facts| facts.get(fact)) == Some(observed)
                    && observed != required => Some(format!("{fact}: required {required}, observed {observed}; prerequisite prevented execution")),
            _ => None,
        });
        if definition.reject_process_leaks
            && unmet_prerequisite.is_none()
            && result.is_none_or(|command| command.process_settlement.is_none())
        {
            blockers.push(format!(
                "command {index} has no required process-settlement proof"
            ));
        }
        let local_evidence = EvidenceDocument {
            schema_version: 1,
            producer: tester.id.clone(),
            check: check.command.clone(),
            working_directory: check.working_directory.clone(),
            result: EvidenceResult::Pass,
            output: String::new(),
            run_at: manifest.run_at.clone().unwrap_or_default(),
            environment: manifest.environment.get(index).cloned().unwrap_or_default(),
            comparison_commits: check.applicability.comparison_commits.clone(),
        };
        let local_applies = validate_check_applicability_at(
            &check.applicability,
            &local_evidence,
            crate::check_applicability::evaluation_time(),
        )
        .is_ok();
        if !matches || !local_applies || result.is_none_or(|command| command.exit_code != 0) {
            let reviewed_replacement = manifest
                .commands
                .iter()
                .filter(|other| other.command == definition.command)
                .count()
                == 1
                && reviewed.iter().any(|result| {
                    result.command == check.command
                        && result.applicability == check.applicability
                        && result.comparison_commit == context.base_commit
                });
            if matches
                && (required_check_is_accepted(project_root, attempt, &check)
                    || reviewed_replacement)
            {
                accepted.push(index);
            } else {
                outstanding.push(index);
                if let Some(reason) = unmet_prerequisite {
                    diagnoses.push(format!("Required check {}: {reason}. Repair the declared environment or submit authorized host proof.", check.id));
                } else if results.error.is_none()
                    && !results.tests.iter().any(|test| test.status == "fail")
                    && result.is_some_and(|command| command.exit_code != 0)
                {
                    diagnoses.push(format!("Required check {} needs diagnosis: a nonzero exit does not establish an environment, source, or harness cause. Preserve the logs and establish the missing cause or submit exact authorized host proof.", check.id));
                }
            }
        }
        checks.push(check);
    }
    for test in results.tests.iter().filter(|test| test.status == "fail") {
        // Only the established command identity can be discharged by a reviewed
        // replacement. A harness match alone cannot attribute an assertion.
        let covered = accepted.iter().any(|index| {
            let command = &manifest.commands[*index];
            test.id == format!("command-{index}:{}", command.command)
                && test.test_harness == command.test_harness
        });
        if !covered {
            defects.push(format!(
                "canonical Tester has an unresolved structured failure: {}",
                test.id
            ));
        }
    }
    Ok(CanonicalCheckAssessment {
        defects,
        reconstruction,
        diagnoses,
        results_path: project_root.join(results_ref.path),
        checks,
        outstanding,
        accepted,
        blockers,
    })
}

/// Select one existing evidence source without granting execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceSelector {
    RequiredCheck(String),
    ReviewArtifact(String),
    PlanRow(String),
}

impl EvidenceSelector {
    fn attachment_argument(&self) -> String {
        match self {
            Self::RequiredCheck(id) => format!("--required-check {id}"),
            Self::ReviewArtifact(path) => format!("--review-artifact {path}"),
            Self::PlanRow(id) => format!("--plan-row {id}"),
        }
    }
}

pub(crate) struct ExpiredPendingEvidence {
    pub recovery: EvidenceRecovery,
    pub selector: EvidenceSelector,
    pub applicability: work_model::ResolvedCheckApplicability,
    pub command: String,
    pub working_directory: String,
}

/// Identify intact proof that expired before its targeted Review settled.
/// Validate every other binding at the recorded execution time; expiry alone
/// never permits replacing changed sources, candidates or comparison inputs.
pub(crate) fn inspect_expired_pending_evidence(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<ExpiredPendingEvidence>> {
    let Some(recovery) = attempt.active_evidence_recovery() else {
        return Ok(None);
    };
    if recovery.targets.len() != 1 {
        return Ok(None);
    }
    let target = &recovery.targets[0];
    let (selector, applicability) = if let Some(check) = &recovery.required_check {
        let selector = match &check.source {
            work_model::RequiredCheckSource::CanonicalTester { .. } => {
                EvidenceSelector::RequiredCheck(check.id.clone())
            }
            work_model::RequiredCheckSource::ReviewRequested { request, .. } => {
                EvidenceSelector::ReviewArtifact(request.path.clone())
            }
        };
        (selector, check.applicability.clone())
    } else if attempt.artifacts.iter().any(|artifact| {
        artifact.producer_id == "plan-host-evidence"
            && artifact.path == target.prior_review_artifact
    }) {
        let (_, bytes) = bound_recovery_artifact(project_root, &target.prior_review_artifact)?;
        let binding: work_model::PlanHostEvidenceBinding = serde_json::from_slice(&bytes)?;
        (
            EvidenceSelector::PlanRow(binding.plan_row),
            binding.applicability,
        )
    } else {
        return Ok(None);
    };
    let work_model::CheckFreshness::LiveState {
        max_age_seconds: Some(seconds),
    } = applicability.definition.freshness
    else {
        return Ok(None);
    };
    let (bound, bytes) = bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    anyhow::ensure!(
        bound.digest == recovery.attachment.digest,
        "pending evidence bytes changed"
    );
    let document = parse_evidence_document(&bytes)?;
    let issued = DateTime::parse_from_rfc3339(&document.run_at)?.with_timezone(&chrono::Utc);
    let expiry = chrono::TimeDelta::try_seconds(i64::try_from(seconds)?)
        .and_then(|duration| issued.checked_add_signed(duration))
        .context("unsupported live-state freshness expiry")?;
    if crate::check_applicability::evaluation_time() < expiry {
        return Ok(None);
    }
    validate_owned_plan_evidence_review_at(project_root, attempt, issued)?;
    Ok(Some(ExpiredPendingEvidence {
        recovery: recovery.clone(),
        selector,
        applicability,
        command: document.check,
        working_directory: document.working_directory,
    }))
}

/// Require settled owners and only the reservations this proof already owns.
pub(crate) fn validate_pending_evidence_owners(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Result<()> {
    anyhow::ensure!(
        !attempt.cancellation_requested()
            || attempt
                .cancellation
                .as_ref()
                .is_some_and(|stop| stop.settled_at.is_some()),
        "local cancellation has not settled"
    );
    for task in &attempt.tasks {
        anyhow::ensure!(
            task.status != TaskStatus::Executing
                && !crate::lease::is_leased(&crate::lease::task_lock_path(
                    project_root,
                    &attempt.work_item_id,
                    &task.id
                )),
            "the Attempt has a live Task owner"
        );
        if let Some(area) = &task.artifact_area {
            anyhow::ensure!(
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path
                ))?,
                "the Attempt has a live execution owner"
            );
        }
        anyhow::ensure!(
            task.status != TaskStatus::Planned
                || recovery
                    .targets
                    .iter()
                    .any(|target| target.review_task_id.as_deref() == Some(task.id.as_str())),
            "another reserved Task must settle before replacing expired evidence"
        );
    }
    Ok(())
}

pub(crate) fn required_check_is_accepted(
    project_root: &Path,
    attempt: &work_model::Attempt,
    check: &work_model::RequiredCheck,
) -> bool {
    // A newer conflicting result must receive its own review; the older pass
    // cannot silently defeat the operator's subsequently submitted evidence.
    let Some(recovery) = attempt.evidence_recoveries.iter().rev().find(|recovery| {
        recovery.required_check.as_ref().is_some_and(|bound| {
            bound.id == check.id && bound.candidate_commit == check.candidate_commit
        })
    }) else {
        return false;
    };
    if recovery.required_check.as_ref() != Some(check)
        || recovery.state != EvidenceRecoveryState::Complete
        || recovery.targets.len() != 1
    {
        return false;
    }
    let validate = || -> Result<()> {
        let (attachment, bytes) =
            bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
        anyhow::ensure!(
            attachment.digest == recovery.attachment.digest,
            "host evidence bytes changed"
        );
        let document = parse_evidence_document(&bytes)?;
        validate_check_applicability_at(
            &check.applicability,
            &document,
            crate::check_applicability::evaluation_time(),
        )?;
        anyhow::ensure!(
            document.result == EvidenceResult::Pass
                && document.check == check.command
                && document.working_directory == check.working_directory,
            "host evidence does not pass this exact check"
        );
        let target = &recovery.targets[0];
        let task = attempt
            .tasks
            .iter()
            .find(|task| Some(task.id.as_str()) == target.review_task_id.as_deref())
            .context("missing targeted review")?;
        let context = task
            .evidence_review_context
            .as_ref()
            .context("missing evidence review context")?;
        anyhow::ensure!(
            task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && context.recovery_id == recovery.id
                && context.candidate_commit == check.candidate_commit
                && context.attachment == recovery.attachment
                && context.prior_review_artifact == target.prior_review_artifact,
            "targeted review binding changed"
        );
        let (_, binding) = bound_recovery_artifact(project_root, &target.prior_review_artifact)?;
        match &check.source {
            work_model::RequiredCheckSource::CanonicalTester {
                results,
                manifest,
                logs,
                reconstruction,
                ..
            } => {
                anyhow::ensure!(
                    serde_json::from_slice::<work_model::RequiredCheck>(&binding)? == *check,
                    "required check snapshot changed"
                );
                for source in std::iter::once(results)
                    .chain(std::iter::once(manifest))
                    .chain(logs)
                    .chain(reconstruction)
                {
                    anyhow::ensure!(
                        bound_recovery_artifact(project_root, &source.path)?
                            .0
                            .digest
                            == source.digest,
                        "required source proof is missing or changed: {}",
                        source.path
                    );
                }
            }
            work_model::RequiredCheckSource::ReviewRequested { request, .. } => anyhow::ensure!(
                request.path == target.prior_review_artifact
                    && request.digest == format!("sha256:{:x}", Sha256::digest(&binding)),
                "review request snapshot changed"
            ),
        }
        let path = format!(
            "{}/review.md",
            task.artifact_area
                .as_ref()
                .context("missing review artifacts")?
                .path
        );
        let (artifact, bytes) = bound_recovery_artifact(project_root, &path)?;
        anyhow::ensure!(
            target.review_digest.as_deref() == Some(artifact.digest.as_str())
                && review::extract_verdict(std::str::from_utf8(&bytes)?) == review::Verdict::Pass,
            "targeted review has no accepted passing result"
        );
        Ok(())
    };
    validate().is_ok()
}

#[cfg(test)]
mod applicability_tests {
    use super::*;

    #[test]
    fn evidence_freshness_uses_exact_expiry_and_requires_a_policy() {
        let mut binding = work_model::ResolvedCheckApplicability::default();
        let document: EvidenceDocument = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "producer": "host", "check": "check", "working_directory": ".",
            "result": "pass", "run_at": "2026-09-09T12:00:00Z", "output": "passed"
        }))
        .unwrap();
        let at = |time: &str| {
            DateTime::parse_from_rfc3339(time)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        assert!(
            validate_check_applicability_at(&binding, &document, at("2099-01-01T00:00:00Z"))
                .is_ok()
        );
        binding.definition.freshness = work_model::CheckFreshness::LiveState {
            max_age_seconds: Some(60),
        };
        assert!(
            validate_check_applicability_at(&binding, &document, at("2026-09-09T12:00:59Z"))
                .is_ok()
        );
        assert!(
            validate_check_applicability_at(&binding, &document, at("2026-09-09T12:01:00Z"))
                .unwrap_err()
                .to_string()
                .contains("expired")
        );
        binding.definition.freshness = work_model::CheckFreshness::LiveState {
            max_age_seconds: None,
        };
        assert!(
            validate_check_applicability_at(&binding, &document, at("2026-09-09T12:00:00Z"))
                .unwrap_err()
                .to_string()
                .contains("freshness")
        );
    }
}

pub(crate) fn validate_check_applicability_at(
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    for (name, expected) in &binding.definition.environment {
        anyhow::ensure!(
            document.environment.get(name) == Some(expected),
            "required environment fact {name} is missing or changed"
        );
    }
    anyhow::ensure!(
        binding
            .comparison_commits
            .iter()
            .all(|(name, commit)| document.comparison_commits.get(name) == Some(commit)),
        "required comparison commit is missing or changed"
    );
    if let work_model::CheckFreshness::LiveState { max_age_seconds } = binding.definition.freshness
    {
        let seconds = max_age_seconds
            .filter(|seconds| *seconds > 0 && *seconds <= i64::MAX as u64)
            .context("live-state evidence is missing a supported freshness expiry")?;
        let recorded = DateTime::parse_from_rfc3339(&document.run_at)?.with_timezone(&chrono::Utc);
        let expiry = chrono::TimeDelta::try_seconds(seconds as i64)
            .and_then(|duration| recorded.checked_add_signed(duration))
            .context("unsupported live-state freshness expiry")?;
        anyhow::ensure!(
            recorded <= now && now < expiry,
            "live-state evidence has expired or has an unverified future timestamp"
        );
    }
    Ok(())
}

pub(crate) fn validate_check_applicability(
    workspace: &Path,
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
) -> Result<()> {
    validate_check_applicability_in_workspace_at(
        workspace,
        binding,
        document,
        crate::check_applicability::evaluation_time(),
    )
}

fn validate_check_applicability_in_workspace_at(
    workspace: &Path,
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    anyhow::ensure!(
        resolve_check_applicability(workspace, &binding.definition)? == *binding,
        "declared comparison ref changed since execution"
    );
    validate_check_applicability_at(binding, document, now)
}

pub(crate) fn validate_required_check_binding(
    project_root: &Path,
    attempt: &work_model::Attempt,
    check: &work_model::RequiredCheck,
) -> Result<()> {
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("required check has no current candidate")?;
    anyhow::ensure!(
        check.work_item_id == attempt.work_item_id
            && check.attempt_id == attempt.id
            && check.candidate_commit == output.commit,
        "required check belongs to another candidate or Attempt"
    );
    match &check.source {
        work_model::RequiredCheckSource::CanonicalTester {
            results,
            manifest,
            logs,
            reconstruction,
            ..
        } => {
            for source in std::iter::once(results)
                .chain(std::iter::once(manifest))
                .chain(logs)
                .chain(reconstruction)
            {
                anyhow::ensure!(
                    bound_recovery_artifact(project_root, &source.path)?.0 == *source,
                    "required source digest changed: {}",
                    source.path
                );
            }
            anyhow::ensure!(
                assess_canonical_checks(project_root, attempt)?
                    .checks
                    .contains(check),
                "required canonical check changed"
            );
        }
        work_model::RequiredCheckSource::ReviewRequested {
            task_id,
            request,
            comparison_commit,
        } => {
            let task = attempt
                .tasks
                .iter()
                .find(|task| {
                    task.id == *task_id
                        && task.kind == TaskKind::Review
                        && task.status == TaskStatus::Complete
                })
                .context("required reviewer request has no completed source Task")?;
            let context = task
                .review_context
                .as_ref()
                .context("reviewer request has no comparison context")?;
            anyhow::ensure!(
                context.candidate_commit == check.candidate_commit
                    && &context.base_commit == comparison_commit
                    && task
                        .artifact_area
                        .as_ref()
                        .is_some_and(|area| request.path == format!("{}/review.md", area.path)),
                "review request source binding changed"
            );
            let (actual, bytes) = bound_recovery_artifact(project_root, &request.path)?;
            anyhow::ensure!(actual == *request, "review request source bytes changed");
            let definition = review::extract_focused_evidence_request(std::str::from_utf8(&bytes)?)
                .context("review request is missing its command declaration")?;
            anyhow::ensure!(
                definition.command == check.command
                    && definition.working_directory == check.working_directory
                    && resolve_check_applicability(
                        &candidate_workspace_path(project_root, output),
                        &definition.applicability
                    )? == check.applicability,
                "review request command, environment, freshness, or comparison changed"
            );
        }
    }
    Ok(())
}

/// Reconstruct only inputs proved by the completed producer and committed code.
fn reconstruct_legacy_manifest(
    config_bytes: &[u8],
    extractor_bytes: &[u8],
    results: &crate::tester::TesterResults,
) -> Result<crate::tester::CanonicalRunManifest> {
    anyhow::ensure!(
        config_bytes.len() <= MAX_EVIDENCE_BYTES && extractor_bytes.len() <= 4 * MAX_EVIDENCE_BYTES,
        "legacy checker source exceeds the supported bound"
    );
    let config: crate::tester::TesterConfig = serde_yaml::from_slice(config_bytes)?;
    anyhow::ensure!(
        !config.commands.is_empty()
            && config.commands.len() <= 1024
            && config.commands.len() == results.commands.len(),
        "legacy Tester lacks complete ordered command coverage"
    );
    for (definition, result) in config.commands.iter().zip(&results.commands) {
        anyhow::ensure!(
            definition.applicability.is_default(),
            "legacy Tester lacks historical applicability proof; current environment facts or comparison refs cannot establish an older run's inputs"
        );
        anyhow::ensure!(
            definition.command == result.command && definition.test_harness == result.test_harness,
            "legacy Tester command or harness differs from its committed configuration"
        );
    }
    Ok(crate::tester::CanonicalRunManifest {
        schema_version: 1,
        config_digest: format!("sha256:{:x}", Sha256::digest(config_bytes)),
        extractor_digest: format!("sha256:{:x}", Sha256::digest(extractor_bytes)),
        commands: config.commands,
        full_run: true,
        run_at: None,
        environment: vec![],
        applicability: vec![],
    })
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct RequiredCheckRecoveryStatus {
    pub can_resume: bool,
    pub reason: String,
    pub next_action: Option<String>,
}

/// Derive admission and guidance from current evidence without publishing state.
pub(crate) fn required_check_recovery_status(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<RequiredCheckRecoveryStatus>> {
    if let Some(expired) = inspect_expired_pending_evidence(project_root, attempt)? {
        let settled =
            validate_pending_evidence_owners(project_root, attempt, &expired.recovery).is_ok();
        return Ok(Some(RequiredCheckRecoveryStatus {
            can_resume: false,
            reason: format!(
                "Required check `{}` has expired live-state evidence; submit fresh proof after execution owners settle.",
                expired.command
            ),
            next_action: settled.then(|| {
                format!(
                    "fluent attempt evidence attach {} {} --candidate {} {} --evidence-file <path>",
                    attempt.work_item_id,
                    attempt.id,
                    expired.recovery.candidate_commit,
                    expired.selector.attachment_argument()
                )
            }),
        }));
    }
    let candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    let has_bound_check = attempt.evidence_recoveries.iter().any(|recovery| {
        recovery
            .required_check
            .as_ref()
            .is_some_and(|check| Some(check.candidate_commit.as_str()) == candidate)
    });
    // A changed Writer candidate still needs its ordinary reviews and first
    // complete Tester. Historical results cannot satisfy or prevent that run.
    let has_canonical = attempt.tasks.iter().any(|task| {
        task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && !task.is_failed_command_recheck()
            && task
                .review_context
                .as_ref()
                .is_some_and(|context| Some(context.candidate_commit.as_str()) == candidate)
    });
    if (!has_bound_check
        && !(has_canonical && attempt.pause_kind == Some(work_model::PauseKind::HostEvidence)))
        || (attempt.status == work_model::AttemptStatus::Complete
            && attempt.review_state == Some(work_model::AttemptReviewState::Passed))
        || attempt.tasks.iter().any(|task| {
            task.kind == TaskKind::Write
                && !matches!(task.status, TaskStatus::Complete | TaskStatus::Superseded)
        })
    {
        return Ok(None);
    }
    if let Some(active) = attempt.active_evidence_recovery() {
        if active.required_check.is_none() || active.state != EvidenceRecoveryState::Reviewing {
            return Ok(None);
        }
        validate_owned_plan_evidence_review(project_root, attempt)?;
        return Ok(Some(RequiredCheckRecoveryStatus {
            can_resume: true,
            reason: format!(
                "Required check {} has bound evidence awaiting targeted Tests Review or settlement.",
                active.required_check.as_ref().unwrap().id
            ),
            next_action: Some(format!(
                "fluent queue resume {} --attempt {}",
                attempt.work_item_id, attempt.id
            )),
        }));
    }
    if !has_canonical {
        return Ok(None);
    }
    let assessment = assess_canonical_checks(project_root, attempt)?;
    if assessment.is_satisfied() {
        return Ok(Some(RequiredCheckRecoveryStatus { can_resume: true, reason: "The complete canonical run and reviewed evidence satisfy every required check; resume the remaining lifecycle gates.".into(), next_action: Some(format!("fluent queue resume {} --attempt {}", attempt.work_item_id, attempt.id)) }));
    }
    // A known defect follows the existing Writer budget and correction gates.
    if !assessment.defects.is_empty() {
        return Ok(None);
    }
    let mut reasons = assessment.blockers.clone();
    reasons.extend(assessment.diagnoses.iter().cloned());
    reasons.extend(assessment.outstanding.iter().map(|index| {
        let check = &assessment.checks[*index];
        format!(
            "Required check {} remains unresolved: `{}` (working directory {}).",
            check.id, check.command, check.working_directory
        )
    }));
    let owners_settled = attempt.tasks.iter().all(|task| {
        task.status != TaskStatus::Executing
            && !crate::lease::is_leased(&crate::lease::task_lock_path(
                project_root,
                &attempt.work_item_id,
                &task.id,
            ))
            && task.artifact_area.as_ref().is_none_or(|area| {
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path,
                ))
                .unwrap_or(true)
            })
    });
    let no_reserved_tasks = attempt
        .tasks
        .iter()
        .all(|task| task.status != TaskStatus::Planned);
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("required check candidate is missing")?;
    let clean = candidate_workspace_is_clean_at_managed(project_root, output, &output.commit);
    if !owners_settled || !no_reserved_tasks {
        reasons.push(
            "Execution owners and reserved Tasks must settle before another attachment.".into(),
        );
    }
    if !clean {
        reasons.push("Restore the unchanged clean managed candidate before attachment.".into());
    }
    let next_action = (owners_settled && no_reserved_tasks && clean).then(|| assessment.outstanding.first()).flatten().map(|index| {
        let check = &assessment.checks[*index];
        format!("fluent attempt evidence attach {} {} --candidate {} --required-check {} --evidence-file <path>", attempt.work_item_id, attempt.id, check.candidate_commit, check.id)
    });
    Ok(Some(RequiredCheckRecoveryStatus {
        can_resume: false,
        reason: reasons.join("\n\n"),
        next_action,
    }))
}

struct ReviewedCheckResult {
    command: String,
    applicability: work_model::ResolvedCheckApplicability,
    comparison_commit: Option<String>,
}

pub(crate) fn completed_review_check_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<String> {
    let mut commands = accepted_review_check_results(project_root, attempt)
        .into_iter()
        .map(|result| result.command)
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

fn accepted_review_check_results(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<ReviewedCheckResult> {
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
    let read_verdict = |root: &Path, task: &Task| {
        let path = review_artifact_path(task)?;
        let (_, bytes) = bound_recovery_artifact(root, &path).ok()?;
        Some(review::extract_verdict(std::str::from_utf8(&bytes).ok()?))
    };
    let commands = attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| {
            if recovery.state != EvidenceRecoveryState::Complete
                || recovery.candidate_commit != latest_candidate
            {
                return None;
            }
            if let Some(check) = &recovery.required_check {
                if !matches!(
                    check.source,
                    work_model::RequiredCheckSource::ReviewRequested { .. }
                ) || validate_required_check_binding(project_root, attempt, check).is_err()
                    || !required_check_is_accepted(project_root, attempt, check)
                {
                    return None;
                }
            }
            let (_, evidence_bytes) =
                bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path).ok()?;
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
                let (_, prior_bytes) =
                    bound_recovery_artifact(project_root, &target.prior_review_artifact).ok()?;
                let prior_review = std::str::from_utf8(&prior_bytes).ok()?;
                let request = review::extract_focused_evidence_request(&prior_review)?;
                if recovery.required_check.is_none() && !request.applicability.is_default() {
                    return None;
                }
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
                        && read_verdict(project_root, task) == Some(review::Verdict::Fail)
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
                            && read_verdict(project_root, task) == Some(review::Verdict::Pass)
                    })
                });
                let source_task = attempt.tasks.iter().find(|task| {
                    review_artifact_path(task).as_deref()
                        == Some(target.prior_review_artifact.as_str())
                })?;
                (prior_task_is_bound && targeted_review_passed).then_some(ReviewedCheckResult {
                    command: request.command,
                    applicability: recovery
                        .required_check
                        .as_ref()
                        .map(|check| check.applicability.clone())
                        .unwrap_or_default(),
                    comparison_commit: source_task.review_context.as_ref()?.base_commit.clone(),
                })
            })
        })
        .collect::<Vec<_>>();
    commands
}
