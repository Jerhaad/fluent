//! Validate finding authority separately from generic command coverage.

use super::*;
use crate::work_model::{CarriedFindingBinding, EvidenceDocument};

/// Inspect attachment before creating locks or artifacts, then repeat under
/// the publisher's landing and Work-model locks before applying the result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn inspect_carried_finding_attachment(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate: &str,
    finding_id: &str,
    evidence: work_model::BoundRecheckArtifact,
    document: &EvidenceDocument,
) -> Result<(CarriedFindingBinding, Option<EvidenceRecovery>)> {
    item.ensure_mutation_compatible()?;
    item.ensure_not_abandoned()?;
    let binding = inspect_carried_finding(
        project_root,
        item,
        attempt_id,
        candidate,
        finding_id,
        evidence,
        document,
        crate::check_applicability::evaluation_time(),
    )?;
    let attempt = item.attempts.last().expect("validated current Attempt");
    anyhow::ensure!(
        !item
            .merge_candidates
            .iter()
            .any(|c| c.attempt_id == attempt_id),
        "finding handoff is unavailable after a Merge Candidate exists"
    );
    if let Some(existing) = attempt.evidence_recoveries.iter().find(|r| {
        r.carried_finding
            .as_ref()
            .is_some_and(|s| s.binding == binding)
    }) {
        anyhow::ensure!(
            existing.state == EvidenceRecoveryState::Reviewing,
            "finding evidence is no longer eligible for attachment replay"
        );
        validate_carried_finding_recovery(project_root, item, attempt, existing)?;
        return Ok((binding, Some(existing.clone())));
    }
    if let Some(prior) = attempt
        .evidence_recoveries
        .last()
        .filter(|r| r.carried_finding.is_some())
    {
        let saved = &prior.carried_finding.as_ref().unwrap().binding;
        let mut expected = saved.clone();
        expected.evidence = binding.evidence.clone();
        anyhow::ensure!(
            expected == binding,
            "replacement must preserve the original finding and candidate binding"
        );
        if prior.state == EvidenceRecoveryState::Reviewing {
            anyhow::ensure!(
                inspect_expired_pending_evidence(project_root, attempt)?.is_some(),
                "changed proof cannot replace an active finding recovery"
            );
        } else {
            validate_failed_finding_proof(project_root, item, attempt, prior)?;
        }
        validate_pending_evidence_owners(project_root, attempt, prior)?;
    } else {
        validate_first_finding_handoff(project_root, item, attempt)?;
    }
    Ok((binding, None))
}

fn validate_first_finding_handoff(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
) -> Result<()> {
    anyhow::ensure!(
        attempt.status == work_model::AttemptStatus::NeedsUser
            && attempt.pause_kind == Some(work_model::PauseKind::WriterCompletion)
            && !attempt.cancellation_requested(),
        "first finding attachment requires needs-user/writer-completion"
    );
    anyhow::ensure!(
        attempt.active_evidence_recovery().is_none(),
        "another evidence recovery is active"
    );
    anyhow::ensure!(
        attempt
            .tasks
            .iter()
            .all(|t| matches!(t.status, TaskStatus::Complete | TaskStatus::Superseded)),
        "finding handoff requires settled Tasks"
    );
    for task in &attempt.tasks {
        anyhow::ensure!(
            !crate::lease::is_leased(&crate::lease::task_lock_path(
                project_root,
                &item.id,
                &task.id
            )),
            "finding handoff has a live Task owner"
        );
        if let Some(area) = &task.artifact_area {
            anyhow::ensure!(
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path
                ))?,
                "finding handoff has a live execution owner"
            );
        }
    }
    anyhow::ensure!(
        !crate::lease::is_leased(&crate::lease::learner_lock_path(
            project_root,
            &item.id,
            &attempt.id
        )),
        "finding handoff has a live Learner owner"
    );
    Ok(())
}

struct FindingSource {
    writer: Task,
    finding: WriterCompletionFinding,
    source_review: WriterCorrectionAssignmentInput,
    source_matrix: work_model::BoundRecheckArtifact,
    request: review::FocusedEvidenceRequest,
    applicability: work_model::ResolvedCheckApplicability,
    contract_digest: String,
}

