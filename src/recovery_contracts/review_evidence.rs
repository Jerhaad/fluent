//! Derive advisory Review inputs from the existing acceptance contracts.

use super::*;
use crate::verification_assessment::validate_required_check_acceptance_at;
use serde::{Deserialize, Serialize};

pub(crate) const PRODUCER: &str = "accepted-host-evidence";
pub(crate) const SOURCE_PRODUCER: &str = "accepted-host-evidence-source";

/// A saved summary pins a selection, but only fresh source validation credits it.
pub(crate) fn validate(
    project_root: &Path,
    attempt: &work_model::Attempt,
    task: &Task,
) -> Result<Vec<AcceptedProof>> {
    if task.kind != TaskKind::Review
        || task.role != "tests"
        || task.evidence_review_context.is_some()
    {
        return Ok(Vec::new());
    }
    let selected = task
        .input_artifacts
        .iter()
        .filter(|input| input.producer_id == PRODUCER)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        anyhow::ensure!(
            !task
                .input_artifacts
                .iter()
                .any(|input| input.producer_id == SOURCE_PRODUCER),
            "accepted host proof summary is missing"
        );
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        selected.len() == 1,
        "accepted host proof requires one ordinary Tests Review selection"
    );
    let (source, bytes) = bound_recovery_artifact(project_root, &selected[0].path)?;
    let expected_path = format!(
        ".fluent/work/artifacts/{}/{}/ordinary-review-evidence/{}.json",
        attempt.work_item_id,
        attempt.id,
        source.digest.trim_start_matches("sha256:")
    );
    anyhow::ensure!(
        source.path == expected_path,
        "accepted host proof summary bytes or path changed"
    );
    let saved: Vec<AcceptedProof> = serde_json::from_slice(&bytes)?;
    let current = current(project_root, attempt);
    anyhow::ensure!(
        !saved.is_empty()
            && current == saved
            && task.review_context.as_ref().is_some_and(|context| saved
                .iter()
                .all(|proof| proof.candidate_commit == context.candidate_commit)),
        "accepted host proof binding changed before ordinary Review launch"
    );
    for binding in saved.iter().flat_map(|proof| &proof.bindings) {
        anyhow::ensure!(
            task.input_artifacts
                .iter()
                .any(|input| input.path == binding.path),
            "accepted host proof source is missing from declared Review inputs: {}",
            binding.path
        );
    }
    anyhow::ensure!(
        task.input_artifacts
            .iter()
            .filter(|input| input.producer_id == SOURCE_PRODUCER)
            .all(|input| saved
                .iter()
                .flat_map(|proof| &proof.bindings)
                .any(|binding| binding.path == input.path)),
        "ordinary Review declares an unvalidated accepted host proof source"
    );
    Ok(saved)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct AcceptedProof {
    pub recovery_id: String,
    pub candidate_commit: String,
    pub scope: String,
    pub command: String,
    pub working_directory: String,
    pub applicability: work_model::ResolvedCheckApplicability,
    pub bindings: Vec<work_model::BoundRecheckArtifact>,
}

/// Invalid retained proof is historical input, never current acceptance.
pub(crate) fn current(project_root: &Path, attempt: &work_model::Attempt) -> Vec<AcceptedProof> {
    current_at(
        project_root,
        attempt,
        crate::check_applicability::evaluation_time(),
    )
}

