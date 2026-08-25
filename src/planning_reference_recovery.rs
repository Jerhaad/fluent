//! Revise human-approved behavior Test references without rewriting candidate history.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::work_model::{
    ApprovedPlanningRevision, ArtifactRef, AttemptReviewState, AttemptStatus,
    DurableTransitionMetadata, DurableTransitionOrigin, PauseKind, TaskKind, TaskStatus,
    WorkModelError, WorkModelStore, WriterCompletionContract, WriterCompletionMatrix,
    WriterCompletionRequirementKind, now_iso8601, work_artifact_path,
};

const PRODUCER: &str = "planning-reference-recovery";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReferenceUpdate {
    pub requirement_id: String,
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReferenceRevision {
    pub revision_id: String,
    pub updated_requirements: usize,
    pub audit_artifact: String,
    pub review_task_ids: Vec<String>,
    pub already_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct ReferenceChange {
    requirement_id: String,
    old_references: Vec<String>,
    new_references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RevisionAudit {
    schema_version: u32,
    revision_id: String,
    work_item_id: String,
    attempt_id: String,
    candidate_commit: String,
    resolution: String,
    resolution_sha256: String,
    changes: Vec<ReferenceChange>,
    prior_behaviors_sha256: String,
    revised_behaviors_sha256: String,
    completion_matrix_artifact: String,
    created_at: String,
}

pub fn revise(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    updates: &[TestReferenceUpdate],
    resolution: &str,
) -> Result<TestReferenceRevision> {
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire planning-reference recovery lock")?;
    let resolution = resolution.trim();
    if resolution.is_empty() {
        bail!("--resolution must record the human authorization");
    }
    if resolution.len() > crate::work_model::MAX_ROUND_CAP_RESOLUTION_BYTES {
        bail!("--resolution is too long");
    }
    let desired = normalize_updates(updates)?;
    let item = store.read_work_item(work_item_id)?;
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;
    let context = item
        .planning_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Work Item has no stored planning context"))?;
    let source = authoritative_behavior_source(context)?;
    let old_contract = WriterCompletionContract::from_planning_context(context)
        .ok_or_else(|| anyhow::anyhow!("Work Item has no Writer completion contract"))?;
    let changes = requested_changes(&old_contract, &desired)?;
    if changes
        .iter()
        .all(|change| change.old_references == change.new_references)
    {
        return existing_revision(project_root, attempt, &changes).ok_or_else(|| {
            anyhow::anyhow!(
                "the requested Test references are already stored, but no audited revision records them"
            )
        });
    }
    ensure_eligible(attempt)?;

    let revised_source = revise_markdown(source, &desired)?;
    let mut revised_context = context.clone();
    if revised_context.combined.as_deref() == Some(source) {
        revised_context.combined = Some(revised_source.clone());
        if let Some(behaviors) = context
            .behaviors
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            revised_context.behaviors = Some(revise_markdown(behaviors, &desired)?);
        }
    } else {
        revised_context.behaviors = Some(revised_source.clone());
    }
    if let Some(revised_behaviors) = revised_context.behaviors.as_deref()
        && let Some(approved) = revised_context.approved_revisions.get_mut("behaviors")
    {
        let digest = digest(revised_behaviors.as_bytes());
        *approved = ApprovedPlanningRevision {
            revision_id: format!("behaviors:{digest}"),
            sha256: digest,
        };
    }
    let new_contract = WriterCompletionContract::from_planning_context(&revised_context)
        .ok_or_else(|| anyhow::anyhow!("revised planning context lost its completion contract"))?;
    validate_contract_delta(&old_contract, &new_contract, &desired)?;

    let latest_writer = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| anyhow::anyhow!("Attempt has no completed Writer candidate"))?;
    let candidate_commit = latest_writer
        .output
        .as_ref()
        .map(|output| output.commit.clone())
        .ok_or_else(|| anyhow::anyhow!("latest Writer has no candidate commit"))?;
    let revision_id = revision_id(&changes);
    let directory = format!("planning-reference-revisions/{revision_id}");
    let matrix_artifact = work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("{directory}/writer-completion.json"),
    );
    let audit_artifact = work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("{directory}/revision.json"),
    );
    let revised_matrix =
        revise_completion_matrix(project_root, attempt, latest_writer, &new_contract)?;
    write_once(
        &project_root.join(&matrix_artifact),
        &serde_json::to_vec_pretty(&revised_matrix)?,
    )?;
    let proposed_audit = RevisionAudit {
        schema_version: SCHEMA_VERSION,
        revision_id: revision_id.clone(),
        work_item_id: work_item_id.to_string(),
        attempt_id: attempt_id.to_string(),
        candidate_commit,
        resolution: resolution.to_string(),
        resolution_sha256: digest(resolution.as_bytes()),
        changes: changes.clone(),
        prior_behaviors_sha256: digest(source.as_bytes()),
        revised_behaviors_sha256: digest(revised_source.as_bytes()),
        completion_matrix_artifact: matrix_artifact.clone(),
        created_at: now_iso8601(),
    };
    let audit_path = project_root.join(&audit_artifact);
    let audit = match fs::read(&audit_path) {
        Ok(bytes) => {
            let existing: RevisionAudit = serde_json::from_slice(&bytes)
                .context("parse existing Test-reference revision audit")?;
            if existing.schema_version != proposed_audit.schema_version
                || existing.revision_id != proposed_audit.revision_id
                || existing.work_item_id != proposed_audit.work_item_id
                || existing.attempt_id != proposed_audit.attempt_id
                || existing.candidate_commit != proposed_audit.candidate_commit
                || existing.resolution != proposed_audit.resolution
                || existing.changes != proposed_audit.changes
                || existing.prior_behaviors_sha256 != proposed_audit.prior_behaviors_sha256
                || existing.revised_behaviors_sha256 != proposed_audit.revised_behaviors_sha256
                || existing.completion_matrix_artifact != proposed_audit.completion_matrix_artifact
            {
                bail!("existing Test-reference revision audit does not match this recovery");
            }
            existing
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            write_once(&audit_path, &serde_json::to_vec_pretty(&proposed_audit)?)?;
            proposed_audit
        }
        Err(error) => return Err(error.into()),
    };
    debug_assert_eq!(audit.revision_id, revision_id);

    let audit_ref = ArtifactRef {
        producer_id: PRODUCER.to_string(),
        path: audit_artifact.clone(),
    };
    let matrix_ref = ArtifactRef {
        producer_id: PRODUCER.to_string(),
        path: matrix_artifact,
    };
    let expected_context = context.clone();
    let metadata = DurableTransitionMetadata {
        origin: DurableTransitionOrigin::External,
        reason: Some("revise approved behavior Test references".to_string()),
        evidence_ref: Some(audit_artifact.clone()),
    };
    let review_task_ids =
        store.mutate_work_item_with_transition(work_item_id, &metadata, |fresh| {
            let current_context = fresh.planning_context.as_ref().ok_or_else(|| {
                WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: "planning context disappeared before revision".to_string(),
                }
            })?;
            if current_context != &expected_context {
                return Err(WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: "planning context changed before revision".to_string(),
                });
            }
            let index = fresh
                .attempts
                .iter()
                .position(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound {
                    id: attempt_id.to_string(),
                })?;
            ensure_eligible(&fresh.attempts[index]).map_err(|e| {
                WorkModelError::AttemptResumeIneligible {
                    attempt_id: attempt_id.to_string(),
                    reason: format!("Test-reference frontier changed before revision: {e:#}"),
                }
            })?;
            fresh.planning_context = Some(revised_context.clone());
            fresh.attempts[index].writer_completion_contract = Some(new_contract.clone());
            for artifact in [&audit_ref, &matrix_ref] {
                if !fresh.attempts[index].artifacts.contains(artifact) {
                    fresh.attempts[index].artifacts.push((*artifact).clone());
                }
            }
            let task_ids = fresh.add_next_review_tasks(attempt_id, &["behaviors", "tests"])?;
            let attempt = &mut fresh.attempts[index];
            for task in attempt
                .tasks
                .iter_mut()
                .filter(|task| task_ids.contains(&task.id))
            {
                if !task.input_artifacts.contains(&audit_ref) {
                    task.input_artifacts.push(audit_ref.clone());
                }
            }
            attempt.pause_kind = None;
            attempt.pause_recovery = None;
            attempt.completed_at = None;
            Ok(task_ids)
        })?;

    Ok(TestReferenceRevision {
        revision_id,
        updated_requirements: changes.len(),
        audit_artifact,
        review_task_ids,
        already_applied: false,
    })
}