#[allow(clippy::too_many_arguments)]
fn inspect_carried_finding(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate: &str,
    finding_id: &str,
    evidence: work_model::BoundRecheckArtifact,
    document: &EvidenceDocument,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<CarriedFindingBinding> {
    let source =
        inspect_carried_finding_source(project_root, item, attempt_id, candidate, finding_id)?;
    anyhow::ensure!(
        source.request.command == document.check
            && source.request.working_directory == document.working_directory,
        "proof must match the exact finding command and working directory"
    );
    validate_check_applicability_in_workspace_at(
        &candidate_workspace_path(project_root, source.writer.output.as_ref().unwrap()),
        &source.applicability,
        document,
        now,
    )?;
    let current_assignment = source.writer.writer_correction_assignment.as_ref().unwrap();
    // Context recovery may freeze proven bytes after attachment. Retain the
    // original binding and validate its exact audited path to the current one.
    let prior = item
        .attempts
        .last()
        .unwrap()
        .evidence_recoveries
        .iter()
        .rev()
        .filter_map(|r| r.carried_finding.as_ref().map(|s| &s.binding))
        .find(|b| {
            b.writer_task_id == source.writer.id
                && b.candidate_commit == candidate
                && b.finding.id == finding_id
        });
    let assignment = if let Some(binding) = prior {
        anyhow::ensure!(
            assignment_digest(&binding.assignment)? == binding.assignment_digest,
            "carried finding assignment digest changed"
        );
        validate_assignment_context_lineage(
            project_root,
            item,
            &source.writer,
            &binding.assignment,
        )?;
        &binding.assignment
    } else {
        current_assignment
    };
    Ok(CarriedFindingBinding {
        schema_version: 1,
        work_item_id: item.id.clone(),
        attempt_id: attempt_id.into(),
        writer_task_id: source.writer.id.clone(),
        candidate_commit: candidate.into(),
        finding: source.finding,
        assignment_digest: format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(assignment)?)
        ),
        assignment: assignment.clone(),
        contract_digest: source.contract_digest,
        source_review: source.source_review,
        source_matrix: source.source_matrix,
        command: source.request.command,
        working_directory: source.request.working_directory,
        applicability: source.applicability,
        evidence,
    })
}

