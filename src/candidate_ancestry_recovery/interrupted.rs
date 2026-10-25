//! Restore an interrupted Writer without accepting its replacement history.
//!
//! The immutable intent precedes Git effects. The model lock binds inspection,
//! reference replacement and operation publication; retries reconcile the same
//! intent after either Git or aggregate publication was interrupted.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};

use crate::execution::{ExecutionRecord, ExecutionState};
use crate::follow_up::content_digest;
use crate::git;
use crate::work_model::{
    ArtifactRef, Attempt, AttemptKind, AttemptStatus, PauseKind, RecoveryOperationRequest, Task,
    TaskKind, TaskStatus, WorkItem, WorkModelError, WorkModelStore, work_artifact_path,
};

const PRODUCER: &str = "interrupted-writer-candidate-restoration";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedWriterRestoration {
    pub task_id: String,
    pub rejected_candidate: String,
    pub recorded_candidate: String,
    pub audit_artifact: String,
    pub operation_id: String,
    pub already_restored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct FileBinding {
    canonical_path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct AssignmentSource {
    task_id: String,
    candidate_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RestorationIntent {
    schema_version: u32,
    work_item_id: String,
    attempt_id: String,
    task_id: String,
    source_task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignment_source: Option<AssignmentSource>,
    recorded_candidate: String,
    rejected_candidate: String,
    candidate_tree: String,
    protected_base: String,
    workspace_path: String,
    canonical_workspace: PathBuf,
    common_git_dir: PathBuf,
    branch_ref: String,
    source_ref: String,
    source_ref_commit: String,
    retained_ref: String,
    model_sha256: String,
    evidence: BTreeMap<String, FileBinding>,
    resolution: String,
}

/// Restore only the saved candidate of one settled, output-free Writer. The
/// caller binds its authorization to the rejected HEAD, never an arbitrary target.
pub fn restore_interrupted(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    rejected_candidate: &str,
    resolution: &str,
) -> Result<InterruptedWriterRestoration> {
    let _land = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire interrupted Writer restoration lock")?;
    let resolution = super::normalize_resolution(resolution)?;
    ensure!(
        matches!(rejected_candidate.len(), 40 | 64)
            && rejected_candidate
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "interrupted restoration requires the exact full rejected candidate ID"
    );
    let expected = store.read_work_item(work_item_id)?;
    expected.ensure_mutation_compatible()?;
    let attempt = current_attempt(&expected, attempt_id)?;
    // Keep every execution owner excluded until Git and the aggregate settle.
    // Acquisition failures are errors, including unreadable lock paths.
    let mut leases = Vec::new();
    for task in &attempt.tasks {
        ensure!(
            task.status != TaskStatus::Executing,
            "Task {} is executing",
            task.id
        );
        leases.push(
            acquire_lease(
                project_root,
                &crate::lease::task_lock_path(project_root, work_item_id, &task.id),
            )
            .with_context(|| format!("Task {} has a live owner or unavailable lease", task.id))?,
        );
        if let Some(area) = &task.artifact_area {
            ensure!(
                area.path == work_artifact_path(work_item_id, attempt_id, &task.id),
                "Task artifact area differs from its exact managed identity"
            );
            let area = crate::recovery_contracts::resolve_managed_artifact_area_path(
                project_root,
                &area.path,
            )?;
            ensure!(
                area.is_dir()
                    && fs::canonicalize(&area)?
                        == fs::canonicalize(project_root)?.join(area.strip_prefix(project_root)?),
                "Task artifact area is missing or aliases another location"
            );
            leases.push(
                acquire_lease(project_root, &area.join("execution-owner.lock")).with_context(
                    || {
                        format!(
                            "Task {} has a live execution owner or unavailable lease",
                            task.id
                        )
                    },
                )?,
            );
        }
    }
    leases.push(
        acquire_lease(
            project_root,
            &crate::lease::learner_lock_path(project_root, work_item_id, attempt_id),
        )
        .context("Learner has a live owner or unavailable lease")?,
    );
    leases.push(
        acquire_lease(
            project_root,
            &crate::queue::dispatch_lease_path(project_root, work_item_id),
        )
        .context("Attempt has a live dispatch owner or unavailable lease")?,
    );

    let restored = store.mutate_work_item_from_external(
        work_item_id,
        "restore interrupted Writer candidate",
        None,
        |fresh| {
            let mut apply = || -> Result<InterruptedWriterRestoration> {
                ensure!(
                    fresh == &expected,
                    "Work changed before interrupted restoration acquired its model lock"
                );
                let attempt = current_attempt(fresh, attempt_id)?;
                let task = interrupted_task(attempt)?;
                let audit_artifact = work_artifact_path(
                    work_item_id,
                    attempt_id,
                    &format!("candidate-ancestry-restorations/{}/intent.json", task.id),
                );
                let reference = ArtifactRef {
                    producer_id: PRODUCER.into(),
                    path: audit_artifact.clone(),
                };
                let existing = read_intent(project_root, &audit_artifact)?;
                let intent = inspect(
                    project_root,
                    fresh,
                    attempt_id,
                    rejected_candidate,
                    &resolution,
                    existing.as_ref(),
                    &reference,
                )?;
                if let Some(existing) = &existing {
                    ensure!(
                        existing == &intent,
                        "interrupted restoration binding changed from its immutable intent"
                    );
                }
                let bytes = serde_json::to_vec_pretty(&intent)?;
                ensure!(
                    bytes.len() as u64 <= super::MAX_RECOVERY_EVIDENCE_BYTES,
                    "interrupted restoration intent exceeds its evidence bound"
                );
                let operation = operation(&intent, &bytes);
                let applied = fresh
                    .validate_recovery_operation(project_root, &operation)?
                    .is_some();
                ensure!(
                    attempt.artifacts.contains(&reference) == applied,
                    "interrupted restoration audit and operation publication disagree"
                );
                let workspace = &intent.canonical_workspace;
                let head = git::run_stdout(
                    workspace,
                    &["rev-parse", "HEAD"],
                    "read interrupted candidate",
                )?;
                ensure!(
                    !applied || head == intent.recorded_candidate,
                    "candidate changed after the recorded restoration"
                );

                // This read-only transaction also proves that Git supports checked
                // symbolic-reference verification before publishing any intent.
                let head_guard = PreparedRefs::hold_head(&intent)?;
                checked_refs(&intent, &head, None)?;
                publication_boundary(project_root, &intent, "before-intent")?;
                crate::work_model::publish_recovery_snapshot(
                    project_root,
                    &audit_artifact,
                    &bytes,
                )?;
                publication_boundary(project_root, &intent, "after-intent")?;
                retain_rejected_ref(&intent)?;
                publication_boundary(project_root, &intent, "after-retain")?;
                // Re-read every file after intent/ref publication. A changed proof
                // cannot authorize the candidate update, even under the model lock.
                ensure!(
                    protected_evidence(project_root, attempt)? == intent.evidence,
                    "interrupted restoration evidence changed before candidate replacement"
                );
                checked_refs(&intent, &head, Some(&intent.recorded_candidate))?;
                drop(head_guard);
                publication_boundary(project_root, &intent, "after-candidate")?;
                ensure!(
                    git::run_stdout(
                        workspace,
                        &["status", "--porcelain", "--untracked-files=all"],
                        "verify restored worktree"
                    )?
                    .is_empty(),
                    "candidate became dirty during interrupted restoration"
                );
                if !applied {
                    let attempt = fresh
                        .attempts
                        .iter_mut()
                        .find(|attempt| attempt.id == attempt_id)
                        .unwrap();
                    attempt.artifacts.push(reference);
                    attempt.record_recovery_operation(
                        &operation,
                        Some(format!("fluent attempt run {work_item_id} {attempt_id}")),
                    )?;
                }
                Ok(InterruptedWriterRestoration {
                    task_id: intent.task_id,
                    rejected_candidate: intent.rejected_candidate,
                    recorded_candidate: intent.recorded_candidate,
                    audit_artifact,
                    operation_id: operation.operation_id(),
                    already_restored: applied,
                })
            };
            apply().map_err(|error| WorkModelError::AttemptResumeIneligible {
                attempt_id: attempt_id.into(),
                reason: format!("interrupted Writer restoration rejected: {error:#}"),
            })
        },
    )?;
    #[cfg(feature = "test-support")]
    if let Some(intent) = read_intent(project_root, &restored.audit_artifact)? {
        publication_boundary(project_root, &intent, "after-operation")?;
    }
    Ok(restored)
}

// Fault injection is compiled out of production and requires the exact Work,
// Attempt, Task and publication point. Pauses expire and never launch children.
#[cfg(feature = "test-support")]
fn publication_boundary(root: &Path, intent: &RestorationIntent, point: &str) -> Result<()> {
    let scope = format!(
        "{}:{}:{}:{point}",
        intent.work_item_id, intent.attempt_id, intent.task_id
    );
    if std::env::var("FLUENT_TEST_INTERRUPTED_RESTORATION_EXIT").as_deref() == Ok(&scope) {
        eprintln!("interrupted restoration fixture exits at {point}");
        std::process::exit(86);
    }
    if std::env::var("FLUENT_TEST_INTERRUPTED_RESTORATION_PAUSE").as_deref() == Ok(&scope) {
        let marker = root.join(work_artifact_path(
            &intent.work_item_id,
            &intent.attempt_id,
            &format!(
                "candidate-ancestry-restorations/{}/{point}.test-ready",
                intent.task_id
            ),
        ));
        fs::write(&marker, b"ready")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !marker.with_extension("release").exists() {
            ensure!(
                std::time::Instant::now() < deadline,
                "restoration fixture pause expired"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

#[cfg(not(feature = "test-support"))]
fn publication_boundary(_: &Path, _: &RestorationIntent, _: &str) -> Result<()> {
    Ok(())
}

fn current_attempt<'a>(item: &'a WorkItem, attempt_id: &str) -> Result<&'a Attempt> {
    item.ensure_mutation_compatible()?;
    let attempt = item
        .attempts
        .last()
        .filter(|attempt| attempt.id == attempt_id)
        .context("interrupted restoration requires the current Attempt")?;
    ensure!(
        attempt.kind == AttemptKind::Write
            && attempt.status == AttemptStatus::NeedsUser
            && attempt.pause_kind == Some(PauseKind::Interrupted),
        "interrupted restoration requires an Interrupted pause"
    );
    ensure!(
        attempt
            .cancellation
            .as_ref()
            .is_some_and(|stop| stop.settled_at.is_some() && stop.resumed_at.is_none()),
        "interrupted restoration requires active settled cancellation"
    );
    ensure!(
        !item
            .merge_candidates
            .iter()
            .any(|candidate| candidate.attempt_id == attempt_id),
        "interrupted restoration is ineligible after a Merge Candidate exists"
    );
    Ok(attempt)
}

fn acquire_lease(root: &Path, path: &Path) -> Result<crate::lease::TaskLease> {
    let relative = path.strip_prefix(root)?;
    ensure!(
        relative.starts_with(".fluent/work"),
        "restoration lease must be managed"
    );
    let mut current = fs::canonicalize(root)?;
    for part in relative.components() {
        ensure!(
            matches!(part, Component::Normal(_)),
            "restoration lease path is not canonical"
        );
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "restoration lease aliases another location: {}",
                current.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    crate::lease::acquire(path).map_err(Into::into)
}

fn interrupted_task(attempt: &Attempt) -> Result<&Task> {
    let task = attempt
        .tasks
        .last()
        .context("interrupted Writer is missing")?;
    ensure!(
        task.kind == TaskKind::Write
            && task.status == TaskStatus::NeedsUser
            && task.output.is_none(),
        "interrupted restoration requires one final output-free interrupted Writer"
    );
    ensure!(
        attempt.tasks[..attempt.tasks.len() - 1]
            .iter()
            .all(|other| matches!(other.status, TaskStatus::Complete | TaskStatus::Superseded)),
        "another unfinished Task prevents interrupted restoration"
    );
    Ok(task)
}

fn inspect(
    project_root: &Path,
    item: &WorkItem,
    attempt_id: &str,
    rejected: &str,
    resolution: &str,
    existing: Option<&RestorationIntent>,
    reference: &ArtifactRef,
) -> Result<RestorationIntent> {
    let attempt = current_attempt(item, attempt_id)?;
    let task = interrupted_task(attempt)?;
    let source = attempt
        .tasks
        .iter()
        .rev()
        .find(|source| source.kind == TaskKind::Write && source.status == TaskStatus::Complete)
        .context("interrupted restoration has no completed source Writer")?;
    let output = source
        .output
        .as_ref()
        .context("completed source Writer has no saved output")?;
    ensure!(
        task.workspace_access.writes.len() == 1
            && task.workspace_access.writes[0].id == output.workspace_id
            && task.workspace_access.writes[0].path == output.workspace_path,
        "interrupted Writer workspace differs from its source output"
    );
    let recovery =
        crate::writer_completion_recovery::validated_writer_context(project_root, item, task)?
            .context("interrupted restoration needs a recorded Writer-completion recovery")?;
    ensure!(
        recovery.source_task_id == source.id && recovery.candidate_commit == output.commit,
        "completion recovery does not bind the latest completed Writer"
    );
    let assignment = task
        .writer_correction_assignment
        .as_ref()
        .context("interrupted Writer has no immutable correction assignment")?;
    // The receipt binds the latest candidate; the assignment retains its
    // original source, including any byte-proven legacy context snapshots.
    crate::recovery_contracts::validate_writer_continuation_lineage(project_root, item, task)?;
    let schema_version = existing.map_or(2, |intent| intent.schema_version);
    ensure!(
        schema_version != 1 || assignment.source_task_id == source.id,
        "legacy restoration intent cannot authorize an inherited assignment"
    );
    let _ = crate::recovery_contracts::pending_writer_completion_rows(project_root, attempt, task)?;
    let workspace = crate::work_model::resolve_managed_sibling_workspace_path(
        project_root,
        &output.workspace_path,
        "interrupted Writer workspace",
    )?;
    super::ensure_registered_worktree(project_root, &workspace)?;
    let common_git_dir = fs::canonicalize(crate::worktree::git_common_dir(&workspace)?)?;
    ensure!(
        common_git_dir == fs::canonicalize(crate::worktree::git_common_dir(project_root)?)?,
        "interrupted Writer belongs to a different Git repository"
    );
    ensure!(
        git::run_stdout(
            &workspace,
            &["status", "--porcelain", "--untracked-files=all"],
            "verify interrupted worktree"
        )?
        .is_empty(),
        "interrupted restoration requires a clean managed worktree"
    );
    let head = git::run_stdout(&workspace, &["rev-parse", "HEAD"], "read interrupted HEAD")?;
    ensure!(
        head == rejected || (existing.is_some() && head == output.commit),
        "interrupted candidate changed from the caller's rejected HEAD"
    );
    ensure!(
        git::run_stdout(
            &workspace,
            &["rev-parse", "--verify", &format!("{rejected}^{{commit}}")],
            "verify rejected commit"
        )? == rejected,
        "rejected candidate must name an exact commit"
    );
    let tree = git::run_stdout(
        &workspace,
        &["rev-parse", &format!("{}^{{tree}}", output.commit)],
        "read saved candidate tree",
    )?;
    ensure!(
        git::run_stdout(
            &workspace,
            &["rev-parse", &format!("{rejected}^{{tree}}")],
            "read rejected candidate tree"
        )? == tree,
        "rejected candidate source tree differs from the recorded candidate"
    );
    let ancestry = git::run_raw(
        &workspace,
        &["merge-base", "--is-ancestor", &output.commit, rejected],
    )?;
    ensure!(
        ancestry.status.code() == Some(1),
        "interrupted restoration requires lost ancestry; append-only candidates are ineligible"
    );
    let protected_base = output
        .base_commit
        .as_deref()
        .context("source Writer has no saved protected base")?;
    git::run(
        &workspace,
        &[
            "merge-base",
            "--is-ancestor",
            protected_base,
            &output.commit,
        ],
        "verify source protected base",
    )?;
    let branch_ref = git::run_stdout(
        &workspace,
        &["symbolic-ref", "-q", "HEAD"],
        "read interrupted branch",
    )?;
    let source_ref = git::run_stdout(
        project_root,
        &["rev-parse", "--symbolic-full-name", &output.source_branch],
        "read recorded source branch",
    )?;
    ensure!(
        source_ref.starts_with("refs/heads/") && source_ref != branch_ref,
        "source output must name a distinct local branch"
    );
    let source_ref_commit = git::run_stdout(
        project_root,
        &["rev-parse", "--verify", &source_ref],
        "read source reference",
    )?;
    for review in attempt
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Review && task.status == TaskStatus::Complete)
    {
        let review_index = attempt
            .tasks
            .iter()
            .position(|task| task.id == review.id)
            .unwrap();
        let reviewed = attempt.tasks[..review_index]
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref())
            .context("review has no saved source Writer")?;
        let context = review
            .review_context
            .as_ref()
            .context("review has no candidate binding")?;
        ensure!(
            context.candidate_commit == reviewed.commit
                && context.candidate_workspace_id == reviewed.workspace_id
                && context.candidate_workspace_path == reviewed.workspace_path
                && context.source_branch == reviewed.source_branch
                && context.base_commit.is_none(),
            "review candidate binding changed"
        );
        let area = review
            .artifact_area
            .as_ref()
            .context("review artifact area is missing")?;
        let bytes = fs::read(project_root.join(&area.path).join("review.md"))?;
        ensure!(
            matches!(
                crate::review::extract_verdict(std::str::from_utf8(&bytes)?),
                crate::review::Verdict::Pass | crate::review::Verdict::Fail
            ),
            "historical review verdict is unreadable or unresolved"
        );
    }
    let execution = task
        .artifact_area
        .as_ref()
        .context("interrupted Writer artifact area is missing")?;
    let record: ExecutionRecord = serde_json::from_slice(&fs::read(
        project_root.join(&execution.path).join("execution.json"),
    )?)?;
    ensure!(
        record.schema_version == 1
            && record.state == ExecutionState::Canceled
            && record.settled_at.is_some()
            && record
                .attempt_owner
                .as_ref()
                .is_some_and(|owner| owner.work_item_id == item.id
                    && owner.attempt_id == attempt_id
                    && fs::canonicalize(&owner.project_root).ok()
                        == fs::canonicalize(project_root).ok()),
        "interrupted Writer execution does not bind the settled cancellation"
    );
    for other in &attempt.tasks {
        if let Some(area) = &other.artifact_area {
            let path = project_root.join(&area.path).join("execution.json");
            match fs::read(&path) {
                Ok(bytes) => {
                    let record: ExecutionRecord = serde_json::from_slice(&bytes)?;
                    ensure!(
                        record.schema_version == 1
                            && matches!(
                                record.state,
                                ExecutionState::Canceled | ExecutionState::Settled
                            )
                            && record.settled_at.is_some(),
                        "Task {} has unsettled execution",
                        other.id
                    );
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let retained_ref = format!(
        "refs/fluent/recovery/{}/{}/interrupted-{}",
        item.id, attempt_id, task.id
    );
    git::run(
        &workspace,
        &["check-ref-format", &retained_ref],
        "validate retained history reference",
    )?;
    let mut normalized = item.clone();
    if let Some(existing) = existing {
        let bytes = serde_json::to_vec_pretty(existing)?;
        let prior = operation(existing, &bytes);
        let attempt = normalized
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .unwrap();
        attempt.artifacts.retain(|artifact| artifact != reference);
        attempt
            .recovery_operations
            .retain(|operation| operation.operation_id != prior.operation_id());
    }
    Ok(RestorationIntent {
        schema_version,
        work_item_id: item.id.clone(),
        attempt_id: attempt_id.into(),
        task_id: task.id.clone(),
        source_task_id: source.id.clone(),
        assignment_source: (schema_version == 2).then(|| AssignmentSource {
            task_id: assignment.source_task_id.clone(),
            candidate_commit: assignment.source_candidate_commit.clone(),
        }),
        recorded_candidate: output.commit.clone(),
        rejected_candidate: rejected.into(),
        candidate_tree: tree,
        protected_base: protected_base.into(),
        workspace_path: output.workspace_path.clone(),
        canonical_workspace: fs::canonicalize(workspace)?,
        common_git_dir,
        branch_ref,
        source_ref,
        source_ref_commit,
        retained_ref,
        model_sha256: content_digest(&serde_json::to_vec(&normalized)?),
        evidence: protected_evidence(project_root, attempt)?,
        resolution: resolution.into(),
    })
}

fn operation(intent: &RestorationIntent, bytes: &[u8]) -> RecoveryOperationRequest {
    RecoveryOperationRequest::new(
        PRODUCER,
        &intent.work_item_id,
        &intent.attempt_id,
        Some(intent.recorded_candidate.clone()),
        Some(intent.task_id.clone()),
        None,
        Some(intent.workspace_path.clone()),
        Some(intent.resolution.clone()),
        Some(format!("sha256:{}", content_digest(bytes))),
    )
}

fn read_intent(project_root: &Path, relative: &str) -> Result<Option<RestorationIntent>> {
    match fs::symlink_metadata(project_root.join(relative)) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && metadata.len() <= super::MAX_RECOVERY_EVIDENCE_BYTES,
                "interrupted restoration intent is not a bounded regular file"
            );
            let path = exact_file(project_root, relative)?;
            let bytes = fs::read(path)?;
            let intent: RestorationIntent =
                serde_json::from_slice(&bytes).context("read interrupted restoration intent")?;
            ensure!(
                matches!(
                    (intent.schema_version, intent.assignment_source.is_some()),
                    (1, false) | (2, true)
                ) && bytes == serde_json::to_vec_pretty(&intent)?,
                "unknown interrupted restoration intent version or changed bytes"
            );
            Ok(Some(intent))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn exact_file(project_root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_)))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "restoration evidence must use canonical relative paths: {relative}"
    );
    let declared = crate::recovery_contracts::resolve_input_artifact_path(project_root, relative)?;
    let canonical = fs::canonicalize(&declared)
        .with_context(|| format!("read restoration evidence {relative}"))?;
    ensure!(
        canonical.starts_with(fs::canonicalize(project_root)?) && canonical.is_file(),
        "restoration evidence must be an exact project file: {relative}"
    );
    Ok(canonical)
}

fn protected_evidence(
    project_root: &Path,
    attempt: &Attempt,
) -> Result<BTreeMap<String, FileBinding>> {
    let mut paths = BTreeSet::new();
    for reference in &attempt.artifacts {
        if reference.producer_id != PRODUCER && reference.path.starts_with(".fluent/work/") {
            paths.insert(reference.path.clone());
        }
    }
    for task in &attempt.tasks {
        paths.extend(task.input_artifacts.iter().map(|input| input.path.clone()));
        if let Some(assignment) = &task.writer_correction_assignment {
            paths.extend(
                assignment
                    .source_task_inputs
                    .iter()
                    .chain(&assignment.evidence)
                    .map(|input| input.artifact.path.clone()),
            );
        }
        if let Some(area) = &task.artifact_area {
            for name in [
                "review.md",
                "writer-completion.json",
                "candidate-gate.json",
                "execution.json",
            ] {
                let relative = format!("{}/{name}", area.path);
                match fs::symlink_metadata(project_root.join(&relative)) {
                    Ok(_) => {
                        paths.insert(relative);
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    paths.insert(format!(
        ".fluent/work/progress/{}/{}/progress.md",
        attempt.work_item_id, attempt.id
    ));
    paths
        .into_iter()
        .map(|path| {
            let canonical_path = exact_file(project_root, &path)?;
            let sha256 = content_digest(&fs::read(&canonical_path)?);
            Ok((
                path,
                FileBinding {
                    canonical_path,
                    sha256,
                },
            ))
        })
        .collect()
}

fn checked_refs(intent: &RestorationIntent, head: &str, target: Option<&str>) -> Result<()> {
    let mut transaction = String::from("start\n");
    let mut direct_refs = vec![intent.branch_ref.as_str()];
    if intent.source_ref != intent.branch_ref {
        transaction.push_str(&format!(
            "option no-deref\nverify {} {}\n",
            intent.source_ref, intent.source_ref_commit
        ));
        direct_refs.push(&intent.source_ref);
    }
    if let Some(target) = target {
        transaction.push_str(&format!(
            "option no-deref\nverify {} {}\noption no-deref\nupdate {} {target} {head}\n",
            intent.retained_ref, intent.rejected_candidate, intent.branch_ref
        ));
        direct_refs.push(&intent.retained_ref);
    } else {
        transaction.push_str(&format!(
            "option no-deref\nverify {} {head}\n",
            intent.branch_ref
        ));
    }
    transaction.push_str("prepare\n");
    let common_git_dir = intent
        .common_git_dir
        .to_str()
        .context("Git directory is not UTF-8")?;
    let prepared = PreparedRefs::prepare(
        &intent.canonical_workspace,
        &[
            "--git-dir",
            common_git_dir,
            "-c",
            "core.fsync=reference",
            "update-ref",
            "--stdin",
        ],
        &transaction,
    )?;
    // no-deref prevents updating a substituted referent, but permits replacing
    // the symbolic ref itself. Check ref types while prepare holds their locks.
    for reference in direct_refs {
        let output = git::run_raw(
            &intent.canonical_workspace,
            &[
                "--git-dir",
                common_git_dir,
                "symbolic-ref",
                "--quiet",
                "--no-recurse",
                reference,
            ],
        )?;
        ensure!(
            output.status.code() == Some(1),
            "interrupted restoration reference must remain direct: {reference}"
        );
    }
    if target.is_some() {
        prepared.commit()?;
    }
    Ok(())
}

/// Keep Git's prepared reference locks until commit or drop. EOF aborts an
/// uncommitted transaction, including when the coordinator exits unexpectedly.
struct PreparedRefs {
    child: Child,
    output: Option<BufReader<ChildStdout>>,
}

impl PreparedRefs {
    fn hold_head(intent: &RestorationIntent) -> Result<Self> {
        Self::prepare(
            &intent.canonical_workspace,
            &["update-ref", "--stdin"],
            &format!(
                "start\noption no-deref\nsymref-verify HEAD {}\nprepare\n",
                intent.branch_ref
            ),
        )
    }

    fn prepare(workspace: &Path, args: &[&str], transaction: &str) -> Result<Self> {
        let mut child = git::build_command(workspace, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start Git reference verification")?;
        let output = child.stdout.take().map(BufReader::new);
        let mut guard = Self { child, output };
        guard
            .child
            .stdin
            .as_mut()
            .context("open Git transaction input")?
            .write_all(transaction.as_bytes())?;
        for expected in ["start: ok", "prepare: ok"] {
            guard.acknowledge(expected)?;
        }
        Ok(guard)
    }

    fn acknowledge(&mut self, expected: &str) -> Result<()> {
        let mut line = String::new();
        self.output
            .as_mut()
            .context("open Git transaction output")?
            .read_line(&mut line)?;
        ensure!(
            line.trim() == expected,
            "Git reference transaction did not acknowledge {expected} (requires symref-verify support)"
        );
        Ok(())
    }

    fn commit(mut self) -> Result<()> {
        self.child
            .stdin
            .as_mut()
            .context("open Git transaction input")?
            .write_all(b"commit\n")?;
        self.acknowledge("commit: ok")?;
        self.child.stdin.take();
        ensure!(self.child.wait()?.success(), "Git reference commit failed");
        Ok(())
    }
}

impl Drop for PreparedRefs {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = self.child.wait();
    }
}

fn retain_rejected_ref(intent: &RestorationIntent) -> Result<()> {
    let refs = git::run_stdout(
        &intent.canonical_workspace,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
            &intent.retained_ref,
        ],
        "inspect retained history",
    )?;
    if !refs.is_empty() {
        ensure!(
            refs.trim() == format!("{} {}", intent.retained_ref, intent.rejected_candidate),
            "retained rejected history reference changed"
        );
        return Ok(());
    }
    git::run_with_stdin(
        &intent.canonical_workspace,
        &["-c", "core.fsync=reference", "update-ref", "--stdin"],
        format!(
            "start\noption no-deref\ncreate {} {}\nprepare\ncommit\n",
            intent.retained_ref, intent.rejected_candidate
        )
        .as_bytes(),
        "retain the exact rejected Writer history",
    )
}