fn authoritative_behavior_source(context: &crate::work_model::PlanningContext) -> Result<&str> {
    if let Some(combined) = context
        .combined
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(combined);
    }
    context
        .behaviors
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Work Item has no approved behavior markdown"))
}

fn normalize_updates(updates: &[TestReferenceUpdate]) -> Result<BTreeMap<String, Vec<String>>> {
    if updates.is_empty() {
        bail!("at least one --reference is required");
    }
    let mut desired = BTreeMap::<String, Vec<String>>::new();
    for update in updates {
        let id = update.requirement_id.trim();
        let reference = update.reference.trim();
        if id.is_empty() || reference.is_empty() {
            bail!("behavior requirement IDs and Test references must not be empty");
        }
        if id.contains(['\r', '\n']) || reference.contains(['\r', '\n']) {
            bail!("behavior requirement IDs and Test references must each fit on one line");
        }
        if reference.starts_with("Untestable:") {
            bail!("--reference accepts a Test reference, not an Untestable marker");
        }
        if !id.starts_with("behavior:") {
            bail!("{id:?} is not a behavior completion requirement ID");
        }
        let refs = desired.entry(id.to_string()).or_default();
        if !refs.iter().any(|existing| existing == reference) {
            refs.push(reference.to_string());
        }
    }
    Ok(desired)
}

