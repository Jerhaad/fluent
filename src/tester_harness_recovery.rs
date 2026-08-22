//! Audited recovery for project-owned Tester configuration failures.
//!
//! Ordinary resume keeps the reviewed candidate immutable. This module is the
//! one explicit transition that authorizes a Writer to commit a narrowly scoped
//! project Tester repair before Fluent repeats review and the final Tester.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::Path;

use crate::git;
use crate::work_model::{
    ArtifactRef, AttemptStatus, PauseKind, TESTER_HARNESS_REPAIR_ROLE, TaskKind, TaskStatus,
    WorkModelError, WorkModelStore, now_iso8601, resolve_managed_sibling_workspace_path,
    work_artifact_path,
};

const REPAIR_SCHEMA_VERSION: u32 = 1;
const MAX_REPAIR_RESOLUTION_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TesterHarnessRepairRequest {
    pub task_id: String,
    pub candidate_commit: String,
    pub already_requested: bool,
}

#[derive(Serialize)]
struct TesterHarnessRepairAuthority<'a> {
    schema_version: u32,
    work_item_id: &'a str,
    attempt_id: &'a str,
    failed_tester_task_id: &'a str,
    candidate_commit: &'a str,
    resolution: &'a str,
    requested_at: &'a str,
}

pub fn request(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    resolution: &str,
) -> Result<TesterHarnessRepairRequest> {
    let resolution = normalize_resolution(resolution)?;
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))?;
    let item = store.read_work_item(work_item_id)?;
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| anyhow::anyhow!("Attempt {attempt_id:?} not found"))?;

    if let Some(existing) = attempt
        .tasks
        .iter()
        .find(|task| task.kind == TaskKind::Write && task.role == TESTER_HARNESS_REPAIR_ROLE)
    {
        if existing.instructions.as_deref() != Some(repair_instructions(&resolution).as_str()) {
            bail!(
                "Tester-harness repair was already requested by Task {:?} with a different resolution",
                existing.id
            );
        }
        if attempt.status != AttemptStatus::NeedsUser
            || attempt.pause_kind != Some(PauseKind::TesterHarness)
            || existing.status != TaskStatus::Planned
        {
            return Err(ineligible(format!(
                "repair Writer {:?} has already advanced to {}; follow the Attempt's current next action",
                existing.id,
                existing.status.as_str()
            ))
            .into());
        }
        let candidate_commit = existing
            .review_context
            .as_ref()
            .map(|context| context.candidate_commit.clone())
            .ok_or_else(|| ineligible("the existing repair Writer has no pinned candidate"))?;
        return Ok(TesterHarnessRepairRequest {
            task_id: existing.id.clone(),
            candidate_commit,
            already_requested: true,
        });
    }

    if attempt.status != AttemptStatus::NeedsUser
        || attempt.pause_kind != Some(PauseKind::TesterHarness)
        || attempt.pause_recovery.is_some()
    {
        return Err(ineligible(
            "the Attempt is not paused on a project-owned Tester harness failure",
        )
        .into());
    }
    if item
        .merge_candidates
        .iter()
        .any(|candidate| candidate.attempt_id == attempt_id)
    {
        return Err(ineligible("the Attempt already has a Merge Candidate").into());
    }

    let mut paused_testers = attempt
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Tester && task.status == TaskStatus::NeedsUser);
    let tester = paused_testers
        .next()
        .ok_or_else(|| ineligible("the pause has no needs-user Tester Task"))?;
    if paused_testers.next().is_some() {
        return Err(ineligible("the pause has more than one needs-user Tester Task").into());
    }
    let context = tester
        .review_context
        .as_ref()
        .ok_or_else(|| ineligible("the paused Tester has no candidate context"))?;
    let repair_context = context.clone();
    let latest_write = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .ok_or_else(|| ineligible("the Attempt has no completed Writer Task"))?;
    let output = latest_write
        .output
        .as_ref()
        .ok_or_else(|| ineligible("the latest Writer has no candidate output"))?;
    if output.commit != context.candidate_commit
        || output.workspace_id != context.candidate_workspace_id
        || output.workspace_path != context.candidate_workspace_path
        || output.source_branch != context.source_branch
    {
        return Err(
            ineligible("the paused Tester is not bound to the latest Writer candidate").into(),
        );
    }

    let workspace = resolve_managed_sibling_workspace_path(
        project_root,
        &context.candidate_workspace_path,
        "Tester-harness repair candidate workspace",
    )?;
    let head = git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "resolve Tester-harness repair candidate HEAD",
    )?;
    if head != context.candidate_commit {
        return Err(ineligible(format!(
            "candidate HEAD changed from {} to {head}; ordinary drift remains rejected",
            context.candidate_commit
        ))
        .into());
    }
    let status = git::run_stdout(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
        "check Tester-harness repair candidate cleanliness",
    )?;
    if !status.is_empty() {
        return Err(ineligible(format!(
            "candidate workspace is dirty before the repair Writer:\n{status}"
        ))
        .into());
    }
    let tester_area = tester
        .artifact_area
        .as_ref()
        .ok_or_else(|| ineligible("the paused Tester has no artifact area"))?;

    let next_round = attempt
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Write)
        .count()
        + 1;
    let task_id = format!("{attempt_id}-write-{next_round}");
    let requested_at = now_iso8601();
    let authority_rel = work_artifact_path(
        work_item_id,
        attempt_id,
        &format!("tester-harness-repairs/{task_id}/repair.json"),
    );
    let authority = TesterHarnessRepairAuthority {
        schema_version: REPAIR_SCHEMA_VERSION,
        work_item_id,
        attempt_id,
        failed_tester_task_id: &tester.id,
        candidate_commit: &context.candidate_commit,
        resolution: &resolution,
        requested_at: &requested_at,
    };
    let authority_path = project_root.join(&authority_rel);
    fs::create_dir_all(
        authority_path
            .parent()
            .expect("Tester-harness authority path has a parent"),
    )?;
    crate::atomic_write::atomic_write(&authority_path, &serde_json::to_vec_pretty(&authority)?)
        .with_context(|| {
            format!(
                "write Tester-harness repair authority {}",
                authority_path.display()
            )
        })?;

    let mut inputs = vec![ArtifactRef {
        producer_id: "tester-harness-repair-authority".to_string(),
        path: authority_rel.clone(),
    }];
    for filename in ["tester-results.json", "commands.json"] {
        let path = format!("{}/{filename}", tester_area.path);
        if project_root.join(&path).is_file() {
            inputs.push(ArtifactRef {
                producer_id: tester.id.clone(),
                path,
            });
        }
    }
    let handoff = work_artifact_path(work_item_id, attempt_id, "needs-user.md");
    if project_root.join(&handoff).is_file() {
        inputs.push(ArtifactRef {
            producer_id: tester.id.clone(),
            path: handoff,
        });
    }

    let expected = item.clone();
    let instructions = repair_instructions(&resolution);
    let failed_tester_id = tester.id.clone();
    let candidate_commit = context.candidate_commit.clone();
    let stored_task_id = store.mutate_work_item_from_external(
        work_item_id,
        format!("approve Tester-harness repair for {attempt_id}"),
        Some(authority_rel),
        |fresh| {
            if fresh != &expected {
                return Err(WorkModelError::TesterHarnessRepairIneligible {
                    reason: "the Work Item changed while repair authority was being recorded"
                        .to_string(),
                });
            }
            let task_id = fresh.add_next_write_round(attempt_id, inputs.clone())?;
            let attempt = fresh
                .attempts
                .iter_mut()
                .find(|attempt| attempt.id == attempt_id)
                .ok_or_else(|| WorkModelError::AttemptNotFound {
                    id: attempt_id.to_string(),
                })?;
            let failed_tester = attempt
                .tasks
                .iter_mut()
                .find(|task| task.id == failed_tester_id)
                .ok_or_else(|| WorkModelError::TaskNotFound {
                    id: failed_tester_id.clone(),
                })?;
            failed_tester.status = TaskStatus::Failed;
            let repair = attempt
                .tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .ok_or_else(|| WorkModelError::TaskNotFound {
                    id: task_id.clone(),
                })?;
            repair.role = TESTER_HARNESS_REPAIR_ROLE.to_string();
            repair.instructions = Some(instructions.clone());
            repair.review_context = Some(repair_context.clone());
            attempt.status = AttemptStatus::NeedsUser;
            attempt.pause_kind = Some(PauseKind::TesterHarness);
            attempt.pause_recovery = None;
            attempt.completed_at = Some(requested_at.clone());
            Ok(task_id)
        },
    )?;

    Ok(TesterHarnessRepairRequest {
        task_id: stored_task_id,
        candidate_commit,
        already_requested: false,
    })
}