pub(crate) fn current_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<AcceptedProof> {
    let Some(writer) = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
    else {
        return Vec::new();
    };
    let Some(output) = writer.output.as_ref() else {
        return Vec::new();
    };
    let candidate = &output.commit;
    let owned_host_rows = attempt
        .writer_completion_contract
        .as_ref()
        .is_some_and(|contract| {
            contract
                .requirements
                .iter()
                .any(|row| row.responsibility == Some(work_model::CompletionResponsibility::Host))
        });
    let plan_source = if owned_host_rows {
        owned_host_completion_matrix(project_root, attempt)
            .ok()
            .flatten()
    } else {
        writer_completion_matrix_ref(project_root, attempt, writer)
            .zip(
                authoritative_writer_completion_matrix(project_root, attempt, writer)
                    .ok()
                    .flatten(),
            )
            .map(|(source, matrix)| (writer.clone(), source, matrix))
    };
    let plans = plan_source
        .map(|(writer, source, matrix)| {
            plan_evidence::accepted_plan_host_evidence(
                project_root,
                attempt,
                &writer,
                &source,
                &matrix,
                now,
            )
        })
        .unwrap_or_default();
    let legacy =
        crate::verification_assessment::accepted_legacy_commands(project_root, attempt, now);
    let mut seen = std::collections::HashSet::new();
    attempt
        .evidence_recoveries
        .iter()
        .rev()
        .filter_map(|recovery| {
            if &recovery.candidate_commit != candidate {
                return None;
            }
            let mut derive = || -> Result<AcceptedProof> {
                let mut bindings = Vec::new();
                let (scope, command, working_directory, applicability) = if let Some(check) =
                    &recovery.required_check
                {
                    anyhow::ensure!(seen.insert(check.id.clone()), "newer result exists");
                    anyhow::ensure!(
                        recovery.state == EvidenceRecoveryState::Complete,
                        "unreviewed proof"
                    );
                    validate_required_check_binding(project_root, attempt, check)?;
                    validate_required_check_acceptance_at(project_root, attempt, check, now)?;
                    if let work_model::RequiredCheckSource::CanonicalTester {
                        results,
                        manifest,
                        logs,
                        reconstruction,
                        ..
                    } = &check.source
                    {
                        bindings.extend([results.clone(), manifest.clone()]);
                        bindings.extend(logs.clone());
                        bindings.extend(reconstruction.clone());
                    }
                    (
                        check.id.clone(),
                        check.command.clone(),
                        check.working_directory.clone(),
                        check.applicability.clone(),
                    )
                } else if let Some(saved) = &recovery.carried_finding {
                    let binding = &saved.binding;
                    anyhow::ensure!(
                        seen.insert(binding.finding.id.clone()),
                        "newer finding result exists"
                    );
                    anyhow::ensure!(
                        recovery.state == EvidenceRecoveryState::Complete,
                        "unreviewed proof"
                    );
                    anyhow::ensure!(
                        carried_finding::carried_finding_review_passes_at(
                            project_root,
                            attempt,
                            recovery,
                            now
                        )?,
                        "finding proof does not pass"
                    );
                    bindings.extend([
                        saved.artifact.clone(),
                        binding.source_matrix.clone(),
                        work_model::BoundRecheckArtifact {
                            path: binding.source_review.artifact.path.clone(),
                            digest: binding.source_review.digest.clone(),
                        },
                    ]);
                    (
                        binding.finding.id.clone(),
                        binding.command.clone(),
                        binding.working_directory.clone(),
                        binding.applicability.clone(),
                    )
                } else if let Some(proof) = legacy.iter().find(|proof| {
                    proof.bindings.iter().any(|binding| {
                        binding.path == recovery.attachment.snapshot_path
                            && binding.digest == recovery.attachment.digest
                    })
                }) {
                    anyhow::ensure!(
                        recovery.state == EvidenceRecoveryState::Complete,
                        "unreviewed legacy proof"
                    );
                    bindings.extend(proof.bindings.clone());
                    let source = recovery.targets.first().context("missing legacy target")?;
                    let scope = if let Some(request) = attempt.tasks.iter().find(|task| {
                        review_artifact_path(task).as_deref()
                            == Some(source.prior_review_artifact.as_str())
                    }) {
                        format!("review-requested:{}", request.id)
                    } else {
                        format!("legacy-check:{}", recovery.id)
                    };
                    let (_, bytes) =
                        bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
                    let document = parse_evidence_document(&bytes)?;
                    (
                        scope,
                        proof.command.clone(),
                        document.working_directory,
                        work_model::ResolvedCheckApplicability::default(),
                    )
                } else {
                    let [target] = recovery.targets.as_slice() else {
                        bail!("not a Plan receipt")
                    };
                    anyhow::ensure!(
                        attempt
                            .artifacts
                            .iter()
                            .any(|artifact| artifact.producer_id == "plan-host-evidence"
                                && artifact.path == target.prior_review_artifact),
                        "not an indexed Plan receipt"
                    );
                    let (_, binding) = plans
                        .iter()
                        .find(|(artifact, _)| artifact.path == target.prior_review_artifact)
                        .context("Plan proof is not currently accepted")?;
                    let binding = binding.clone();
                    if owned_host_rows {
                        anyhow::ensure!(
                            validate_plan_evidence_recovery_at(
                                project_root,
                                attempt,
                                recovery,
                                now,
                                false
                            )?,
                            "owned Plan acceptance changed"
                        );
                    }
                    anyhow::ensure!(
                        recovery.state == EvidenceRecoveryState::Complete
                            && plan_evidence_review_is_accepted(
                                project_root,
                                attempt,
                                recovery,
                                target,
                                &binding
                            ),
                        "Plan acceptance changed"
                    );
                    anyhow::ensure!(
                        seen.insert(binding.plan_row.clone()),
                        "newer Plan result exists"
                    );
                    bindings.push(binding.source_matrix.clone());
                    (
                        binding.plan_row,
                        binding.command,
                        binding.working_directory,
                        binding.applicability,
                    )
                };
                bindings.push(
                    bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?.0,
                );
                for target in &recovery.targets {
                    bindings.push(
                        bound_recovery_artifact(project_root, &target.prior_review_artifact)?.0,
                    );
                    let reviewer = attempt
                        .tasks
                        .iter()
                        .find(|task| Some(&task.id) == target.review_task_id.as_ref())
                        .context("missing targeted Review")?;
                    bindings.push(
                        bound_recovery_artifact(
                            project_root,
                            &review_artifact_path(reviewer)
                                .context("missing targeted Review path")?,
                        )?
                        .0,
                    );
                }
                for binding in &bindings {
                    anyhow::ensure!(
                        bound_recovery_artifact(project_root, &binding.path)?.0 == *binding,
                        "accepted proof source changed"
                    );
                }
                bindings.sort_by(|a, b| a.path.cmp(&b.path));
                bindings.dedup();
                Ok(AcceptedProof {
                    recovery_id: recovery.id.clone(),
                    candidate_commit: candidate.clone(),
                    scope,
                    command,
                    working_directory,
                    applicability,
                    bindings,
                })
            };
            derive().ok()
        })
        .collect()
}