/// Resolve the original assignment and the current completed Writer without
/// interpreting an older Review as a review of the current candidate.
fn inspect_carried_finding_source(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate: &str,
    finding_id: &str,
) -> Result<FindingSource> {
    let attempt = item
        .attempts
        .last()
        .filter(|a| a.id == attempt_id)
        .context("finding handoff requires the current Attempt")?;
    anyhow::ensure!(
        attempt.kind == work_model::AttemptKind::Write,
        "finding handoff requires a code-producing Attempt"
    );
    let writer_index = attempt
        .tasks
        .iter()
        .rposition(|t| t.kind == TaskKind::Write)
        .context("finding handoff has no Writer")?;
    let writer = &attempt.tasks[writer_index];
    anyhow::ensure!(
        writer.status == TaskStatus::Complete,
        "latest Writer is not complete"
    );
    let output = writer
        .output
        .as_ref()
        .context("completed Writer has no candidate")?;
    anyhow::ensure!(
        output.commit == candidate
            && candidate_workspace_is_clean_at_managed(project_root, output, candidate),
        "finding candidate changed, is dirty, or is not a managed workspace"
    );
    let assignment = writer
        .writer_correction_assignment
        .as_ref()
        .context("finding handoff requires an immutable corrective assignment")?;
    validate_writer_correction_assignment(project_root, item, writer, assignment)?;
    let finding = assignment
        .findings
        .iter()
        .find(|f| f.id == finding_id)
        .context("selected finding is not assigned to the current Writer")?;
    anyhow::ensure!(
        finding.role == "tests",
        "selected finding is not a Tests finding"
    );
    let sources: Vec<_> = assignment
        .evidence
        .iter()
        .filter(|input| input.artifact.path == finding.source)
        .collect();
    let [source_review] = sources.as_slice() else {
        bail!("selected finding must bind exactly one assigned source Review");
    };
    let source_index = attempt
        .tasks
        .iter()
        .position(|t| t.id == assignment.source_task_id)
        .context("assignment source Writer is missing")?;
    let review_index = attempt
        .tasks
        .iter()
        .position(|t| t.id == source_review.artifact.producer_id)
        .context("assigned source Review Task is missing")?;
    let source = &attempt.tasks[review_index];
    anyhow::ensure!(
        source_index < review_index
            && review_index < writer_index
            && source.kind == TaskKind::Review
            && source.role == "tests"
            && source.status == TaskStatus::Complete
            && review_artifact_path(source).as_deref() == Some(finding.source.as_str())
            && source
                .review_context
                .as_ref()
                .is_some_and(|c| c.candidate_commit == assignment.source_candidate_commit),
        "assigned source Review does not match its original candidate and Task order"
    );
    let (source_bound, source_bytes) = bound_recovery_artifact(project_root, &finding.source)?;
    anyhow::ensure!(
        source_bound.digest == source_review.digest,
        "assigned source Review digest changed"
    );
    let text = std::str::from_utf8(&source_bytes)?;
    anyhow::ensure!(
        review::open_finding_titles(text)
            .iter()
            .any(|title| title == &finding.requirement
                && format!("finding:tests:{}", review::finding_identity("tests", title))
                    == finding.id),
        "selected finding no longer matches the source Review"
    );
    let request = review::extract_focused_evidence_request(text)
        .context("source Review must request exactly one structured host check")?;
    anyhow::ensure!(
        request.command.len() <= 4096
            && request.working_directory.len() <= 1024
            && !request.command.contains(['\n', '\r', '\0']),
        "finding check exceeds its prompt or command bounds"
    );
    let workspace = candidate_workspace_path(project_root, output);
    let relative = Path::new(&request.working_directory);
    anyhow::ensure!(
        request.working_directory == "."
            || (!relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                && relative.components().collect::<PathBuf>().as_os_str() == relative.as_os_str()),
        "finding working directory must use a canonical candidate-relative path"
    );
    anyhow::ensure!(
        workspace
            .join(relative)
            .canonicalize()?
            .starts_with(workspace.canonicalize()?)
            && workspace.join(relative).is_dir(),
        "finding working directory escapes the candidate"
    );
    let applicability = crate::check_applicability::resolve_check_applicability(
        &workspace,
        &request.applicability,
    )?;
    let contract = attempt
        .writer_completion_contract
        .as_ref()
        .context("finding handoff requires an authoritative completion contract")?;
    let matrix = authoritative_writer_completion_matrix(project_root, attempt, writer)?
        .context("finding handoff requires the current Writer completion matrix")?;
    matrix
        .status_against(contract, &writer.id, &assignment.findings)
        .map_err(anyhow::Error::msg)?;
    let pending = matrix.pending_writer_rows();
    anyhow::ensure!(
        pending.len() == 1
            && pending[0].id == finding_id
            && pending[0].kind == work_model::WriterCompletionRequirementKind::Finding,
        "all other Writer obligations must be complete and the selected finding must be pending"
    );
    let matrix_ref = writer_completion_matrix_ref(project_root, attempt, writer)
        .context("source completion matrix is missing")?;
    let (source_matrix, _) = bound_recovery_artifact(project_root, &matrix_ref.path)?;
    Ok(FindingSource {
        writer: writer.clone(),
        finding: finding.clone(),
        source_review: (*source_review).clone(),
        source_matrix,
        request,
        applicability,
        contract_digest: format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(contract)?)),
    })
}