fn requested_changes(
    contract: &WriterCompletionContract,
    desired: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<ReferenceChange>> {
    let mut changes = Vec::new();
    for (id, references) in desired {
        let requirement = contract
            .requirements
            .iter()
            .find(|requirement| requirement.id == *id)
            .ok_or_else(|| anyhow::anyhow!("behavior requirement {id:?} was not found"))?;
        if requirement.kind != WriterCompletionRequirementKind::Behavior {
            bail!("completion requirement {id:?} is not a behavior");
        }
        if requirement
            .verification_refs
            .iter()
            .any(|reference| reference.starts_with("Untestable:"))
        {
            bail!("behavior requirement {id:?} is marked Untestable");
        }
        changes.push(ReferenceChange {
            requirement_id: id.clone(),
            old_references: requirement.verification_refs.clone(),
            new_references: references.clone(),
        });
    }
    Ok(changes)
}

fn ensure_eligible(attempt: &crate::work_model::Attempt) -> Result<()> {
    if attempt.status != AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(PauseKind::RoundCap)
        || attempt.review_state != Some(AttemptReviewState::Failed)
    {
        bail!("Attempt is not paused at a failed review round cap");
    }
    if !attempt.tasks.iter().any(|task| {
        task.kind == TaskKind::Review
            && matches!(task.role.as_str(), "behaviors" | "tests")
            && matches!(task.status, TaskStatus::Complete | TaskStatus::Failed)
    }) {
        bail!("Attempt has no settled Behaviors or Tests review to recover");
    }
    Ok(())
}

fn revise_markdown(source: &str, desired: &BTreeMap<String, Vec<String>>) -> Result<String> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut output = Vec::<String>::new();
    let mut area = "unscoped".to_string();
    let mut current: Option<String> = None;
    let mut inserted = false;
    let mut found = HashSet::new();
    let mut identities = HashMap::<String, usize>::new();

    let finish_block = |output: &mut Vec<String>, current: &Option<String>, inserted: &mut bool| {
        if let Some(id) = current
            && !*inserted
            && let Some(references) = desired.get(id)
        {
            output.extend(
                references
                    .iter()
                    .map(|reference| format!("Test: {reference}")),
            );
            *inserted = true;
        }
    };

    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            finish_block(&mut output, &current, &mut inserted);
            current = None;
            area = completion_slug(trimmed.trim_start_matches("## "));
        } else if let Some(behavior) = trimmed.strip_prefix("### ").filter(|id| {
            id.strip_prefix('B').is_some_and(|number| {
                !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
            })
        }) {
            finish_block(&mut output, &current, &mut inserted);
            let base = format!("behavior:{area}:{}", behavior.to_ascii_lowercase());
            let count = identities.entry(base.clone()).or_default();
            *count += 1;
            let id = if *count == 1 {
                base
            } else {
                format!("{base}:{count}")
            };
            if desired.contains_key(&id) {
                found.insert(id.clone());
            }
            current = Some(id);
            inserted = false;
        }
        if current.as_ref().is_some_and(|id| desired.contains_key(id))
            && trimmed.starts_with("Test:")
        {
            if !inserted {
                output.extend(
                    desired[current.as_ref().unwrap()]
                        .iter()
                        .map(|reference| format!("Test: {reference}")),
                );
                inserted = true;
            }
            continue;
        }
        output.push(line.to_string());
    }
    finish_block(&mut output, &current, &mut inserted);
    if found.len() != desired.len() {
        let missing = desired
            .keys()
            .filter(|id| !found.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        bail!("behavior markdown did not contain selected requirements: {missing:?}");
    }
    let mut revised = output.join("\n");
    if source.ends_with('\n') {
        revised.push('\n');
    }
    Ok(revised)
}