fn repair_instructions(resolution: &str) -> String {
    format!(
        "Repair the project-owned Tester harness described by the attached immutable artifacts.\n\nOperator decision:\n{resolution}\n\nChange only `.fluent/tester.yaml` and `.fluent/extract-tester-results`. Do not change product code or product tests. Preserve authoritative failures while producing unique normalized result identities. Commit the tracked repair; Fluent will bind all new Tester and review evidence to that commit."
    )
}

fn normalize_resolution(resolution: &str) -> Result<String> {
    let normalized = resolution.trim();
    if normalized.is_empty() {
        return Err(ineligible("provide a non-empty --resolution for the repair Writer").into());
    }
    if normalized.len() > MAX_REPAIR_RESOLUTION_BYTES {
        return Err(ineligible(format!(
            "the resolution is {} bytes; limit is {MAX_REPAIR_RESOLUTION_BYTES}",
            normalized.len()
        ))
        .into());
    }
    Ok(normalized.to_string())
}

fn ineligible(reason: impl Into<String>) -> WorkModelError {
    WorkModelError::TesterHarnessRepairIneligible {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_model::{
        AttemptReviewState, ReviewContext, Task, TaskArtifactArea, TaskOutput, WorkItem,
        WorkspaceAccess, WorkspaceRef,
    };
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        main: std::path::PathBuf,
        candidate: std::path::PathBuf,
        store: WorkModelStore,
        candidate_commit: String,
    }

    fn git_ok(path: &Path, args: &[&str]) {
        git::run(path, args, "prepare Tester-harness recovery fixture").unwrap();
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        let candidate = tmp.path().join("work-4-work-1-attempt-1");
        fs::create_dir_all(main.join(".fluent")).unwrap();
        git_ok(&main, &["init", "-b", "main"]);
        git_ok(&main, &["config", "user.name", "Mrinal Wadhwa"]);
        git_ok(&main, &["config", "user.email", "mrinal.wadhwa@gmail.com"]);
        fs::write(
            main.join(".fluent/tester.yaml"),
            "commands:\n  - command: true\n    test_harness: shell\n",
        )
        .unwrap();
        fs::write(
            main.join(".fluent/extract-tester-results"),
            "#!/bin/sh\necho '[]'\n",
        )
        .unwrap();
        fs::write(main.join("product.txt"), "product\n").unwrap();
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-m", "Create fixture"]);
        git_ok(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "work/tester-repair",
                candidate.to_str().unwrap(),
            ],
        );
        let candidate_commit =
            git::run_stdout(&candidate, &["rev-parse", "HEAD"], "fixture HEAD").unwrap();

        let store = WorkModelStore::new(&main);
        let mut item = WorkItem::planned("work-1", "Repair Tester harness");
        item.add_initial_attempt("attempt-1").unwrap();
        let attempt = &mut item.attempts[0];
        let writer = &mut attempt.tasks[0];
        writer.status = TaskStatus::Complete;
        writer.workspace_access = WorkspaceAccess {
            reads: vec![WorkspaceRef {
                id: "target".to_string(),
                path: ".".to_string(),
            }],
            writes: vec![WorkspaceRef {
                id: "candidate".to_string(),
                path: "../work-4-work-1-attempt-1".to_string(),
            }],
        };
        writer.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: "../work-4-work-1-attempt-1".to_string(),
            source_branch: "main".to_string(),
            base_commit: Some(candidate_commit.clone()),
            commit: candidate_commit.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        attempt.tasks.push(Task {
            id: "attempt-1-review-tests".to_string(),
            kind: TaskKind::Review,
            status: TaskStatus::Complete,
            role: "tests".to_string(),
            instructions: None,
            work_item_id: "work-1".to_string(),
            attempt_id: Some("attempt-1".to_string()),
            workspace_access: WorkspaceAccess::read_only(vec![WorkspaceRef {
                id: "candidate".to_string(),
                path: "../work-4-work-1-attempt-1".to_string(),
            }]),
            artifact_area: Some(TaskArtifactArea {
                path: work_artifact_path("work-1", "attempt-1", "attempt-1-review-tests"),
            }),
            review_context: Some(ReviewContext {
                candidate_workspace_id: "candidate".to_string(),
                candidate_workspace_path: "../work-4-work-1-attempt-1".to_string(),
                source_branch: "main".to_string(),
                candidate_commit: candidate_commit.clone(),
                base_commit: None,
            }),
            evidence_review_context: None,
            input_artifacts: Vec::new(),
            depends_on: None,
            output: None,
            created_at: Some(now_iso8601()),
            started_at: Some(now_iso8601()),
            completed_at: Some(now_iso8601()),
        });
        let tester_id = "attempt-1-tester";
        let tester_area = work_artifact_path("work-1", "attempt-1", tester_id);
        attempt.tasks.push(Task {
            id: tester_id.to_string(),
            kind: TaskKind::Tester,
            status: TaskStatus::NeedsUser,
            role: "tester".to_string(),
            instructions: None,
            work_item_id: "work-1".to_string(),
            attempt_id: Some("attempt-1".to_string()),
            workspace_access: WorkspaceAccess::read_only(vec![WorkspaceRef {
                id: "candidate".to_string(),
                path: "../work-4-work-1-attempt-1".to_string(),
            }]),
            artifact_area: Some(TaskArtifactArea {
                path: tester_area.clone(),
            }),
            review_context: Some(ReviewContext {
                candidate_workspace_id: "candidate".to_string(),
                candidate_workspace_path: "../work-4-work-1-attempt-1".to_string(),
                source_branch: "main".to_string(),
                candidate_commit: candidate_commit.clone(),
                base_commit: None,
            }),
            evidence_review_context: None,
            input_artifacts: Vec::new(),
            depends_on: None,
            output: None,
            created_at: Some(now_iso8601()),
            started_at: Some(now_iso8601()),
            completed_at: Some(now_iso8601()),
        });
        attempt.status = AttemptStatus::NeedsUser;
        attempt.pause_kind = Some(PauseKind::TesterHarness);
        attempt.review_state = Some(AttemptReviewState::Passed);
        attempt.completed_at = Some(now_iso8601());
        store.create_work_item(&mut item).unwrap();
        fs::create_dir_all(main.join(&tester_area)).unwrap();
        fs::write(main.join(&tester_area).join("commands.json"), "[]\n").unwrap();
        let handoff = main.join(work_artifact_path("work-1", "attempt-1", "needs-user.md"));
        fs::create_dir_all(handoff.parent().unwrap()).unwrap();
        fs::write(handoff, "Tester extractor emitted duplicate IDs.\n").unwrap();

        Fixture {
            _tmp: tmp,
            main,
            candidate,
            store,
            candidate_commit,
        }
    }

    #[test]
    fn repair_instructions_are_narrow_and_include_the_decision() {
        let instructions = repair_instructions("Deduplicate repeated nextest failures.");
        assert!(instructions.contains("Deduplicate repeated nextest failures."));
        assert!(instructions.contains(".fluent/tester.yaml"));
        assert!(instructions.contains("Do not change product code"));
    }

    #[test]
    fn request_records_repair_writer_with_source_provenance() {
        let fixture = fixture();
        let first = request(
            &fixture.main,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Deduplicate repeated nextest failures while preserving failure.",
        )
        .unwrap();
        assert!(!first.already_requested);
        assert_eq!(first.candidate_commit, fixture.candidate_commit);

        let item = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &item.attempts[0];
        assert_eq!(attempt.status, AttemptStatus::NeedsUser);
        assert_eq!(attempt.pause_kind, Some(PauseKind::TesterHarness));
        assert_eq!(attempt.tasks[2].status, TaskStatus::Failed);
        let repair = attempt
            .tasks
            .iter()
            .find(|task| task.id == first.task_id)
            .unwrap();
        assert_eq!(repair.role, TESTER_HARNESS_REPAIR_ROLE);
        assert_eq!(repair.status, TaskStatus::Planned);
        assert!(
            repair
                .instructions
                .as_deref()
                .unwrap()
                .contains("Deduplicate repeated nextest failures while preserving failure.")
        );
        assert!(repair.input_artifacts.iter().any(|artifact| {
            artifact.producer_id == "tester-harness-repair-authority"
                && artifact.path.ends_with("/repair.json")
        }));
        assert!(repair.input_artifacts.iter().any(|artifact| {
            artifact.producer_id == "attempt-1-tester" && artifact.path.ends_with("/commands.json")
        }));
        assert_eq!(
            repair.review_context.as_ref().unwrap().candidate_commit,
            fixture.candidate_commit
        );
        assert!(attempt.has_tester_harness_repair_for("attempt-1-tester"));
        attempt.ensure_resume_ready().unwrap();

        let mut malformed = item.clone();
        malformed.attempts[0]
            .tasks
            .iter_mut()
            .find(|task| task.id == first.task_id)
            .unwrap()
            .review_context = None;
        assert!(matches!(
            malformed.validate(),
            Err(WorkModelError::InvalidTesterHarnessRepairTask { .. })
        ));
    }

    #[test]
    fn repeated_request_reuses_existing_repair_writer() {
        let fixture = fixture();
        let first = request(
            &fixture.main,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Deduplicate repeated nextest failures while preserving failure.",
        )
        .unwrap();
        let second = request(
            &fixture.main,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Deduplicate repeated nextest failures while preserving failure.",
        )
        .unwrap();
        assert!(second.already_requested);
        assert_eq!(second.task_id, first.task_id);
        assert_eq!(
            fixture.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .len(),
            4
        );
    }

    #[test]
    fn request_rejects_dirty_candidate_without_mutating_state() {
        let dirty = fixture();
        fs::write(dirty.candidate.join("product.txt"), "dirty\n").unwrap();
        let error = request(
            &dirty.main,
            &dirty.store,
            "work-1",
            "attempt-1",
            "Repair the extractor.",
        )
        .unwrap_err();
        assert!(error.to_string().contains("candidate workspace is dirty"));
        assert_eq!(
            dirty.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .len(),
            3
        );
    }

    #[test]
    fn request_rejects_drifted_candidate_without_mutating_state() {
        let drifted = fixture();
        fs::write(
            drifted.candidate.join(".fluent/extract-tester-results"),
            "#!/bin/sh\necho repaired\n",
        )
        .unwrap();
        git_ok(
            &drifted.candidate,
            &["add", ".fluent/extract-tester-results"],
        );
        git_ok(&drifted.candidate, &["commit", "-m", "Drift candidate"]);
        let error = request(
            &drifted.main,
            &drifted.store,
            "work-1",
            "attempt-1",
            "Repair the extractor.",
        )
        .unwrap_err();
        assert!(error.to_string().contains("candidate HEAD changed"));
        assert_eq!(
            drifted.store.read_work_item("work-1").unwrap().attempts[0]
                .tasks
                .len(),
            3
        );
    }

    #[test]
    fn repaired_writer_output_pins_fresh_review_and_tester_contexts() {
        let fixture = fixture();
        let original_candidate = fixture.candidate_commit.clone();
        let approved = request(
            &fixture.main,
            &fixture.store,
            "work-1",
            "attempt-1",
            "Deduplicate repeated nextest failures while preserving failure.",
        )
        .unwrap();
        fs::write(
            fixture.candidate.join(".fluent/extract-tester-results"),
            "#!/bin/sh\necho '[]' | awk '!seen[$0]++'\n",
        )
        .unwrap();
        git_ok(
            &fixture.candidate,
            &["add", ".fluent/extract-tester-results"],
        );
        git_ok(
            &fixture.candidate,
            &["commit", "-m", "Repair Tester extractor"],
        );
        let repaired =
            git::run_stdout(&fixture.candidate, &["rev-parse", "HEAD"], "repaired HEAD").unwrap();

        let mut item = fixture.store.read_work_item("work-1").unwrap();
        let attempt = &mut item.attempts[0];
        crate::work_model::reopen_attempt(attempt);
        let repair = attempt
            .tasks
            .iter_mut()
            .find(|task| task.id == approved.task_id)
            .unwrap();
        repair.status = TaskStatus::Complete;
        repair.completed_at = Some(now_iso8601());
        repair.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: "../work-4-work-1-attempt-1".to_string(),
            source_branch: "main".to_string(),
            base_commit: Some(original_candidate.clone()),
            commit: repaired.clone(),
            no_change: None,
            learner_canonicalization: None,
        });
        let reviews = item.add_next_review_tasks("attempt-1", &["tests"]).unwrap();
        let tester = item.add_final_tester_task("attempt-1").unwrap();
        let attempt = &item.attempts[0];
        for task_id in reviews.into_iter().chain([tester]) {
            let context = attempt
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .unwrap()
                .review_context
                .as_ref()
                .unwrap();
            assert_eq!(context.candidate_commit, repaired);
        }
        assert_eq!(attempt.tasks[2].status, TaskStatus::Failed);
        assert_eq!(
            attempt
                .tasks
                .iter()
                .find(|task| task.id == approved.task_id)
                .unwrap()
                .output
                .as_ref()
                .unwrap()
                .base_commit
                .as_deref(),
            Some(original_candidate.as_str())
        );
        for task in &mut item.attempts[0].tasks {
            if task.status == TaskStatus::Planned {
                task.status = TaskStatus::Complete;
                task.completed_at = Some(now_iso8601());
            }
        }
        item.attempts[0].status = AttemptStatus::Complete;
        item.validate().unwrap();
    }
}