/// Revalidate saved authority independently from first-attachment pause state.
pub(crate) fn validate_carried_finding_recovery(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Result<()> {
    validate_carried_finding_recovery_at(
        project_root,
        item,
        attempt,
        recovery,
        crate::check_applicability::evaluation_time(),
    )
}

fn validate_carried_finding_recovery_at(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let saved = recovery
        .carried_finding
        .as_ref()
        .context("carried finding binding is missing")?;
    let binding = &saved.binding;
    let (artifact, bytes) = bound_recovery_artifact(project_root, &saved.artifact.path)?;
    anyhow::ensure!(
        artifact == saved.artifact
            && serde_json::from_slice::<CarriedFindingBinding>(&bytes)? == *binding,
        "carried finding binding digest or contents changed"
    );
    let (evidence, bytes) =
        bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    anyhow::ensure!(
        evidence.digest == recovery.attachment.digest && evidence == binding.evidence,
        "carried finding evidence digest changed"
    );
    let current = inspect_carried_finding(
        project_root,
        item,
        &attempt.id,
        &recovery.candidate_commit,
        &binding.finding.id,
        evidence,
        &parse_evidence_document(&bytes)?,
        now,
    )?;
    anyhow::ensure!(
        current == *binding && recovery.required_check.is_none(),
        "carried finding source or candidate binding changed"
    );
    let [target] = recovery.targets.as_slice() else {
        bail!("finding recovery must target one Tests Review");
    };
    let reviewer = attempt
        .tasks
        .iter()
        .find(|t| Some(t.id.as_str()) == target.review_task_id.as_deref())
        .context("finding recovery's targeted Review is missing")?;
    anyhow::ensure!(
        target.role == "tests"
            && target.prior_review_artifact == saved.artifact.path
            && reviewer.kind == TaskKind::Review
            && reviewer.role == "tests"
            && reviewer
                .review_context
                .as_ref()
                .is_some_and(|c| c.candidate_commit == binding.candidate_commit)
            && reviewer
                .evidence_review_context
                .as_ref()
                .is_some_and(|c| c.recovery_id == recovery.id
                    && c.candidate_commit == binding.candidate_commit
                    && c.attachment == recovery.attachment
                    && c.prior_review_artifact == saved.artifact.path),
        "finding recovery's targeted Review binding changed"
    );
    for path in [
        &saved.artifact.path,
        &binding.evidence.path,
        &binding.source_review.artifact.path,
        &binding.source_matrix.path,
    ] {
        anyhow::ensure!(
            reviewer.input_artifacts.iter().any(|a| &a.path == path),
            "finding Review is missing a declared input: {path}"
        );
    }
    let writer_index = attempt
        .tasks
        .iter()
        .position(|t| t.id == binding.writer_task_id)
        .context("bound finding Writer is missing")?;
    for (index, task) in attempt.tasks.iter().enumerate() {
        if Some(task.id.as_str()) == target.review_task_id.as_deref() {
            continue;
        }
        if index > writer_index && recovery.state != EvidenceRecoveryState::Reviewing {
            continue;
        }
        anyhow::ensure!(
            matches!(task.status, TaskStatus::Complete | TaskStatus::Superseded)
                && !crate::lease::is_leased(&crate::lease::task_lock_path(
                    project_root,
                    &attempt.work_item_id,
                    &task.id
                )),
            "finding recovery has another unresolved Task or live owner"
        );
        if let Some(area) = &task.artifact_area {
            anyhow::ensure!(
                !crate::execution::execution_is_live(&crate::execution::record_path_for_artifact(
                    project_root,
                    &area.path
                ))?,
                "finding recovery has another live execution owner"
            );
        }
    }
    Ok(())
}

/// Require both passing proof and a successful, candidate-bound Tests Review.
pub(crate) fn carried_finding_review_passes(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Result<bool> {
    carried_finding_review_passes_at(
        project_root,
        attempt,
        recovery,
        crate::check_applicability::evaluation_time(),
    )
}

pub(super) fn carried_finding_review_passes_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let item = work_model::WorkModelStore::new(project_root)
        .read_work_item_snapshot(&attempt.work_item_id)?;
    validate_carried_finding_recovery_at(project_root, &item, attempt, recovery, now)?;
    let (_, bytes) = bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    let proof_passes = parse_evidence_document(&bytes)?.result == work_model::EvidenceResult::Pass;
    let target = &recovery.targets[0];
    let task = attempt
        .tasks
        .iter()
        .find(|t| Some(t.id.as_str()) == target.review_task_id.as_deref())
        .expect("validated finding reviewer");
    if task.status != TaskStatus::Complete {
        return Ok(false);
    }
    let (review, bytes) =
        bound_recovery_artifact(project_root, &review_artifact_path(task).unwrap())?;
    anyhow::ensure!(
        target
            .review_digest
            .as_ref()
            .is_none_or(|d| d == &review.digest),
        "accepted finding Review digest changed"
    );
    if recovery.state == EvidenceRecoveryState::Complete {
        anyhow::ensure!(
            target.review_digest.is_some(),
            "accepted finding Review digest is missing"
        );
    }
    Ok(proof_passes
        && review::extract_verdict(std::str::from_utf8(&bytes)?) == review::Verdict::Pass)
}

/// Preserve a failed targeted Review as the authority for requesting fresh proof.
fn validate_failed_finding_proof(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Result<()> {
    anyhow::ensure!(
        recovery.state == EvidenceRecoveryState::NeedsEvidence,
        "finding recovery does not permit fresh proof"
    );
    let (_, bytes) = bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    let proof = parse_evidence_document(&bytes)?;
    let issued = DateTime::parse_from_rfc3339(&proof.run_at)?.with_timezone(&chrono::Utc);
    validate_carried_finding_recovery_at(project_root, item, attempt, recovery, issued)?;
    let target = &recovery.targets[0];
    let task = attempt
        .tasks
        .iter()
        .find(|t| Some(t.id.as_str()) == target.review_task_id.as_deref())
        .context("failed finding Review is missing")?;
    anyhow::ensure!(
        task.status == TaskStatus::Complete,
        "failed finding Review is not complete"
    );
    let (bound, bytes) =
        bound_recovery_artifact(project_root, &review_artifact_path(task).unwrap())?;
    let text = std::str::from_utf8(&bytes)?;
    anyhow::ensure!(
        target.review_digest.as_deref() == Some(bound.digest.as_str())
            && review::extract_verdict(text) == review::Verdict::Fail
            && review::extract_evidence_disposition(text)
                == Some(review::EvidenceDisposition::EvidenceNeeded),
        "fresh finding proof requires the unchanged evidence-needed Review"
    );
    Ok(())
}

/// Account for host acceptance in a current view, preserving every Writer file.
pub(crate) fn apply_accepted_carried_findings(
    project_root: &Path,
    attempt: &work_model::Attempt,
    writer: &Task,
    matrix: &mut work_model::WriterCompletionMatrix,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for recovery in attempt.evidence_recoveries.iter().rev() {
        let Some(saved) = &recovery.carried_finding else {
            continue;
        };
        let binding = &saved.binding;
        if binding.writer_task_id != writer.id
            || writer
                .output
                .as_ref()
                .is_none_or(|o| o.commit != binding.candidate_commit)
            || !seen.insert(&binding.finding.id)
        {
            continue;
        }
        if recovery.state != EvidenceRecoveryState::Complete {
            continue;
        }
        anyhow::ensure!(
            carried_finding_review_passes(project_root, attempt, recovery)?,
            "accepted carried finding no longer has passing proof and Review"
        );
        let row = matrix
            .rows
            .iter_mut()
            .find(|r| r.id == binding.finding.id)
            .context("accepted finding is absent from the completion matrix")?;
        row.state = work_model::WriterCompletionState::Complete;
        row.implementation_evidence = vec![format!(
            "Host evidence accepted by Tests Review: {}",
            saved.artifact.path
        )];
        row.verification = vec![work_model::WriterVerificationClaim {
            command: binding.command.clone(),
            result: work_model::WriterVerificationResult::Pass,
        }];
        row.applicable_constraints =
            vec!["none: Accepted host proof satisfies this assigned finding.".into()];
    }
    Ok(())
}

/// Describe only a validated handoff or saved proof, without publishing state.
pub(crate) fn carried_finding_recovery_status(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<Option<RequiredCheckRecoveryStatus>> {
    let item = work_model::WorkModelStore::new(project_root)
        .read_work_item_snapshot(&attempt.work_item_id)?;
    if let Some(recovery) = attempt
        .active_evidence_recovery()
        .filter(|r| r.carried_finding.is_some())
    {
        if inspect_expired_pending_evidence(project_root, attempt)?.is_some() {
            return Ok(None);
        }
        validate_carried_finding_recovery(project_root, &item, attempt, recovery)?;
        let binding = &recovery.carried_finding.as_ref().unwrap().binding;
        return Ok(Some(RequiredCheckRecoveryStatus {
            can_resume: true,
            reason: format!(
                "Carried finding {} has proof for candidate {} and `{}` awaiting targeted Tests Review.",
                binding.finding.id, binding.candidate_commit, binding.command
            ),
            next_action: Some(format!(
                "fluent queue resume {} --attempt {}",
                attempt.work_item_id, attempt.id
            )),
        }));
    }
    let Some(writer) = attempt
        .tasks
        .iter()
        .rev()
        .find(|t| t.kind == TaskKind::Write)
    else {
        return Ok(None);
    };
    if writer.status != TaskStatus::Complete {
        return Ok(None);
    }
    let Some(output) = &writer.output else {
        return Ok(None);
    };
    let current = attempt.evidence_recoveries.iter().rev().find(|r| {
        r.carried_finding.as_ref().is_some_and(|s| {
            s.binding.writer_task_id == writer.id && s.binding.candidate_commit == output.commit
        })
    });
    if let Some(recovery) = current {
        if recovery.state == EvidenceRecoveryState::Complete {
            anyhow::ensure!(
                carried_finding_review_passes(project_root, attempt, recovery)?,
                "accepted carried finding no longer passes"
            );
            // Preserve acceptance validation, then let the caller assess the
            // remaining checks and any later recovery for this candidate.
            return Ok(None);
        }
        if recovery.state == EvidenceRecoveryState::NeedsEvidence {
            validate_failed_finding_proof(project_root, &item, attempt, recovery)?;
            validate_pending_evidence_owners(project_root, attempt, recovery)?;
            let binding = &recovery.carried_finding.as_ref().unwrap().binding;
            return Ok(Some(RequiredCheckRecoveryStatus {
                can_resume: false,
                reason: format!(
                    "Finding {} requires fresh proof for `{}` in `{}` after its targeted Review.",
                    binding.finding.id, binding.command, binding.working_directory
                ),
                next_action: Some(format!(
                    "fluent attempt evidence attach {} {} --candidate {} --finding {} --evidence-file <path>",
                    attempt.work_item_id, attempt.id, binding.candidate_commit, binding.finding.id
                )),
            }));
        }
        return Ok(None);
    }
    if attempt.status != work_model::AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(work_model::PauseKind::WriterCompletion)
    {
        return Ok(None);
    }
    let Some(assignment) = &writer.writer_correction_assignment else {
        return Ok(None);
    };
    let Some(matrix) = authoritative_writer_completion_matrix(project_root, attempt, writer)?
    else {
        return Ok(None);
    };
    let pending = matrix.pending_writer_rows();
    let [row] = pending.as_slice() else {
        return Ok(None);
    };
    let Some(finding) = assignment
        .findings
        .iter()
        .find(|f| f.id == row.id && f.role == "tests")
    else {
        return Ok(None);
    };
    let source = inspect_carried_finding_source(
        project_root,
        &item,
        &attempt.id,
        &output.commit,
        &finding.id,
    )?;
    validate_first_finding_handoff(project_root, &item, attempt)?;
    anyhow::ensure!(
        !item
            .merge_candidates
            .iter()
            .any(|c| c.attempt_id == attempt.id)
            && !crate::land_lock::is_locked(&crate::land_lock::lock_path(project_root)),
        "finding handoff has a landing candidate or live landing owner"
    );
    Ok(Some(RequiredCheckRecoveryStatus {
        can_resume: false,
        reason: format!(
            "Carried finding {} requires host proof for candidate {}: `{}` in `{}`. Attach a schema-version-1 evidence document, then require targeted Tests Review.",
            finding.id, output.commit, source.request.command, source.request.working_directory
        ),
        next_action: Some(format!(
            "fluent attempt evidence attach {} {} --candidate {} --finding {} --evidence-file <path>",
            attempt.work_item_id, attempt.id, output.commit, finding.id
        )),
    }))
}