fn completion_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            if dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch);
            dash = false;
        } else {
            dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn validate_contract_delta(
    old: &WriterCompletionContract,
    new: &WriterCompletionContract,
    desired: &BTreeMap<String, Vec<String>>,
) -> Result<()> {
    if old.version != new.version || old.requirements.len() != new.requirements.len() {
        bail!("Test-reference revision changed the completion-contract shape");
    }
    for (before, after) in old.requirements.iter().zip(&new.requirements) {
        if before.id != after.id
            || before.kind != after.kind
            || before.source != after.source
            || before.requirement != after.requirement
        {
            bail!(
                "Test-reference revision changed requirement {:?}",
                before.id
            );
        }
        let expected = desired.get(&before.id).unwrap_or(&before.verification_refs);
        if &after.verification_refs != expected {
            bail!(
                "Test-reference revision did not update {:?} exactly",
                before.id
            );
        }
    }
    Ok(())
}

fn revise_completion_matrix(
    project_root: &Path,
    attempt: &crate::work_model::Attempt,
    writer: &crate::work_model::Task,
    contract: &WriterCompletionContract,
) -> Result<WriterCompletionMatrix> {
    let source = attempt
        .artifacts
        .iter()
        .rev()
        .find(|artifact| {
            artifact.path.ends_with("/writer-completion.json")
                && ((artifact.producer_id == "writer-completion-repair"
                    && artifact
                        .path
                        .contains(&format!("/writer-completion-repairs/{}/", writer.id)))
                    || artifact.producer_id == PRODUCER)
        })
        .cloned()
        .or_else(|| {
            writer.artifact_area.as_ref().map(|area| ArtifactRef {
                producer_id: writer.id.clone(),
                path: format!("{}/writer-completion.json", area.path),
            })
        })
        .ok_or_else(|| anyhow::anyhow!("latest Writer has no completion matrix artifact"))?;
    let text = fs::read_to_string(project_root.join(&source.path))
        .with_context(|| format!("read Writer completion matrix {}", source.path))?;
    let mut matrix: WriterCompletionMatrix = serde_json::from_str(&text)
        .context("parse Writer completion matrix for Test-reference revision")?;
    let findings = crate::work_task_executor::writer_completion_findings(project_root, writer)?;
    matrix
        .repair_writer_claims_against(contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    matrix
        .status_against(contract, &writer.id, &findings)
        .map_err(anyhow::Error::msg)?;
    Ok(matrix)
}

fn revision_id(changes: &[ReferenceChange]) -> String {
    let bytes = serde_json::to_vec(changes).expect("reference changes serialize");
    format!("revision-{}", &digest(&bytes)[..12])
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_once(path: &Path, bytes: &[u8]) -> Result<()> {
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

fn existing_revision(
    project_root: &Path,
    attempt: &crate::work_model::Attempt,
    changes: &[ReferenceChange],
) -> Option<TestReferenceRevision> {
    attempt.artifacts.iter().find_map(|artifact| {
        if artifact.producer_id != PRODUCER || !artifact.path.ends_with("/revision.json") {
            return None;
        }
        let audit: RevisionAudit =
            serde_json::from_slice(&fs::read(project_root.join(&artifact.path)).ok()?).ok()?;
        let matches = changes.iter().all(|requested| {
            audit.changes.iter().any(|recorded| {
                recorded.requirement_id == requested.requirement_id
                    && recorded.new_references == requested.new_references
            })
        });
        matches.then(|| TestReferenceRevision {
            revision_id: audit.revision_id,
            updated_requirements: changes.len(),
            audit_artifact: artifact.path.clone(),
            review_task_ids: Vec::new(),
            already_applied: true,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_model::{
        PlanningContext, TaskOutput, WriterCompletionState, WriterVerificationClaim,
        WriterVerificationResult,
    };

    const BEHAVIORS: &str = "# Behaviors\n\n## Merge Candidate verification\n\n### B1\n\nWHEN relevant, THE SYSTEM SHALL require evidence.\nTest: tests/policy.py::relevant\n\n### B2\n\nIF stale, THEN THE SYSTEM SHALL reject evidence.\nTest: tests/policy.py::stale\n\n## Local transition\n\n### B1\n\nWHEN idle, THE SYSTEM SHALL proceed.\nTest: tests/local.rs::idle\n";

    #[test]
    fn revises_only_selected_behavior_references() {
        let desired = BTreeMap::from([
            (
                "behavior:merge-candidate-verification:b1".to_string(),
                vec!["python3 tests/fluent_merge_readiness.py".to_string()],
            ),
            (
                "behavior:merge-candidate-verification:b2".to_string(),
                vec!["python3 tests/fluent_merge_readiness.py".to_string()],
            ),
        ]);
        let revised = revise_markdown(BEHAVIORS, &desired).unwrap();
        assert_eq!(
            revised
                .matches("Test: python3 tests/fluent_merge_readiness.py")
                .count(),
            2
        );
        assert!(revised.contains("Test: tests/local.rs::idle"));
        assert!(!revised.contains("tests/policy.py::relevant"));
        assert!(!revised.contains("tests/policy.py::stale"));
    }

    #[test]
    fn rejects_unknown_behavior_without_partial_output() {
        let desired = BTreeMap::from([(
            "behavior:missing:b1".to_string(),
            vec!["python3 tests/readiness.py".to_string()],
        )]);
        let error = revise_markdown(BEHAVIORS, &desired).unwrap_err();
        assert!(format!("{error:#}").contains("did not contain selected requirements"));
    }

    #[test]
    fn public_recovery_revises_selected_test_references() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let store = WorkModelStore::new(project);
        let mut item = crate::work_model::WorkItem::planned("work-1", "Revise references");
        item.planning_context = Some(PlanningContext {
            behaviors: Some(BEHAVIORS.to_string()),
            combined: Some(format!("# Brief\n\nRecover references.\n\n{BEHAVIORS}")),
            ..Default::default()
        });
        item.add_initial_attempt("attempt-1").unwrap();
        let contract = item.attempts[0].writer_completion_contract.clone().unwrap();
        let writer_id = item.attempts[0].tasks[0].id.clone();
        let writer_area = item.attempts[0].tasks[0]
            .artifact_area
            .as_ref()
            .unwrap()
            .path
            .clone();
        let mut matrix =
            WriterCompletionMatrix::skeleton(&contract, &writer_id, &[], None).unwrap();
        for row in &mut matrix.rows {
            row.state = WriterCompletionState::Complete;
            row.implementation_evidence = vec!["candidate: unchanged implementation".to_string()];
            row.verification = vec![WriterVerificationClaim {
                command: "tests/original".to_string(),
                result: WriterVerificationResult::Pass,
            }];
        }
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: "../candidate".to_string(),
            source_branch: "work/references".to_string(),
            base_commit: Some("base".to_string()),
            commit: "candidate".to_string(),
            no_change: None,
            learner_canonicalization: None,
        });
        item.add_review_tasks("attempt-1", &["behaviors", "tests"])
            .unwrap();
        let attempt = &mut item.attempts[0];
        for task in attempt
            .tasks
            .iter_mut()
            .filter(|task| task.kind == TaskKind::Review)
        {
            task.status = TaskStatus::Complete;
        }
        attempt.status = AttemptStatus::NeedsUser;
        attempt.pause_kind = Some(PauseKind::RoundCap);
        attempt.review_state = Some(AttemptReviewState::Failed);
        attempt.completed_at = Some(now_iso8601());
        store.create_work_item(&item).unwrap();
        let matrix_path = project.join(format!("{writer_area}/writer-completion.json"));
        fs::create_dir_all(matrix_path.parent().unwrap()).unwrap();
        fs::write(&matrix_path, serde_json::to_vec_pretty(&matrix).unwrap()).unwrap();

        let updates = [TestReferenceUpdate {
            requirement_id: "behavior:merge-candidate-verification:b1".to_string(),
            reference: "python3 tests/fluent_merge_readiness.py".to_string(),
        }];
        let result = revise(
            project,
            &store,
            "work-1",
            "attempt-1",
            &updates,
            "The public readiness journey owns this behavior.",
        )
        .unwrap();
        assert!(!result.already_applied);
        assert_eq!(result.review_task_ids.len(), 2);

        let stored = store.read_work_item("work-1").unwrap();
        let attempt = &stored.attempts[0];
        assert_eq!(attempt.status, AttemptStatus::Reviewing);
        assert!(attempt.pause_kind.is_none());
        assert_eq!(
            attempt.tasks.len(),
            5,
            "the Writer and prior reviews remain history"
        );
        let revised = attempt
            .writer_completion_contract
            .as_ref()
            .unwrap()
            .requirements
            .iter()
            .find(|row| row.id == updates[0].requirement_id)
            .unwrap();
        assert_eq!(
            revised.verification_refs,
            vec![updates[0].reference.clone()]
        );
        let planning = stored.planning_context.as_ref().unwrap();
        assert!(
            planning
                .behaviors
                .as_deref()
                .unwrap()
                .contains("Test: python3 tests/fluent_merge_readiness.py")
        );
        assert!(
            planning
                .combined
                .as_deref()
                .unwrap()
                .contains("Test: python3 tests/fluent_merge_readiness.py")
        );
        let audit: RevisionAudit =
            serde_json::from_slice(&fs::read(project.join(&result.audit_artifact)).unwrap())
                .unwrap();
        assert_eq!(
            audit.changes[0].old_references,
            vec!["tests/policy.py::relevant"]
        );
        assert_eq!(
            audit.changes[0].new_references,
            vec![updates[0].reference.clone()]
        );
        assert_eq!(
            audit.resolution,
            "The public readiness journey owns this behavior."
        );

        let repeated = revise(
            project,
            &store,
            "work-1",
            "attempt-1",
            &updates,
            "A repeated invocation must not create another review round.",
        )
        .unwrap();
        assert!(repeated.already_applied);
        assert_eq!(
            store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .len(),
            5
        );

        let different_update = [TestReferenceUpdate {
            requirement_id: "behavior:merge-candidate-verification:b2".to_string(),
            reference: "python3 tests/other_readiness.py".to_string(),
        }];
        let error = revise(
            project,
            &store,
            "work-1",
            "attempt-1",
            &different_update,
            "This should be rejected after the frontier advances.",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("not paused at a failed review round cap"));
        assert_eq!(
            store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .len(),
            5,
            "a stale revision must not add Tasks"
        );
    }
}
