//! Preview and approve responsibility changes without rewriting saved evidence.

use crate::work_model::{
    self, ArtifactRef, OwnershipApproval, OwnershipPreview, OwnershipProposal, TaskKind,
    TaskStatus, WorkItem, WorkModelError, WorkModelStore, WriterCompletionMatrix,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

pub(crate) const PRODUCER: &str = "ownership-recovery";

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn snapshot(
    project: &Path,
    item: &WorkItem,
    attempt_id: &str,
    proposal: &OwnershipProposal,
    original: Option<&OwnershipPreview>,
) -> Result<OwnershipPreview> {
    crate::recovery_contracts::inspect_ownership_adoption(project, item, attempt_id)?;
    let attempt = item.attempts.last().context("missing Attempt")?;
    let writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .context("missing completed Writer")?;
    let output = writer.output.as_ref().context("missing candidate")?;
    let source_matrix = match original {
        Some(preview) => preview.source_matrix.clone(),
        None => crate::recovery_contracts::writer_completion_matrix_ref(project, attempt, writer)
            .context("missing source completion matrix")?,
    };
    let bytes = fs::read(crate::recovery_contracts::resolve_input_artifact_path(
        project,
        &source_matrix.path,
    )?)?;
    ensure!(
        bytes.len() <= work_model::WRITER_COMPLETION_MATRIX_MAX_BYTES,
        "source matrix exceeds size limit"
    );
    let source: WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
    let original_contract = original
        .map(|preview| &preview.original_contract)
        .or(attempt.writer_completion_contract.as_ref())
        .context("missing original contract")?
        .clone();
    let findings = crate::recovery_contracts::writer_completion_findings(project, writer)?;
    source
        .status_against(&original_contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    let (contract, matrix) = proposal
        .apply(&original_contract, &source)
        .map_err(anyhow::Error::msg)?;
    matrix
        .status_against(&contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    if original.is_some() {
        ensure!(
            attempt.writer_completion_contract.as_ref() == Some(&contract),
            "adopted requirements changed"
        );
    }
    let mut input_digests = BTreeMap::new();
    for input in &item.input_artifacts {
        let path = project.join(&input.snapshot_path);
        ensure!(
            path.starts_with(project.join(".fluent/work")),
            "unmanaged planning snapshot"
        );
        input_digests.insert(input.snapshot_path.clone(), digest(&fs::read(path)?));
    }
    for input in &writer.input_artifacts {
        input_digests.insert(
            input.path.clone(),
            digest(&fs::read(project.join(&input.path))?),
        );
    }
    let mut preview = OwnershipPreview {
        schema_version: 1,
        preview_id: String::new(),
        work_item_id: item.id.clone(),
        attempt_id: attempt_id.into(),
        candidate_commit: output.commit.clone(),
        writer_task_id: writer.id.clone(),
        original_contract,
        proposal: proposal.clone(),
        source_matrix,
        source_matrix_digest: digest(&bytes),
        planning_digest: digest(&serde_json::to_vec(&item.planning_context)?),
        tasks_digest: digest(&serde_json::to_vec(&attempt.tasks)?),
        input_digests,
    };
    preview.preview_id = digest(&serde_json::to_vec(&preview)?);
    Ok(preview)
}

pub fn preview(
    project: &Path,
    store: &WorkModelStore,
    id: &str,
    attempt: &str,
    proposal: &OwnershipProposal,
) -> Result<OwnershipPreview> {
    snapshot(project, &store.read_work_item(id)?, attempt, proposal, None)
}

pub fn approve(
    project: &Path,
    store: &WorkModelStore,
    id: &str,
    attempt_id: &str,
    preview: &OwnershipPreview,
    resolution: &str,
) -> Result<OwnershipApproval> {
    ensure!(
        preview.schema_version == 1
            && preview.work_item_id == id
            && preview.attempt_id == attempt_id,
        "preview belongs to a different Work Item or Attempt"
    );
    let resolution = work_model::normalize_round_cap_resolution(resolution)?;
    let _land = crate::land_lock::acquire(&crate::land_lock::lock_path(project))?;
    let slug = preview
        .preview_id
        .strip_prefix("sha256:")
        .context("invalid preview identity")?;
    ensure!(
        slug.len() == 64 && slug.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid preview identity"
    );
    let directory = work_model::work_artifact_path(id, attempt_id, &format!("ownership/{slug}"));
    let audit_path = format!("{directory}/approval.json");
    let matrix_artifact = ArtifactRef {
        producer_id: PRODUCER.into(),
        path: format!("{directory}/writer-completion.json"),
    };
    let apply = |item: &mut WorkItem| -> Result<OwnershipApproval> {
        item.ensure_mutation_compatible()?;
        let approved = item
            .attempts
            .last()
            .context("missing Attempt")?
            .artifacts
            .iter()
            .any(|artifact| artifact.producer_id == PRODUCER && artifact.path == audit_path);
        let fresh = snapshot(
            project,
            item,
            attempt_id,
            &preview.proposal,
            approved.then_some(preview),
        )?;
        ensure!(
            &fresh == preview,
            "ownership preview is stale; preview the current candidate and evidence again"
        );
        if approved {
            let audit: OwnershipApproval =
                serde_json::from_slice(&fs::read(project.join(&audit_path))?)?;
            ensure!(
                audit.preview == *preview
                    && audit.resolution == resolution
                    && audit.matrix_artifact == matrix_artifact,
                "ownership approval changed"
            );
            let expected: WriterCompletionMatrix =
                serde_json::from_slice(&fs::read(project.join(&preview.source_matrix.path))?)?;
            let (_, expected) = preview
                .proposal
                .apply(&preview.original_contract, &expected)
                .map_err(anyhow::Error::msg)?;
            let actual: WriterCompletionMatrix =
                serde_json::from_slice(&fs::read(project.join(&matrix_artifact.path))?)?;
            ensure!(actual == expected, "adopted completion evidence changed");
            return Ok(audit);
        }
        let bytes = fs::read(project.join(&preview.source_matrix.path))?;
        let source: WriterCompletionMatrix = serde_json::from_slice(&bytes)?;
        let (contract, matrix) = preview
            .proposal
            .apply(&preview.original_contract, &source)
            .map_err(anyhow::Error::msg)?;
        let mut audit = OwnershipApproval {
            preview: preview.clone(),
            resolution: resolution.clone(),
            approved_at: work_model::now_iso8601(),
            matrix_artifact: matrix_artifact.clone(),
        };
        // Reuse a matching audit left before a journaled aggregate publication.
        match fs::read(project.join(&audit_path)) {
            Ok(bytes) => {
                let prior: OwnershipApproval = serde_json::from_slice(&bytes)?;
                ensure!(
                    prior.preview == audit.preview
                        && prior.resolution == audit.resolution
                        && prior.matrix_artifact == audit.matrix_artifact,
                    "ownership approval artifact conflicts"
                );
                audit = prior;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        work_model::publish_recovery_snapshot(
            project,
            &matrix_artifact.path,
            &serde_json::to_vec_pretty(&matrix)?,
        )?;
        work_model::publish_recovery_snapshot(
            project,
            &audit_path,
            &serde_json::to_vec_pretty(&audit)?,
        )?;
        let attempt = item.attempts.last_mut().context("missing Attempt")?;
        attempt.progress_contract = Some(work_model::ProgressContract {
            version: work_model::PROGRESS_CONTRACT_VERSION,
            required: contract
                .requirements
                .iter()
                .filter(|row| {
                    row.kind == work_model::WriterCompletionRequirementKind::Plan
                        && row.responsibility.is_none_or(|owner| {
                            owner == work_model::CompletionResponsibility::Writer
                        })
                })
                .map(|row| work_model::RequiredProgressEntry {
                    id: row.id.strip_prefix("plan:").expect("Plan identity").into(),
                    requirement: row.requirement.clone(),
                })
                .collect(),
        });
        attempt.writer_completion_contract = Some(contract);
        attempt.artifacts.push(matrix_artifact.clone());
        attempt.artifacts.push(ArtifactRef {
            producer_id: PRODUCER.into(),
            path: audit_path.clone(),
        });
        let request = work_model::RecoveryOperationRequest::new(
            "ownership-adoption",
            id,
            attempt_id,
            Some(preview.candidate_commit.clone()),
            Some(preview.writer_task_id.clone()),
            None,
            None,
            Some(digest(resolution.as_bytes())),
            Some(preview.preview_id.clone()),
        );
        attempt.record_recovery_operation(&request, None)?;
        Ok(audit)
    };
    Ok(store.mutate_work_item_from_external(
        id,
        "approve saved ownership split",
        Some(audit_path.clone()),
        |item| {
            apply(item).map_err(|error| WorkModelError::RecoveryOperationIneligible {
                attempt_id: attempt_id.into(),
                reason: format!("{error:#}"),
            })
        },
    )?)
}
