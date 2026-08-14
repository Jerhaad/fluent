//! Local scheduler that dispatches execution-ready queued Work through the
//! durable dispatch ledger.
//!
//! The scheduler claims a queued dispatch, durably binds exactly one Attempt to
//! it before launching, transitions it to `running`, drives the Attempt to a
//! terminal outcome, and reconciles the dispatch. It never lands a Merge
//! Candidate: a passing Attempt reaches `candidate-ready` and its Merge
//! Candidate stays pending for an explicit land or the separately authorized
//! auto-merge policy.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};

use crate::lease::{self, TaskLease};
use crate::queue::{self, Dispatch, DispatchStatus, DispatchToken};
use crate::scheduler_service::BuildIdentity;
use crate::work_model::{AttemptStatus, WorkModelStore};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
const COORDINATOR_METADATA_SCHEMA: u32 = 1;
const STOP_REQUEST_SCHEMA: u32 = 1;

/// The terminal outcome of running a bound Attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    CandidateReady,
    Failed,
    NeedsUser,
    Blocked,
}

impl AttemptOutcome {
    fn to_dispatch_status(self) -> DispatchStatus {
        match self {
            Self::CandidateReady => DispatchStatus::CandidateReady,
            Self::Failed => DispatchStatus::Failed,
            Self::NeedsUser => DispatchStatus::NeedsUser,
            Self::Blocked => DispatchStatus::Blocked,
        }
    }
}

/// Runs a bound Attempt to a terminal outcome. The scheduler owns claiming,
/// binding, launching, and reconciling; a runner only advances the Attempt.
pub trait AttemptRunner: Send + Sync {
    fn run(
        &self,
        project_root: &Path,
        work_item_id: &str,
        attempt_id: &str,
        tester_no_sandbox: bool,
    ) -> Result<AttemptOutcome>;
}

/// Production runner that drives an Attempt by invoking `fluent attempt run`.
pub struct CliAttemptRunner;

impl AttemptRunner for CliAttemptRunner {
    fn run(
        &self,
        project_root: &Path,
        work_item_id: &str,
        attempt_id: &str,
        tester_no_sandbox: bool,
    ) -> Result<AttemptOutcome> {
        let fluent_bin = std::env::current_exe()?;
        if tester_no_sandbox {
            eprintln!(
                "[scheduler] approving one unsandboxed retry of the paused Tester for {attempt_id}"
            );
        }
        let _run_status = attempt_run_command(
            &fluent_bin,
            project_root,
            work_item_id,
            attempt_id,
            scheduler_baseline_session(),
            tester_no_sandbox,
        )
        .status()?;

        // Durable Attempt state is authoritative even when the child exits
        // non-zero. In particular, a resume can fail closed while preserving
        // `needs-user`; recording that dispatch as `failed` would lose the
        // scheduler's recoverable disposition.
        classify_attempt_outcome(project_root, work_item_id, attempt_id)
    }
}

fn scheduler_baseline_session() -> &'static str {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{nanos}", std::process::id())
    })
}

fn attempt_run_command(
    fluent_bin: &Path,
    project_root: &Path,
    work_item_id: &str,
    attempt_id: &str,
    baseline_session: &str,
    tester_no_sandbox: bool,
) -> Command {
    let mut command = Command::new(fluent_bin);
    command
        .current_dir(project_root)
        .args(["attempt", "run"])
        .env(
            crate::baseline_cache::BASELINE_SESSION_ENV,
            baseline_session,
        );
    if tester_no_sandbox {
        command.arg("--tester-no-sandbox");
    }
    command.args([work_item_id, attempt_id]);
    command
}

/// Classify a bound Attempt's terminal outcome from its persisted lifecycle.
/// Completion alone is insufficient: `candidate-ready` means the pending Merge
/// Candidate can pass the same Learner advancement gate used by landing.
pub fn classify_attempt_outcome(
    project_root: &Path,
    work_item_id: &str,
    attempt_id: &str,
) -> Result<AttemptOutcome> {
    let store = WorkModelStore::new(project_root);
    let item = store.read_work_item(work_item_id)?;
    Ok(classify_persisted_attempt(&item, attempt_id))
}

fn classify_persisted_attempt(
    item: &crate::work_model::WorkItem,
    attempt_id: &str,
) -> AttemptOutcome {
    let Some(attempt) = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
    else {
        return AttemptOutcome::Failed;
    };
    match attempt.status {
        AttemptStatus::Failed => AttemptOutcome::Failed,
        AttemptStatus::NeedsUser => AttemptOutcome::NeedsUser,
        AttemptStatus::Complete => item
            .merge_candidates
            .iter()
            .find(|candidate| candidate.attempt_id == attempt_id)
            .filter(|candidate| candidate.validate_advancement(item).is_ok())
            .map(|_| AttemptOutcome::CandidateReady)
            .unwrap_or(AttemptOutcome::Blocked),
        AttemptStatus::Planned | AttemptStatus::Executing | AttemptStatus::Reviewing => {
            AttemptOutcome::Blocked
        }
    }
}

/// Elect one coordinator per project and, when elected, run its capacity-limited
/// worker pool. A start that finds another live coordinator reports reuse and
/// returns successfully without claiming Work.
pub fn run(project_root: &Path, poll_seconds: u64, runner: &dyn AttemptRunner) -> Result<()> {
    SHUTDOWN.store(false, Ordering::Release);
    install_signal_handler();
    let executable = std::env::current_exe().context("resolve current Fluent executable")?;
    let build = crate::scheduler_service::executable_build_identity(&executable)?;

    match start_or_reuse(project_root, &build)? {
        CoordinatorStart::Reused(metadata) => {
            eprintln!(
                "[scheduler] reusing live coordinator {} running {} ({})",
                metadata.instance_id, metadata.build.version, metadata.build.hash
            );
            Ok(())
        }
        CoordinatorStart::Elected(mut lease) => {
            // A newly elected coordinator must prove the host can apply Fluent's
            // worker boundary before it prepares baselines, polls, or claims Work.
            // A caller that merely reuses an existing coordinator performs no new
            // execution and therefore does not need this process-local probe.
            let resolver = crate::content::ContentResolver::new(Some(project_root));
            crate::os::preflight_host_execution(project_root, &resolver)?;
            let capacity = resolve_capacity(project_root)?;
            let baseline_session = scheduler_baseline_session();
            crate::baseline_cache::prepare_scheduler_session(project_root, baseline_session)?;
            let result = run_coordinator_controlled(
                project_root,
                poll_seconds,
                capacity,
                runner,
                &SHUTDOWN,
                Some(&mut lease),
            );
            let cleanup =
                crate::baseline_cache::finish_scheduler_session(project_root, baseline_session);
            drop(lease);
            result.and(cleanup)
        }
    }
}

/// Run the elected coordinator's worker pool: each poll fills free capacity with
/// newly claimed dispatches, launching each on its own worker thread. Running
/// Work is never interrupted; once `shutdown` is set the coordinator stops
/// claiming, and the scope drains live workers before returning.
#[cfg(test)]
fn run_coordinator(
    project_root: &Path,
    poll_seconds: u64,
    capacity: usize,
    runner: &dyn AttemptRunner,
    shutdown: &AtomicBool,
) -> Result<()> {
    run_coordinator_controlled(project_root, poll_seconds, capacity, runner, shutdown, None)
}

fn run_coordinator_controlled(
    project_root: &Path,
    poll_seconds: u64,
    capacity: usize,
    runner: &dyn AttemptRunner,
    shutdown: &AtomicBool,
    mut coordinator: Option<&mut CoordinatorLease>,
) -> Result<()> {
    std::thread::scope(|scope| -> Result<()> {
        let mut workers: Vec<std::thread::ScopedJoinHandle<()>> = Vec::new();
        loop {
            workers.retain(|handle| !handle.is_finished());

            if shutdown_requested(project_root, shutdown, coordinator.as_deref_mut())? {
                break;
            }

            recover_and_reconcile(project_root)?;

            let mut spawned = false;
            for (token, lease) in claim_available(project_root, capacity)? {
                let handle = scope.spawn(move || {
                    if let Err(error) = run_claimed(project_root, token, lease, runner) {
                        eprintln!("[scheduler] worker error: {error}");
                    }
                });
                workers.push(handle);
                spawned = true;
            }

            if shutdown_requested(project_root, shutdown, coordinator.as_deref_mut())? {
                break;
            }

            if spawned {
                // Re-evaluate promptly so freed capacity refills without waiting
                // a full poll interval.
                sleep_until_shutdown(
                    project_root,
                    Duration::from_millis(50),
                    shutdown,
                    coordinator.as_deref_mut(),
                )?;
            } else {
                sleep_until_shutdown(
                    project_root,
                    Duration::from_secs(poll_seconds),
                    shutdown,
                    coordinator.as_deref_mut(),
                )?;
            }
        }
        // The scope join drains any live workers, recording their outcomes.
        Ok(())
    })?;
    Ok(())
}

/// Whether queued Work exists but no live coordinator can claim it, so
/// execution is stopped.
pub fn execution_is_stopped(project_root: &Path) -> Result<bool> {
    if coordinator_is_live(project_root) {
        return Ok(false);
    }
    Ok(queue::list(project_root)?
        .iter()
        .any(|dispatch| dispatch.status == DispatchStatus::Queued))
}

/// Whether a live coordinator currently owns this project.
pub fn coordinator_is_live(project_root: &Path) -> bool {
    lease::is_leased(&coordinator_lock_path(project_root))
}

fn resolve_capacity(project_root: &Path) -> Result<usize> {
    let config = crate::config::resolve_scheduler_config(project_root)?;
    Ok(config.max_local_concurrency.value as usize)
}

/// The highest-priority queued dispatch, breaking ties by oldest queue time.
pub fn pick_next_queued(project_root: &Path) -> Result<Option<Dispatch>> {
    Ok(queue::list(project_root)?
        .into_iter()
        .find(|dispatch| dispatch.status == DispatchStatus::Queued))
}

/// Claim queued dispatches up to the project's remaining local capacity,
/// binding one Attempt and holding a whole-Attempt lease for each. Each returned
/// claim reads as live, so capacity is never exceeded across polls.
pub fn claim_available(
    project_root: &Path,
    capacity: usize,
) -> Result<Vec<(DispatchToken, TaskLease)>> {
    let mut claims = Vec::new();
    while project_local_capacity_used(project_root)? < capacity {
        match claim_next(project_root)? {
            Some(claim) => claims.push(claim),
            None => break,
        }
    }
    Ok(claims)
}

/// Claim the next eligible queued dispatch, binding an Attempt id and acquiring
/// its whole-Attempt lease before returning.
fn claim_next(project_root: &Path) -> Result<Option<(DispatchToken, TaskLease)>> {
    let Some(dispatch) = pick_next_queued(project_root)? else {
        return Ok(None);
    };
    let bound_attempt_id = resolve_bound_attempt_id(project_root, &dispatch.work_item_id)?;
    let Some(token) = queue::claim(project_root, &dispatch.work_item_id, &bound_attempt_id)? else {
        return Ok(None);
    };
    let lease = acquire_dispatch_lease(project_root, &dispatch.work_item_id)?;
    Ok(Some((token, lease)))
}

/// Count the project's local scheduler concurrency: every Work Item whose active
/// dispatch is claimed or running with a still-live bound-Attempt lease. Direct
/// and Fargate Attempts have no dispatch and never count; a Work Item with
/// nested reviewers is one dispatch and therefore one slot.
pub fn project_local_capacity_used(project_root: &Path) -> Result<usize> {
    let mut used = 0;
    for ledger in queue::list_ledgers(project_root)? {
        let Some(active) = ledger.active() else {
            continue;
        };
        if matches!(
            active.status,
            DispatchStatus::Claimed | DispatchStatus::Running
        ) && lease::is_leased(&queue::dispatch_lease_path(
            project_root,
            &ledger.work_item_id,
        )) {
            used += 1;
        }
    }
    Ok(used)
}

// -------------------------------------------------------------------------
// Recovery and reconciliation
// -------------------------------------------------------------------------

/// Reconcile the queue against durable Work model state before claiming fresh
/// Work. Recovers stale claims, cancels abandoned queued Work, blocks invalid
/// references, and reflects human-resumed suspensions. One bad entry never
/// stalls the others.
pub fn recover_and_reconcile(project_root: &Path) -> Result<()> {
    for ledger in queue::list_ledgers(project_root)? {
        let work_item_id = ledger.work_item_id.clone();
        let Some(dispatch) = ledger.latest() else {
            continue;
        };
        let result = match dispatch.status {
            DispatchStatus::Queued => reconcile_queued(project_root, &work_item_id),
            DispatchStatus::Claimed | DispatchStatus::Running => reconcile_claim(
                project_root,
                &work_item_id,
                dispatch.bound_attempt_id.as_deref(),
            ),
            DispatchStatus::NeedsUser => reconcile_suspension(
                project_root,
                &work_item_id,
                dispatch.bound_attempt_id.as_deref(),
            ),
            _ => Ok(()),
        };
        if let Err(error) = result {
            eprintln!("[scheduler] recovery skipped {work_item_id}: {error}");
        }
    }
    Ok(())
}

/// A queued dispatch that has since become abandoned is canceled; one that
/// references missing, malformed, or non-executable Work is blocked. Both leave
/// other eligible entries untouched.
fn reconcile_queued(project_root: &Path, work_item_id: &str) -> Result<()> {
    let store = WorkModelStore::new(project_root);
    match store.read_work_item(work_item_id) {
        Ok(item) => {
            if item.abandonment.is_some() {
                queue::cancel_active(project_root, work_item_id)?;
            } else if item.authorization.is_proposed() {
                queue::block_active(
                    project_root,
                    work_item_id,
                    "Work Item is not execution-ready",
                )?;
            }
            Ok(())
        }
        Err(error) if is_missing(&error) => {
            queue::block_active(project_root, work_item_id, "Work Item not found")?;
            Ok(())
        }
        Err(_) => {
            queue::block_active(project_root, work_item_id, "Work Item could not be read")?;
            Ok(())
        }
    }
}

/// A claim whose bound-Attempt lease is dead is recovered: an unbound or
/// never-created binding returns to `queued` for a fresh single Attempt, a
/// terminal Attempt reconciles the dispatch from its outcome, and a nonterminal
/// Attempt returns to `queued` with its binding preserved so the same Attempt
/// resumes.
fn reconcile_claim(
    project_root: &Path,
    work_item_id: &str,
    bound_attempt_id: Option<&str>,
) -> Result<()> {
    if lease::is_leased(&queue::dispatch_lease_path(project_root, work_item_id)) {
        return Ok(());
    }

    match bound_attempt_terminal_status(project_root, work_item_id, bound_attempt_id)? {
        BoundAttemptState::Unbound => {
            queue::requeue_active(project_root, work_item_id, true)?;
        }
        BoundAttemptState::Nonterminal => {
            queue::requeue_active(project_root, work_item_id, false)?;
        }
        BoundAttemptState::Terminal(status) => {
            queue::reconcile_active(project_root, work_item_id, status)?;
        }
    }
    Ok(())
}

/// A needs-user dispatch whose bound Attempt has been resumed to a new terminal
/// outcome reconciles to that outcome; one still suspended is left untouched.
fn reconcile_suspension(
    project_root: &Path,
    work_item_id: &str,
    bound_attempt_id: Option<&str>,
) -> Result<()> {
    if let BoundAttemptState::Terminal(status) =
        bound_attempt_terminal_status(project_root, work_item_id, bound_attempt_id)?
    {
        if status != DispatchStatus::NeedsUser {
            queue::reconcile_active(project_root, work_item_id, status)?;
        }
    }
    Ok(())
}

enum BoundAttemptState {
    Unbound,
    Nonterminal,
    Terminal(DispatchStatus),
}

fn bound_attempt_terminal_status(
    project_root: &Path,
    work_item_id: &str,
    bound_attempt_id: Option<&str>,
) -> Result<BoundAttemptState> {
    let Some(attempt_id) = bound_attempt_id else {
        return Ok(BoundAttemptState::Unbound);
    };
    let store = WorkModelStore::new(project_root);
    let item = store.read_work_item(work_item_id)?;
    let Some(attempt) = item.attempts.iter().find(|a| a.id == attempt_id) else {
        // The binding was recorded but the Attempt was never created.
        return Ok(BoundAttemptState::Unbound);
    };
    Ok(match attempt.status {
        AttemptStatus::Complete => BoundAttemptState::Terminal(
            classify_persisted_attempt(&item, attempt_id).to_dispatch_status(),
        ),
        AttemptStatus::Failed => BoundAttemptState::Terminal(DispatchStatus::Failed),
        AttemptStatus::NeedsUser => BoundAttemptState::Terminal(DispatchStatus::NeedsUser),
        _ => BoundAttemptState::Nonterminal,
    })
}

fn is_missing(error: &crate::work_model::WorkModelStorageError) -> bool {
    matches!(
        error,
        crate::work_model::WorkModelStorageError::ReadFile { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound
    )
}

/// Claim a queued dispatch, durably bind exactly one Attempt, launch it, and
/// reconcile the dispatch from the Attempt's outcome. Never invokes merge
/// logic.
pub fn dispatch_one(
    project_root: &Path,
    dispatch: &Dispatch,
    runner: &dyn AttemptRunner,
) -> Result<()> {
    let work_item_id = &dispatch.work_item_id;

    // Decide the bound Attempt id before claiming so the ledger records the
    // exact Attempt the claim owns, then ensure that Attempt under the Work
    // model. A crash between the two reconciles by the persisted binding.
    let bound_attempt_id = resolve_bound_attempt_id(project_root, work_item_id)?;

    let Some(token) = queue::claim(project_root, work_item_id, &bound_attempt_id)? else {
        // Another worker claimed it first; nothing to do.
        return Ok(());
    };

    // Hold the whole-Attempt lease for the life of this dispatch so the claim
    // reads as live to capacity counting and duplicate-dispatch prevention.
    let lease = acquire_dispatch_lease(project_root, work_item_id)?;
    run_claimed(project_root, token, lease, runner)
}

/// Run an already-claimed dispatch on the current thread: ensure its bound
/// Attempt, transition to running, drive it, and reconcile. Holds the passed
/// lease for the full lifetime of the dispatch.
fn run_claimed(
    project_root: &Path,
    token: DispatchToken,
    lease: TaskLease,
    runner: &dyn AttemptRunner,
) -> Result<()> {
    let _lease = lease;
    let work_item_id = token.work_item_id.clone();

    ensure_bound_attempt(project_root, &work_item_id, &token.bound_attempt_id)?;

    let token = queue::mark_running(project_root, &token)?;
    eprintln!("[scheduler] starting {work_item_id}");

    let outcome = runner.run(
        project_root,
        &work_item_id,
        &token.bound_attempt_id,
        token.tester_no_sandbox,
    )?;

    let status = outcome.to_dispatch_status();
    queue::reconcile(project_root, &token, status)?;
    eprintln!("[scheduler] finished {work_item_id} -> {status}");
    Ok(())
}

// -------------------------------------------------------------------------
// Coordinator election
// -------------------------------------------------------------------------

/// The outcome of a scheduler start: this process became the coordinator, or a
/// live coordinator already owns the project.
pub enum CoordinatorStart {
    Elected(CoordinatorLease),
    Reused(CoordinatorMetadata),
}

/// The lifetime lease of an elected coordinator. Held for the coordinator's run
/// and released when it exits, so the next start can be elected.
pub struct CoordinatorLease {
    file: File,
    project_root: PathBuf,
    metadata: CoordinatorMetadata,
}

/// Durable identity and lifecycle state for one elected coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorMetadata {
    pub schema: u32,
    pub instance_id: String,
    /// Display-only process identifier. Fluent never signals this PID.
    pub pid: u32,
    pub started_at: String,
    pub state: CoordinatorState,
    pub build: BuildIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinatorState {
    Running,
    Draining,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    schema: u32,
    instance_id: String,
    requested_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerStatus {
    Stopped,
    Running(CoordinatorMetadata),
}

fn coordinator_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/coordinator.lock")
}

fn coordinator_metadata_path(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/coordinator.json")
}

fn stop_request_path(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/stop-request.json")
}

/// Try to become the project's coordinator. Elects this process when the
/// coordinator lease is free; otherwise reports reuse of the live coordinator.
pub fn start_or_reuse(project_root: &Path, build: &BuildIdentity) -> Result<CoordinatorStart> {
    let path = coordinator_lock_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    loop {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                let metadata = CoordinatorMetadata {
                    schema: COORDINATOR_METADATA_SCHEMA,
                    instance_id: crate::scheduler_service::generate_random_hex(16)?,
                    pid: std::process::id(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    state: CoordinatorState::Running,
                    build: build.clone(),
                };
                publish_lock_identity(&mut file, &metadata.instance_id)?;
                write_coordinator_metadata(project_root, &metadata)?;
                return Ok(CoordinatorStart::Elected(CoordinatorLease {
                    file,
                    project_root: project_root.to_path_buf(),
                    metadata,
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let Some(metadata) = read_live_coordinator_metadata(project_root)? else {
                    // The owner exited after this start lost election. Retry the
                    // lease instead of misclassifying its cleanup gap as legacy.
                    continue;
                };
                if metadata.state == CoordinatorState::Draining {
                    bail!(
                        "scheduler coordinator {} is draining; wait for `fluent scheduler stop` to finish, then run `fluent scheduler run`",
                        metadata.instance_id
                    );
                }
                if metadata.build != *build {
                    bail!(
                        "a live scheduler coordinator is running {} ({}), but this Fluent build is {} ({}); run `fluent scheduler stop`, then `fluent scheduler run`",
                        metadata.build.version,
                        metadata.build.hash,
                        build.version,
                        build.hash
                    );
                }
                return Ok(CoordinatorStart::Reused(metadata));
            }
            Err(error) => return Err(error).context("elect scheduler coordinator"),
        }
    }
}

impl CoordinatorLease {
    fn mark_draining(&mut self) -> Result<()> {
        if self.metadata.state == CoordinatorState::Draining {
            return Ok(());
        }
        self.metadata.state = CoordinatorState::Draining;
        write_coordinator_metadata(&self.project_root, &self.metadata)
    }
}

impl Drop for CoordinatorLease {
    fn drop(&mut self) {
        remove_if_instance_matches(
            &coordinator_metadata_path(&self.project_root),
            &self.metadata.instance_id,
        );
        remove_if_instance_matches(
            &stop_request_path(&self.project_root),
            &self.metadata.instance_id,
        );
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

fn write_coordinator_metadata(project_root: &Path, metadata: &CoordinatorMetadata) -> Result<()> {
    let path = coordinator_metadata_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_write::atomic_write(&path, &serde_json::to_vec_pretty(metadata)?)
        .with_context(|| format!("write scheduler coordinator metadata {}", path.display()))
}

fn publish_lock_identity(file: &mut File, instance_id: &str) -> Result<()> {
    file.set_len(0)
        .context("clear scheduler coordinator identity")?;
    file.seek(SeekFrom::Start(0))
        .context("seek scheduler coordinator identity")?;
    file.write_all(instance_id.as_bytes())
        .context("write scheduler coordinator identity")?;
    file.sync_data()
        .context("sync scheduler coordinator identity")
}

fn read_lock_identity(project_root: &Path) -> Result<Option<String>> {
    let path = coordinator_lock_path(project_root);
    let value = fs::read_to_string(&path)
        .with_context(|| format!("read scheduler coordinator identity {}", path.display()))?;
    let value = value.trim();
    if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(Some(value.to_string()))
    } else {
        Ok(None)
    }
}

fn read_coordinator_metadata(project_root: &Path) -> Result<Option<CoordinatorMetadata>> {
    let path = coordinator_metadata_path(project_root);
    match fs::read(&path) {
        Ok(bytes) => {
            let metadata: CoordinatorMetadata =
                serde_json::from_slice(&bytes).with_context(|| {
                    format!("parse scheduler coordinator metadata {}", path.display())
                })?;
            if metadata.schema != COORDINATOR_METADATA_SCHEMA || metadata.instance_id.is_empty() {
                bail!(
                    "unsupported scheduler coordinator metadata in {}",
                    path.display()
                );
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("read scheduler coordinator metadata {}", path.display())),
    }
}

fn read_live_coordinator_metadata(project_root: &Path) -> Result<Option<CoordinatorMetadata>> {
    // Election acquires the lease before it can publish metadata. Give that
    // bounded startup window time to complete, then treat absence as a legacy
    // or malformed live coordinator and fail closed.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let problem = match (
            read_lock_identity(project_root),
            read_coordinator_metadata(project_root),
        ) {
            (Ok(Some(lock_id)), Ok(Some(metadata))) if metadata.instance_id == lock_id => {
                return Ok(Some(metadata));
            }
            (Ok(Some(lock_id)), Ok(Some(metadata))) => format!(
                "metadata instance {} does not match lease instance {lock_id}",
                metadata.instance_id
            ),
            (Err(error), _) | (_, Err(error)) => format!("{error:#}"),
            _ => "identity metadata is missing".to_string(),
        };
        if !coordinator_is_live(project_root) {
            return Ok(None);
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "a live scheduler coordinator has no valid identity metadata ({problem}); stop its owning process manually before starting another coordinator",
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_stop_request(project_root: &Path) -> Result<Option<StopRequest>> {
    let path = stop_request_path(project_root);
    match fs::read(&path) {
        Ok(bytes) => {
            let request: StopRequest = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse scheduler stop request {}", path.display()))?;
            if request.schema != STOP_REQUEST_SCHEMA || request.instance_id.is_empty() {
                bail!("unsupported scheduler stop request in {}", path.display());
            }
            Ok(Some(request))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read scheduler stop request {}", path.display()))
        }
    }
}

fn remove_if_instance_matches(path: &Path, instance_id: &str) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let matches = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("instance_id")?.as_str().map(str::to_owned))
        .is_some_and(|stored| stored == instance_id);
    if matches {
        let _ = fs::remove_file(path);
    }
}

/// Read the current coordinator without changing scheduler state.
pub fn status(project_root: &Path) -> Result<SchedulerStatus> {
    if !coordinator_is_live(project_root) {
        return Ok(SchedulerStatus::Stopped);
    }
    match read_live_coordinator_metadata(project_root)? {
        Some(metadata) => Ok(SchedulerStatus::Running(metadata)),
        None => Ok(SchedulerStatus::Stopped),
    }
}

/// Ask the exact live coordinator to stop claiming, drain workers, and exit.
pub fn stop(project_root: &Path) -> Result<CoordinatorMetadata> {
    let target = match status(project_root)? {
        SchedulerStatus::Stopped => bail!("scheduler coordinator is not running"),
        SchedulerStatus::Running(metadata) => metadata,
    };
    let request = StopRequest {
        schema: STOP_REQUEST_SCHEMA,
        instance_id: target.instance_id.clone(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    let path = stop_request_path(project_root);
    crate::atomic_write::atomic_write(&path, &serde_json::to_vec_pretty(&request)?)
        .with_context(|| format!("write scheduler stop request {}", path.display()))?;

    // Revalidate after publication. A replacement instance ignores this request.
    match status(project_root)? {
        SchedulerStatus::Stopped => {
            remove_if_instance_matches(&path, &target.instance_id);
            return Ok(target);
        }
        SchedulerStatus::Running(current) if current.instance_id != target.instance_id => {
            remove_if_instance_matches(&path, &target.instance_id);
            bail!(
                "scheduler coordinator changed while requesting stop; instance {} is now running",
                current.instance_id
            );
        }
        SchedulerStatus::Running(_) => {}
    }

    loop {
        if !coordinator_is_live(project_root) {
            remove_if_instance_matches(&path, &target.instance_id);
            return Ok(target);
        }
        match read_coordinator_metadata(project_root) {
            Ok(Some(current)) if current.instance_id == target.instance_id => {}
            Ok(Some(current)) => {
                remove_if_instance_matches(&path, &target.instance_id);
                bail!(
                    "scheduler coordinator changed while draining; instance {} is now running",
                    current.instance_id
                )
            }
            // The elected process removes metadata immediately before releasing
            // its lease. Let that short cleanup window finish.
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The Attempt id a dispatch will bind. A queued dispatch that still carries a
/// binding — a stale claim returned to the queue for resume — reuses that exact
/// Attempt; otherwise a fresh id is allocated so a requeue after a terminal
/// Attempt opens a new one.
fn resolve_bound_attempt_id(project_root: &Path, work_item_id: &str) -> Result<String> {
    if let Some(ledger) = queue::read_ledger(project_root, work_item_id)? {
        if let Some(active) = ledger.active() {
            if let Some(bound) = &active.bound_attempt_id {
                return Ok(bound.clone());
            }
        }
    }
    let store = WorkModelStore::new(project_root);
    let item = store.read_work_item(work_item_id)?;
    if let Some(attempt) = item
        .attempts
        .last()
        .filter(|attempt| attempt.status == AttemptStatus::Planned)
    {
        return Ok(attempt.id.clone());
    }
    Ok(item.next_attempt_id())
}

/// Ensure exactly the bound Attempt exists, creating it once when absent.
fn ensure_bound_attempt(project_root: &Path, work_item_id: &str, attempt_id: &str) -> Result<()> {
    let store = WorkModelStore::new(project_root);
    let mut item = store.read_work_item(work_item_id)?;
    if item.attempts.iter().any(|a| a.id == attempt_id) {
        return Ok(());
    }
    let coder_mapping = crate::work_model::resolve_coder_mapping(
        &crate::config::attempt_creation_inputs(project_root)?,
    )?;
    item.add_initial_attempt_with_coder_mapping(attempt_id, coder_mapping)?;
    store.write_work_item(&item)?;
    Ok(())
}

fn acquire_dispatch_lease(project_root: &Path, work_item_id: &str) -> Result<TaskLease> {
    let path = queue::dispatch_lease_path(project_root, work_item_id);
    lease::acquire(&path).map_err(|source| {
        anyhow::anyhow!("could not acquire dispatch lease for {work_item_id:?}: {source}")
    })
}

fn install_signal_handler() {
    let _ = ctrlc::set_handler(|| {
        SHUTDOWN.store(true, Ordering::Release);
    });
}

fn is_shutdown(shutdown: &AtomicBool) -> bool {
    shutdown.load(Ordering::Acquire)
}

fn shutdown_requested(
    project_root: &Path,
    shutdown: &AtomicBool,
    coordinator: Option<&mut CoordinatorLease>,
) -> Result<bool> {
    if is_shutdown(shutdown) {
        if let Some(coordinator) = coordinator {
            coordinator.mark_draining()?;
        }
        return Ok(true);
    }
    let Some(coordinator) = coordinator else {
        return Ok(false);
    };
    match read_stop_request(project_root) {
        Ok(Some(request)) if request.instance_id == coordinator.metadata.instance_id => {
            coordinator.mark_draining()?;
            Ok(true)
        }
        Ok(Some(request)) => {
            remove_if_instance_matches(&stop_request_path(project_root), &request.instance_id);
            Ok(false)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            eprintln!("[scheduler] ignoring invalid stop request: {error:#}");
            Ok(false)
        }
    }
}

/// Sleep up to `duration`, checking the shutdown signal each second so a
/// SIGTERM or SIGINT stops an idle coordinator without waiting the full poll
/// interval.
fn sleep_until_shutdown(
    project_root: &Path,
    duration: Duration,
    shutdown: &AtomicBool,
    mut coordinator: Option<&mut CoordinatorLease>,
) -> Result<()> {
    let one_second = Duration::from_secs(1);
    let mut remaining = duration;
    while remaining > Duration::ZERO {
        if shutdown_requested(project_root, shutdown, coordinator.as_deref_mut())? {
            return Ok(());
        }
        let sleep_for = remaining.min(one_second);
        std::thread::sleep(sleep_for);
        remaining = remaining.saturating_sub(sleep_for);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::work_model::{
        AttemptReviewState, TaskOutput, TaskStatus, WorkspaceAccess, WorkspaceRef,
    };
    use std::fs;
    use std::sync::Mutex;

    /// A runner that returns a canned outcome and records its invocations.
    #[allow(dead_code)] // used by concurrency and outcome tests in later slices
    pub struct MockRunner {
        outcome: AttemptOutcome,
        pub invocations: Mutex<Vec<String>>,
    }

    #[allow(dead_code)] // used by concurrency and outcome tests in later slices
    impl MockRunner {
        pub fn new(outcome: AttemptOutcome) -> Self {
            Self {
                outcome,
                invocations: Mutex::new(Vec::new()),
            }
        }

        pub fn invoked_ids(&self) -> Vec<String> {
            self.invocations.lock().unwrap().clone()
        }
    }

    impl AttemptRunner for MockRunner {
        fn run(
            &self,
            _project_root: &Path,
            work_item_id: &str,
            _attempt_id: &str,
            _tester_no_sandbox: bool,
        ) -> Result<AttemptOutcome> {
            self.invocations
                .lock()
                .unwrap()
                .push(work_item_id.to_string());
            Ok(self.outcome)
        }
    }

    /// A runner that drives the bound Attempt to a passing Merge Candidate
    /// before returning `CandidateReady`, so reconcile can produce
    /// `candidate-ready`.
    pub struct PassingRunner;

    impl AttemptRunner for PassingRunner {
        fn run(
            &self,
            project_root: &Path,
            work_item_id: &str,
            attempt_id: &str,
            _tester_no_sandbox: bool,
        ) -> Result<AttemptOutcome> {
            make_attempt_pass_with_candidate(project_root, work_item_id, attempt_id);
            Ok(AttemptOutcome::CandidateReady)
        }
    }

    pub fn setup_project(tmp: &Path) {
        fs::create_dir_all(tmp.join(".fluent/work/items")).unwrap();
    }

    pub fn test_build(hash: &str) -> BuildIdentity {
        BuildIdentity {
            version: "0.1.5 test".to_string(),
            hash: hash.to_string(),
        }
    }

    pub fn write_ready_work_item(tmp: &Path, id: &str) {
        fs::write(
            tmp.join(format!(".fluent/work/items/{id}.json")),
            format!(r#"{{"id": "{id}", "title": "Test"}}"#),
        )
        .unwrap();
    }

    /// Drive a bound write Attempt to `Complete` + passing review with a Merge
    /// Candidate, mirroring the shape a real passing Attempt produces.
    pub fn make_attempt_pass_with_candidate(
        project_root: &Path,
        work_item_id: &str,
        attempt_id: &str,
    ) {
        let store = WorkModelStore::new(project_root);
        let mut item = store.read_work_item(work_item_id).unwrap();
        let attempt = item
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt_id)
            .expect("bound attempt exists");
        let task = attempt.tasks.last_mut().unwrap();
        task.status = TaskStatus::Complete;
        task.workspace_access = WorkspaceAccess {
            reads: vec![WorkspaceRef {
                id: "target".to_string(),
                path: ".".to_string(),
            }],
            writes: vec![WorkspaceRef {
                id: "candidate".to_string(),
                path: format!("../work-{work_item_id}-{attempt_id}"),
            }],
        };
        task.output = Some(TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: format!("../work-{work_item_id}-{attempt_id}"),
            source_branch: "main".to_string(),
            base_commit: None,
            commit: "abc123".to_string(),
            no_change: None,
            learner_canonicalization: None,
        });
        attempt.status = AttemptStatus::Complete;
        attempt.review_state = Some(AttemptReviewState::Passed);
        attempt.pause_kind = None;
        attempt.learning = Some(crate::work_model::AttemptLearning::succeeded(
            1,
            crate::follow_up::ArtifactRef {
                path: "handoff.json".to_string(),
                digest: "sha256:test".to_string(),
            },
        ));
        item.create_or_get_merge_candidate(attempt_id).unwrap();
        store.write_work_item(&item).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::work_model::{
        MergeCandidateMergeStatus, PauseKind, PauseRecovery, TaskStatus, WorkItem,
    };

    fn add_queued(project_root: &Path, id: &str, priority: i64) {
        queue::add(project_root, id, Some(priority)).unwrap();
    }

    #[test]
    fn scheduler_child_receives_one_shared_baseline_session_without_disabling_sandbox() {
        let command = attempt_run_command(
            Path::new("/tmp/fluent"),
            Path::new("/tmp/project"),
            "work-1",
            "attempt-1",
            "scheduler-session-1",
            false,
        );
        let session = command
            .get_envs()
            .find(|(key, _)| *key == crate::baseline_cache::BASELINE_SESSION_ENV)
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str());

        assert_eq!(session, Some("scheduler-session-1"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["attempt", "run", "work-1", "attempt-1"]
        );
    }

    #[test]
    fn scheduler_child_uses_tester_only_capability_for_approved_recovery() {
        let approved = attempt_run_command(
            Path::new("/tmp/fluent"),
            Path::new("/tmp/project"),
            "work-1",
            "attempt-1",
            "scheduler-session-1",
            true,
        );
        assert_eq!(
            approved.get_args().collect::<Vec<_>>(),
            [
                "attempt",
                "run",
                "--tester-no-sandbox",
                "work-1",
                "attempt-1"
            ]
        );
    }

    #[test]
    fn persisted_needs_user_is_authoritative_scheduler_outcome() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-paused", "Paused");
        item.add_initial_attempt("attempt-1").unwrap();
        item.attempts[0].status = AttemptStatus::NeedsUser;
        item.attempts[0].tasks[0].status = TaskStatus::NeedsUser;
        store.create_work_item(&item).unwrap();

        assert_eq!(
            classify_attempt_outcome(dir.path(), "wi-paused", "attempt-1").unwrap(),
            AttemptOutcome::NeedsUser
        );
    }

    #[test]
    fn scheduler_created_attempt_snapshots_project_coder_mapping() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-profile");
        std::fs::create_dir_all(dir.path().join(".fluent")).unwrap();
        std::fs::write(
            dir.path().join(".fluent/config.yaml"),
            "coders:\n  writer:\n    coder: codex\n    model: gpt-write\n    effort: medium\n  reviewer:\n    coder: claude\n    model: claude-review\n    effort: high\n",
        )
        .unwrap();

        ensure_bound_attempt(dir.path(), "wi-profile", "attempt-1").unwrap();

        let item = WorkModelStore::new(dir.path())
            .read_work_item("wi-profile")
            .unwrap();
        let mapping = &item.attempts[0].coder_mapping;
        assert_eq!(mapping.write.coder, crate::coder::CoderKind::Codex);
        assert_eq!(mapping.write.model, "gpt-write");
        assert_eq!(mapping.write.effort.as_deref(), Some("medium"));
        assert_eq!(mapping.review.coder, crate::coder::CoderKind::Claude);
        assert_eq!(mapping.review.model, "claude-review");
        assert_eq!(mapping.review.effort.as_deref(), Some("high"));
    }

    #[test]
    fn scheduler_preserves_existing_attempt_coder_mapping() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-resume-profile", "Resume profile");
        let persisted = crate::work_model::CoderMapping {
            write: crate::work_model::CoderModelPair {
                coder: crate::coder::CoderKind::Pi,
                model: "persisted-write".to_string(),
                effort: Some("high".to_string()),
            },
            review: crate::work_model::CoderModelPair {
                coder: crate::coder::CoderKind::Codex,
                model: "persisted-review".to_string(),
                effort: Some("medium".to_string()),
            },
        };
        item.add_initial_attempt_with_coder_mapping("attempt-1", persisted.clone())
            .unwrap();
        store.create_work_item(&item).unwrap();
        std::fs::create_dir_all(dir.path().join(".fluent")).unwrap();
        std::fs::write(
            dir.path().join(".fluent/config.yaml"),
            "coders:\n  writer:\n    coder: claude\n  reviewer:\n    coder: claude\n",
        )
        .unwrap();

        ensure_bound_attempt(dir.path(), "wi-resume-profile", "attempt-1").unwrap();

        let item = store.read_work_item("wi-resume-profile").unwrap();
        assert_eq!(item.attempts[0].coder_mapping, persisted);
    }

    #[test]
    fn complete_attempt_without_succeeded_learner_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-unlearned");
        let store = WorkModelStore::new(dir.path());
        let mut item = store.read_work_item("wi-unlearned").unwrap();
        item.add_initial_attempt("attempt-1").unwrap();
        store.write_work_item(&item).unwrap();
        make_attempt_pass_with_candidate(dir.path(), "wi-unlearned", "attempt-1");
        store
            .mutate_work_item("wi-unlearned", |item| {
                item.attempts[0].learning = Some(crate::work_model::AttemptLearning::failed(
                    1,
                    "Learner candidate gate rejected the candidate",
                ));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            classify_attempt_outcome(dir.path(), "wi-unlearned", "attempt-1").unwrap(),
            AttemptOutcome::Blocked
        );
        assert!(matches!(
            bound_attempt_terminal_status(dir.path(), "wi-unlearned", Some("attempt-1")).unwrap(),
            BoundAttemptState::Terminal(DispatchStatus::Blocked)
        ));
    }

    #[test]
    fn claim_binds_existing_current_planned_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-retry", "Retry");
        item.add_initial_attempt("attempt-1").unwrap();
        item.attempts[0].status = AttemptStatus::NeedsUser;
        item.attempts[0].tasks[0].status = TaskStatus::NeedsUser;
        item.add_initial_attempt("attempt-2").unwrap();
        store.create_work_item(&item).unwrap();
        add_queued(dir.path(), "wi-retry", 0);

        let (token, _lease) = claim_next(dir.path()).unwrap().unwrap();

        assert_eq!(token.bound_attempt_id, "attempt-2");
    }

    /// Acquire and return the whole-Attempt lease for a Work Item so a test can
    /// hold a claim live across capacity assertions.
    fn hold_lease(project_root: &Path, work_item_id: &str) -> TaskLease {
        lease::acquire(&queue::dispatch_lease_path(project_root, work_item_id)).unwrap()
    }

    /// Drive a Work Item to a running dispatch with a held lease, returning the
    /// lease so the caller keeps the claim live.
    fn make_running(project_root: &Path, id: &str) -> TaskLease {
        add_queued(project_root, id, 0);
        let token = queue::claim(project_root, id, "attempt-1")
            .unwrap()
            .unwrap();
        let lease = hold_lease(project_root, id);
        queue::mark_running(project_root, &token).unwrap();
        lease
    }

    #[test]
    fn project_capacity_counts_all_claimed_and_running_work() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        for id in ["wi-claimed", "wi-running", "wi-queued", "wi-stale"] {
            write_ready_work_item(dir.path(), id);
        }

        // A claimed dispatch with a live lease.
        add_queued(dir.path(), "wi-claimed", 0);
        queue::claim(dir.path(), "wi-claimed", "attempt-1")
            .unwrap()
            .unwrap();
        let _claimed_lease = hold_lease(dir.path(), "wi-claimed");

        // A running dispatch with a live lease.
        let _running_lease = make_running(dir.path(), "wi-running");

        // A merely-queued dispatch does not count.
        add_queued(dir.path(), "wi-queued", 0);

        // A stale claim whose lease is dead does not count toward live capacity.
        add_queued(dir.path(), "wi-stale", 0);
        queue::claim(dir.path(), "wi-stale", "attempt-1")
            .unwrap()
            .unwrap();

        assert_eq!(project_local_capacity_used(dir.path()).unwrap(), 2);
    }

    #[test]
    fn direct_and_fargate_attempts_do_not_consume_local_scheduler_capacity() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());

        // A directly started Attempt exists in the Work model but has no queue
        // dispatch, so it never counts toward local scheduler capacity.
        let store = WorkModelStore::new(dir.path());
        let mut item = crate::work_model::WorkItem::planned("wi-direct", "Direct run");
        item.add_initial_attempt("attempt-1").unwrap();
        item.attempts[0].status = AttemptStatus::Executing;
        store.create_work_item(&item).unwrap();

        assert_eq!(project_local_capacity_used(dir.path()).unwrap(), 0);
    }

    #[test]
    fn nested_reviewers_use_one_work_slot_and_separate_reviewer_limit() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-review");

        // A single running dispatch counts as exactly one Work slot regardless
        // of how many reviewers run inside its Attempt.
        let _lease = make_running(dir.path(), "wi-review");
        assert_eq!(project_local_capacity_used(dir.path()).unwrap(), 1);

        // The reviewer-parallelism limit is applied independently of the Work
        // slot count.
        assert!(crate::work_attempt_loop::max_parallel_reviewers() >= 1);
    }

    #[test]
    fn scheduler_fills_available_project_capacity() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        for n in 0..6 {
            let id = format!("wi-{n}");
            write_ready_work_item(dir.path(), &id);
            add_queued(dir.path(), &id, 0);
        }

        // With capacity 4 and six queued, exactly four are claimed at once.
        let claims = claim_available(dir.path(), 4).unwrap();
        assert_eq!(claims.len(), 4);
        assert_eq!(project_local_capacity_used(dir.path()).unwrap(), 4);

        // At capacity, a further fill claims nothing while leases stay held.
        let more = claim_available(dir.path(), 4).unwrap();
        assert!(more.is_empty(), "capacity is not exceeded");

        drop(claims);
    }

    #[test]
    fn scheduler_orders_priority_then_fifo_without_preemption() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        for id in ["wi-run", "wi-hi", "wi-b", "wi-a"] {
            write_ready_work_item(dir.path(), id);
        }

        // An already-running dispatch that must not be interrupted.
        let _running = make_running(dir.path(), "wi-run");

        // Queue three: one high priority, two equal priority added oldest first.
        add_queued(dir.path(), "wi-hi", 10);
        add_queued(dir.path(), "wi-b", 5);
        add_queued(dir.path(), "wi-a", 5);

        // Capacity 3 leaves two free slots after the running dispatch.
        let claims = claim_available(dir.path(), 3).unwrap();
        let claimed: Vec<String> = claims
            .iter()
            .map(|(token, _)| token.work_item_id.clone())
            .collect();
        // Higher priority first, then the older equal-priority entry.
        assert_eq!(claimed, vec!["wi-hi", "wi-b"]);

        // The lowest-priority entry stays queued; the running one is untouched.
        let ledger_a = queue::read_ledger(dir.path(), "wi-a").unwrap().unwrap();
        assert_eq!(ledger_a.active().unwrap().status, DispatchStatus::Queued);
        let ledger_run = queue::read_ledger(dir.path(), "wi-run").unwrap().unwrap();
        assert_eq!(ledger_run.active().unwrap().status, DispatchStatus::Running);

        drop(claims);
    }

    #[test]
    fn concurrent_starts_elect_one_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let root = dir.path().to_path_buf();

        let elected = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let build = test_build("same-build");

        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let root = root.clone();
                let elected = elected.clone();
                let barrier = barrier.clone();
                let build = build.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    let start = start_or_reuse(&root, &build).unwrap();
                    match start {
                        CoordinatorStart::Elected(lease) => {
                            elected.fetch_add(1, Ordering::SeqCst);
                            // Hold the lease until every start has resolved so no
                            // other start can be elected in the meantime.
                            std::thread::sleep(Duration::from_millis(100));
                            drop(lease);
                        }
                        CoordinatorStart::Reused(metadata) => {
                            assert_eq!(metadata.build, build);
                        }
                    }
                }));
            }
        });

        assert_eq!(
            elected.load(Ordering::SeqCst),
            1,
            "exactly one coordinator is elected among concurrent starts"
        );
    }

    #[test]
    fn same_build_reuse_reports_the_elected_identity() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let build = test_build("build-a");
        let elected = match start_or_reuse(dir.path(), &build).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };

        let reused = match start_or_reuse(dir.path(), &build).unwrap() {
            CoordinatorStart::Reused(metadata) => metadata,
            CoordinatorStart::Elected(_) => panic!("live coordinator must be reused"),
        };
        assert_eq!(reused.instance_id, elected.metadata.instance_id);
        assert_eq!(reused.build, build);
        assert_eq!(reused.state, CoordinatorState::Running);
    }

    #[test]
    fn status_reads_live_identity_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let build = test_build("build-a");
        let elected = match start_or_reuse(dir.path(), &build).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };
        let metadata_path = coordinator_metadata_path(dir.path());
        let lock_path = coordinator_lock_path(dir.path());
        let metadata_before = fs::read(&metadata_path).unwrap();
        let lock_before = fs::read(&lock_path).unwrap();

        assert_eq!(
            status(dir.path()).unwrap(),
            SchedulerStatus::Running(elected.metadata.clone())
        );
        assert_eq!(fs::read(metadata_path).unwrap(), metadata_before);
        assert_eq!(fs::read(lock_path).unwrap(), lock_before);
        assert!(!stop_request_path(dir.path()).exists());
    }

    #[test]
    fn stopped_status_does_not_create_scheduler_state() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let scheduler_dir = dir.path().join(".fluent/work/scheduler");

        assert_eq!(status(dir.path()).unwrap(), SchedulerStatus::Stopped);
        assert!(!scheduler_dir.exists());
    }

    #[test]
    fn different_build_reuse_requires_explicit_stop_then_run() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let _elected = match start_or_reuse(dir.path(), &test_build("build-a")).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };

        let error = match start_or_reuse(dir.path(), &test_build("build-b")) {
            Ok(_) => panic!("different build must not reuse"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("build-a"));
        assert!(message.contains("build-b"));
        assert!(message.contains("`fluent scheduler stop`, then `fluent scheduler run`"));
    }

    #[test]
    fn run_does_not_reuse_a_draining_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let build = test_build("build-a");
        let mut elected = match start_or_reuse(dir.path(), &build).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };
        elected.mark_draining().unwrap();

        let error = match start_or_reuse(dir.path(), &build) {
            Ok(_) => panic!("draining coordinator must not be reused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("is draining"));
        assert!(error.to_string().contains("`fluent scheduler run`"));
    }

    #[test]
    fn live_coordinator_with_malformed_metadata_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let path = coordinator_lock_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .unwrap();
        flock(&lock, FlockOperation::LockExclusive).unwrap();
        fs::write(coordinator_metadata_path(dir.path()), b"not json").unwrap();

        assert!(status(dir.path()).is_err());
        assert!(start_or_reuse(dir.path(), &test_build("build-a")).is_err());
    }

    #[test]
    fn live_legacy_coordinator_without_metadata_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let path = coordinator_lock_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .unwrap();
        flock(&lock, FlockOperation::LockExclusive).unwrap();

        let error = match start_or_reuse(dir.path(), &test_build("build-a")) {
            Ok(_) => panic!("legacy coordinator must not be reused"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("stop its owning process manually")
        );
    }

    #[test]
    fn metadata_from_an_earlier_lease_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let path = coordinator_lock_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        flock(&lock, FlockOperation::LockExclusive).unwrap();
        publish_lock_identity(&mut lock, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        write_coordinator_metadata(
            dir.path(),
            &CoordinatorMetadata {
                schema: COORDINATOR_METADATA_SCHEMA,
                instance_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                pid: 42,
                started_at: "2026-08-10T00:00:00Z".to_string(),
                state: CoordinatorState::Running,
                build: test_build("stale"),
            },
        )
        .unwrap();

        let error = status(dir.path()).unwrap_err();
        assert!(error.to_string().contains("does not match lease instance"));
    }

    #[test]
    fn matching_stop_request_publishes_draining_before_exit() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let mut elected = match start_or_reuse(dir.path(), &test_build("build-a")).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };
        let request = StopRequest {
            schema: STOP_REQUEST_SCHEMA,
            instance_id: elected.metadata.instance_id.clone(),
            requested_at: chrono::Utc::now().to_rfc3339(),
        };
        crate::atomic_write::atomic_write(
            &stop_request_path(dir.path()),
            &serde_json::to_vec_pretty(&request).unwrap(),
        )
        .unwrap();

        assert!(
            shutdown_requested(dir.path(), &AtomicBool::new(false), Some(&mut elected)).unwrap()
        );
        assert_eq!(
            read_coordinator_metadata(dir.path())
                .unwrap()
                .unwrap()
                .state,
            CoordinatorState::Draining
        );
    }

    #[test]
    fn stale_stop_request_cannot_stop_replacement_instance() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        fs::create_dir_all(dir.path().join(".fluent/work/scheduler")).unwrap();
        let stale = StopRequest {
            schema: STOP_REQUEST_SCHEMA,
            instance_id: "departed-instance".to_string(),
            requested_at: chrono::Utc::now().to_rfc3339(),
        };
        fs::write(
            stop_request_path(dir.path()),
            serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();
        let mut elected = match start_or_reuse(dir.path(), &test_build("build-a")).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };

        assert!(
            !shutdown_requested(dir.path(), &AtomicBool::new(false), Some(&mut elected)).unwrap()
        );
        assert_eq!(elected.metadata.state, CoordinatorState::Running);
        assert!(!stop_request_path(dir.path()).exists());
    }

    #[test]
    fn durable_stop_request_drains_and_releases_exact_coordinator() {
        struct SlowRunner;
        impl AttemptRunner for SlowRunner {
            fn run(
                &self,
                _project_root: &Path,
                _work_item_id: &str,
                _attempt_id: &str,
                _tester_no_sandbox: bool,
            ) -> Result<AttemptOutcome> {
                std::thread::sleep(Duration::from_millis(300));
                Ok(AttemptOutcome::Failed)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-drain");
        add_queued(dir.path(), "wi-drain", 0);
        let elected = match start_or_reuse(dir.path(), &test_build("build-a")).unwrap() {
            CoordinatorStart::Elected(lease) => lease,
            CoordinatorStart::Reused(_) => panic!("first start must elect"),
        };
        let target_id = elected.metadata.instance_id.clone();
        let shutdown = AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut elected = elected;
                run_coordinator_controlled(
                    dir.path(),
                    30,
                    4,
                    &SlowRunner,
                    &shutdown,
                    Some(&mut elected),
                )
                .unwrap();
            });

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while latest_status(dir.path(), "wi-drain") != DispatchStatus::Running {
                assert!(
                    std::time::Instant::now() < deadline,
                    "scheduled Work did not start"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let stopped = stop(dir.path()).unwrap();
            assert_eq!(stopped.instance_id, target_id);
        });

        assert_eq!(
            latest_status(dir.path(), "wi-drain"),
            DispatchStatus::Failed
        );
        assert_eq!(status(dir.path()).unwrap(), SchedulerStatus::Stopped);
        assert!(!coordinator_metadata_path(dir.path()).exists());
        assert!(!stop_request_path(dir.path()).exists());
    }

    #[test]
    fn live_claim_or_execution_prevents_duplicate_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);

        // Claim it and hold the lease so the claim reads as live.
        queue::claim(dir.path(), "wi-1", "attempt-1")
            .unwrap()
            .unwrap();
        let _lease = hold_lease(dir.path(), "wi-1");

        // A second claim finds no queued dispatch and does not bind another
        // Attempt.
        assert!(
            queue::claim(dir.path(), "wi-1", "attempt-2")
                .unwrap()
                .is_none()
        );
        assert!(pick_next_queued(dir.path()).unwrap().is_none());
        assert_eq!(project_local_capacity_used(dir.path()).unwrap(), 1);
        assert!(
            claim_available(dir.path(), 4).unwrap().is_empty(),
            "a live claim is not re-dispatched"
        );
    }

    /// A runner that records the Attempt ids it was launched with, so a test can
    /// prove recovery resumed the same Attempt rather than opening a new one.
    struct AttemptRecordingRunner {
        outcome: AttemptOutcome,
        attempts: std::sync::Mutex<Vec<String>>,
        tester_no_sandbox: std::sync::Mutex<Vec<bool>>,
    }
    impl AttemptRecordingRunner {
        fn new(outcome: AttemptOutcome) -> Self {
            Self {
                outcome,
                attempts: std::sync::Mutex::new(Vec::new()),
                tester_no_sandbox: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl AttemptRunner for AttemptRecordingRunner {
        fn run(
            &self,
            _p: &Path,
            _wi: &str,
            attempt_id: &str,
            tester_no_sandbox: bool,
        ) -> Result<AttemptOutcome> {
            self.attempts.lock().unwrap().push(attempt_id.to_string());
            self.tester_no_sandbox
                .lock()
                .unwrap()
                .push(tester_no_sandbox);
            Ok(self.outcome)
        }
    }

    /// Create a bound Attempt in a given status on an existing Work Item.
    fn create_attempt(
        project_root: &Path,
        work_item_id: &str,
        attempt_id: &str,
        status: AttemptStatus,
    ) {
        let store = WorkModelStore::new(project_root);
        let mut item = store.read_work_item(work_item_id).unwrap();
        item.add_initial_attempt(attempt_id).unwrap();
        item.attempts
            .iter_mut()
            .find(|a| a.id == attempt_id)
            .unwrap()
            .status = status;
        store.write_work_item(&item).unwrap();
    }

    fn suspend_bound_attempt(project_root: &Path, work_item_id: &str, attempt_id: &str) {
        let store = WorkModelStore::new(project_root);
        store
            .mutate_work_item(work_item_id, |item| {
                let attempt = item
                    .attempts
                    .iter_mut()
                    .find(|attempt| attempt.id == attempt_id)
                    .unwrap();
                attempt.status = AttemptStatus::NeedsUser;
                attempt.pause_kind = Some(PauseKind::WriterCompletion);
                attempt.tasks[0].status = TaskStatus::NeedsUser;
                Ok(())
            })
            .unwrap();
    }

    fn active_dispatch(project_root: &Path, work_item_id: &str) -> Dispatch {
        queue::read_ledger(project_root, work_item_id)
            .unwrap()
            .unwrap()
            .active()
            .unwrap()
            .clone()
    }

    fn latest_status(project_root: &Path, work_item_id: &str) -> DispatchStatus {
        queue::read_ledger(project_root, work_item_id)
            .unwrap()
            .unwrap()
            .latest()
            .unwrap()
            .status
    }

    #[test]
    fn stale_claim_resumes_bound_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);
        queue::claim(dir.path(), "wi-1", "attempt-1")
            .unwrap()
            .unwrap();
        // The bound Attempt exists and is still in flight; the claim is stale
        // because no lease is held.
        create_attempt(dir.path(), "wi-1", "attempt-1", AttemptStatus::Executing);

        recover_and_reconcile(dir.path()).unwrap();

        // Returned to queued with its binding preserved for resume.
        let active = active_dispatch(dir.path(), "wi-1");
        assert_eq!(active.status, DispatchStatus::Queued);
        assert_eq!(active.bound_attempt_id.as_deref(), Some("attempt-1"));

        // A subsequent dispatch advances that same Attempt, creating no new one.
        let runner = AttemptRecordingRunner::new(AttemptOutcome::CandidateReady);
        dispatch_one(dir.path(), &active, &runner).unwrap();
        assert_eq!(runner.attempts.lock().unwrap().as_slice(), ["attempt-1"]);
        let store = WorkModelStore::new(dir.path());
        assert_eq!(store.read_work_item("wi-1").unwrap().attempts.len(), 1);
    }

    #[test]
    fn stale_claim_reconciles_terminal_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);
        queue::claim(dir.path(), "wi-1", "attempt-1")
            .unwrap()
            .unwrap();
        // The bound Attempt already reached a terminal outcome before the claim
        // went stale.
        create_attempt(dir.path(), "wi-1", "attempt-1", AttemptStatus::Failed);

        recover_and_reconcile(dir.path()).unwrap();

        // Reconciled from the Attempt's outcome, without running it again.
        assert_eq!(latest_status(dir.path(), "wi-1"), DispatchStatus::Failed);
        let store = WorkModelStore::new(dir.path());
        assert_eq!(store.read_work_item("wi-1").unwrap().attempts.len(), 1);
    }

    #[test]
    fn unbound_stale_claim_recovers_without_duplicate_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);
        // Claim binds an Attempt id but the Attempt was never created (a crash
        // between the claim write and Attempt creation).
        queue::claim(dir.path(), "wi-1", "attempt-1")
            .unwrap()
            .unwrap();

        recover_and_reconcile(dir.path()).unwrap();

        let active = active_dispatch(dir.path(), "wi-1");
        assert_eq!(active.status, DispatchStatus::Queued);
        assert_eq!(active.bound_attempt_id, None, "binding is cleared");

        // The next claim creates exactly one Attempt.
        let runner = AttemptRecordingRunner::new(AttemptOutcome::CandidateReady);
        dispatch_one(dir.path(), &active, &runner).unwrap();
        let store = WorkModelStore::new(dir.path());
        assert_eq!(store.read_work_item("wi-1").unwrap().attempts.len(), 1);
    }

    #[test]
    fn concurrent_terminal_outcomes_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        for id in ["wi-fail", "wi-nu", "wi-keep"] {
            write_ready_work_item(dir.path(), id);
            add_queued(dir.path(), id, 0);
        }

        let fail = active_dispatch(dir.path(), "wi-fail");
        dispatch_one(dir.path(), &fail, &MockRunner::new(AttemptOutcome::Failed)).unwrap();
        let nu = active_dispatch(dir.path(), "wi-nu");
        dispatch_one(dir.path(), &nu, &MockRunner::new(AttemptOutcome::NeedsUser)).unwrap();

        assert_eq!(latest_status(dir.path(), "wi-fail"), DispatchStatus::Failed);
        assert_eq!(
            latest_status(dir.path(), "wi-nu"),
            DispatchStatus::NeedsUser
        );
        // The third entry is untouched by the others' outcomes.
        assert_eq!(
            active_dispatch(dir.path(), "wi-keep").status,
            DispatchStatus::Queued
        );
    }

    #[test]
    fn needs_user_recovery_resumes_bound_attempt_and_reconciles_queue() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);
        queue::claim(dir.path(), "wi-1", "attempt-1")
            .unwrap()
            .unwrap();
        create_attempt(dir.path(), "wi-1", "attempt-1", AttemptStatus::NeedsUser);
        queue::reconcile_active(dir.path(), "wi-1", DispatchStatus::NeedsUser).unwrap();
        assert_eq!(latest_status(dir.path(), "wi-1"), DispatchStatus::NeedsUser);

        // A human resumes the same Attempt through its recovery action; it now
        // completes with a passing Merge Candidate.
        make_attempt_pass_with_candidate(dir.path(), "wi-1", "attempt-1");
        let store = WorkModelStore::new(dir.path());

        recover_and_reconcile(dir.path()).unwrap();

        // The queue entry reconciles to the new outcome; no new Attempt appears.
        assert_eq!(
            latest_status(dir.path(), "wi-1"),
            DispatchStatus::CandidateReady
        );
        assert_eq!(store.read_work_item("wi-1").unwrap().attempts.len(), 1);
    }

    #[test]
    fn explicit_resume_survives_restart_and_runs_exact_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-resume", "Resume");
        let persisted_mapping = crate::work_model::CoderMapping {
            write: crate::work_model::CoderModelPair {
                coder: crate::coder::CoderKind::Codex,
                model: "persisted-write".to_string(),
                effort: Some("medium".to_string()),
            },
            review: crate::work_model::CoderModelPair {
                coder: crate::coder::CoderKind::Pi,
                model: "persisted-review".to_string(),
                effort: Some("high".to_string()),
            },
        };
        item.add_initial_attempt_with_coder_mapping("attempt-1", persisted_mapping.clone())
            .unwrap();
        store.create_work_item(&item).unwrap();
        std::fs::create_dir_all(dir.path().join(".fluent")).unwrap();
        std::fs::write(
            dir.path().join(".fluent/config.yaml"),
            "coders:\n  writer:\n    coder: claude\n  reviewer:\n    coder: claude\n",
        )
        .unwrap();
        add_queued(dir.path(), "wi-resume", 0);
        queue::claim(dir.path(), "wi-resume", "attempt-1")
            .unwrap()
            .unwrap();
        suspend_bound_attempt(dir.path(), "wi-resume", "attempt-1");
        queue::reconcile_active(dir.path(), "wi-resume", DispatchStatus::NeedsUser).unwrap();

        queue::resume(dir.path(), "wi-resume", "attempt-1", Some(9)).unwrap();
        // A coordinator restart reconciles durable state without consuming or
        // replacing the queued resume authorization.
        recover_and_reconcile(dir.path()).unwrap();
        let resumed = active_dispatch(dir.path(), "wi-resume");
        assert_eq!(resumed.bound_attempt_id.as_deref(), Some("attempt-1"));

        dispatch_one(dir.path(), &resumed, &PassingRunner).unwrap();

        let item = store.read_work_item("wi-resume").unwrap();
        assert_eq!(item.attempts.len(), 1, "resume creates no new Attempt");
        assert_eq!(item.attempts[0].id, "attempt-1");
        assert_eq!(item.attempts[0].status, AttemptStatus::Complete);
        assert_eq!(item.attempts[0].coder_mapping, persisted_mapping);
        assert_eq!(item.merge_candidates.len(), 1);
        let ledger = queue::read_ledger(dir.path(), "wi-resume")
            .unwrap()
            .unwrap();
        assert_eq!(ledger.dispatches.len(), 2);
        assert_eq!(ledger.dispatches[0].status, DispatchStatus::NeedsUser);
        assert_eq!(ledger.dispatches[1].status, DispatchStatus::CandidateReady);
    }

    #[test]
    fn candidate_gate_resume_runs_the_same_attempt_through_scheduler() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-resume", "Resume candidate gate");
        item.add_initial_attempt("attempt-1").unwrap();
        store.create_work_item(&item).unwrap();
        add_queued(dir.path(), "wi-resume", 0);
        queue::claim(dir.path(), "wi-resume", "attempt-1")
            .unwrap()
            .unwrap();
        suspend_bound_attempt(dir.path(), "wi-resume", "attempt-1");
        store
            .mutate_work_item("wi-resume", |item| {
                item.attempts[0].pause_kind = Some(PauseKind::CandidateGate);
                Ok(())
            })
            .unwrap();
        queue::reconcile_active(dir.path(), "wi-resume", DispatchStatus::NeedsUser).unwrap();

        queue::resume(dir.path(), "wi-resume", "attempt-1", None).unwrap();
        let resumed = active_dispatch(dir.path(), "wi-resume");
        dispatch_one(dir.path(), &resumed, &PassingRunner).unwrap();

        let item = store.read_work_item("wi-resume").unwrap();
        assert_eq!(item.attempts.len(), 1);
        assert_eq!(item.attempts[0].id, "attempt-1");
        assert_eq!(item.attempts[0].status, AttemptStatus::Complete);
        let ledger = queue::read_ledger(dir.path(), "wi-resume")
            .unwrap()
            .unwrap();
        assert_eq!(ledger.dispatches.len(), 2);
        assert_eq!(ledger.dispatches[0].status, DispatchStatus::NeedsUser);
        assert_eq!(ledger.dispatches[1].status, DispatchStatus::CandidateReady);
    }

    #[test]
    fn explicit_resume_can_return_to_needs_user_without_losing_history() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-resume", "Resume");
        item.add_initial_attempt("attempt-1").unwrap();
        store.create_work_item(&item).unwrap();
        add_queued(dir.path(), "wi-resume", 0);
        queue::claim(dir.path(), "wi-resume", "attempt-1")
            .unwrap()
            .unwrap();
        suspend_bound_attempt(dir.path(), "wi-resume", "attempt-1");
        queue::reconcile_active(dir.path(), "wi-resume", DispatchStatus::NeedsUser).unwrap();
        queue::resume(dir.path(), "wi-resume", "attempt-1", None).unwrap();

        let resumed = active_dispatch(dir.path(), "wi-resume");
        dispatch_one(
            dir.path(),
            &resumed,
            &AttemptRecordingRunner::new(AttemptOutcome::NeedsUser),
        )
        .unwrap();

        let ledger = queue::read_ledger(dir.path(), "wi-resume")
            .unwrap()
            .unwrap();
        assert_eq!(ledger.dispatches.len(), 2);
        assert!(
            ledger
                .dispatches
                .iter()
                .all(|dispatch| dispatch.status == DispatchStatus::NeedsUser)
        );
        assert_eq!(store.read_work_item("wi-resume").unwrap().attempts.len(), 1);
    }

    #[test]
    fn tester_recovery_dispatch_forwards_one_run_no_sandbox_approval() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        let store = WorkModelStore::new(dir.path());
        let mut item = WorkItem::planned("wi-resume", "Resume Tester");
        item.add_initial_attempt("attempt-1").unwrap();
        store.create_work_item(&item).unwrap();
        add_queued(dir.path(), "wi-resume", 0);
        queue::claim(dir.path(), "wi-resume", "attempt-1")
            .unwrap()
            .unwrap();
        store
            .mutate_work_item("wi-resume", |item| {
                let attempt = &mut item.attempts[0];
                attempt.status = AttemptStatus::NeedsUser;
                attempt.pause_kind = Some(PauseKind::TesterHarness);
                attempt.pause_recovery = Some(PauseRecovery::TesterNoSandbox);
                attempt.tasks[0].status = TaskStatus::NeedsUser;
                Ok(())
            })
            .unwrap();
        queue::reconcile_active(dir.path(), "wi-resume", DispatchStatus::NeedsUser).unwrap();
        queue::resume_with_tester_no_sandbox(dir.path(), "wi-resume", "attempt-1", None, true)
            .unwrap();

        let runner = AttemptRecordingRunner::new(AttemptOutcome::NeedsUser);
        let resumed = active_dispatch(dir.path(), "wi-resume");
        dispatch_one(dir.path(), &resumed, &runner).unwrap();

        assert_eq!(runner.attempts.lock().unwrap().as_slice(), ["attempt-1"]);
        assert_eq!(runner.tester_no_sandbox.lock().unwrap().as_slice(), [true]);
        let terminal = queue::read_ledger(dir.path(), "wi-resume")
            .unwrap()
            .unwrap();
        assert_eq!(terminal.latest().unwrap().status, DispatchStatus::NeedsUser);
        assert!(
            terminal.latest().unwrap().tester_no_sandbox,
            "the terminal dispatch retains the approval as audit evidence"
        );
    }

    #[test]
    fn abandoned_queued_work_is_canceled_before_attempt() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-abandoned");
        write_ready_work_item(dir.path(), "wi-ok");
        add_queued(dir.path(), "wi-abandoned", 0);
        add_queued(dir.path(), "wi-ok", 0);

        let store = WorkModelStore::new(dir.path());
        let mut item = store.read_work_item("wi-abandoned").unwrap();
        item.abandon(Some("superseded".to_string()), None).unwrap();
        store.write_work_item(&item).unwrap();

        recover_and_reconcile(dir.path()).unwrap();

        assert_eq!(
            latest_status(dir.path(), "wi-abandoned"),
            DispatchStatus::Canceled
        );
        assert!(
            store
                .read_work_item("wi-abandoned")
                .unwrap()
                .attempts
                .is_empty(),
            "no Attempt is created for abandoned Work"
        );
        // The other eligible entry keeps processing.
        assert_eq!(
            active_dispatch(dir.path(), "wi-ok").status,
            DispatchStatus::Queued
        );
    }

    #[test]
    fn invalid_queue_reference_is_blocked_without_stalling_scheduler() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-missing");
        write_ready_work_item(dir.path(), "wi-ok");
        add_queued(dir.path(), "wi-missing", 0);
        add_queued(dir.path(), "wi-ok", 0);

        // The referenced Work Item disappears after being queued.
        fs::remove_file(dir.path().join(".fluent/work/items/wi-missing.json")).unwrap();

        recover_and_reconcile(dir.path()).unwrap();

        let blocked = queue::read_ledger(dir.path(), "wi-missing")
            .unwrap()
            .unwrap();
        let blocked = blocked.latest().unwrap();
        assert_eq!(blocked.status, DispatchStatus::Blocked);
        assert!(blocked.blocked_reason.is_some(), "blocked reason recorded");
        // The other eligible entry is unaffected.
        assert_eq!(
            active_dispatch(dir.path(), "wi-ok").status,
            DispatchStatus::Queued
        );
    }

    #[test]
    fn lowered_capacity_does_not_preempt_and_blocks_claims() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        for id in ["wi-r1", "wi-r2", "wi-r3", "wi-q"] {
            write_ready_work_item(dir.path(), id);
        }

        // Three dispatches are already running.
        let _l1 = make_running(dir.path(), "wi-r1");
        let _l2 = make_running(dir.path(), "wi-r2");
        let _l3 = make_running(dir.path(), "wi-r3");
        add_queued(dir.path(), "wi-q", 0);

        // The configured limit is now two — below the three already running.
        let claims = claim_available(dir.path(), 2).unwrap();
        assert!(claims.is_empty(), "no new Work starts above the new limit");

        // Running Work is undisturbed and the queued entry stays queued.
        for id in ["wi-r1", "wi-r2", "wi-r3"] {
            assert_eq!(
                active_dispatch(dir.path(), id).status,
                DispatchStatus::Running
            );
        }
        assert_eq!(
            active_dispatch(dir.path(), "wi-q").status,
            DispatchStatus::Queued
        );
    }

    #[test]
    fn shutdown_stops_claiming_and_preserves_unclaimed_work() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);

        // Shutdown is already requested: the coordinator claims nothing.
        let shutdown = AtomicBool::new(true);
        run_coordinator(
            dir.path(),
            1,
            4,
            &MockRunner::new(AttemptOutcome::CandidateReady),
            &shutdown,
        )
        .unwrap();

        assert_eq!(
            active_dispatch(dir.path(), "wi-1").status,
            DispatchStatus::Queued,
            "unclaimed Work is preserved as queued"
        );
    }

    #[test]
    fn graceful_shutdown_drains_claimed_work() {
        // A runner that lingers so shutdown arrives while its Attempt is live.
        struct SlowRunner;
        impl AttemptRunner for SlowRunner {
            fn run(
                &self,
                _p: &Path,
                _wi: &str,
                _a: &str,
                _tester_no_sandbox: bool,
            ) -> Result<AttemptOutcome> {
                std::thread::sleep(Duration::from_millis(300));
                Ok(AttemptOutcome::CandidateReady)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 0);

        let shutdown = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                run_coordinator(dir.path(), 1, 4, &SlowRunner, &shutdown).unwrap();
            });

            // Wait until the dispatch is live, then request shutdown mid-flight.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if latest_status(dir.path(), "wi-1") == DispatchStatus::Running {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "dispatch never ran");
                std::thread::sleep(Duration::from_millis(20));
            }
            shutdown.store(true, Ordering::Release);
        });

        // The in-flight Attempt was drained and its outcome recorded.
        assert_eq!(
            latest_status(dir.path(), "wi-1"),
            DispatchStatus::CandidateReady
        );
    }

    #[test]
    fn claim_persists_bound_attempt_before_launch() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        std::fs::create_dir_all(dir.path().join(".fluent")).unwrap();
        std::fs::write(
            dir.path().join(".fluent/config.yaml"),
            "coders:\n  writer:\n    coder: codex\n    model: scheduled-write\n  reviewer:\n    coder: pi\n    model: scheduled-review\n",
        )
        .unwrap();
        add_queued(dir.path(), "wi-1", 1);

        // A runner that inspects the ledger and Work model at launch time:
        // the dispatch must already be bound to exactly one created Attempt.
        struct InspectRunner {
            root: std::path::PathBuf,
        }
        impl AttemptRunner for InspectRunner {
            fn run(
                &self,
                _p: &Path,
                wi: &str,
                attempt_id: &str,
                _tester_no_sandbox: bool,
            ) -> Result<AttemptOutcome> {
                let ledger = queue::read_ledger(&self.root, wi).unwrap().unwrap();
                let active = ledger.active().unwrap();
                assert_eq!(active.bound_attempt_id.as_deref(), Some(attempt_id));
                let store = WorkModelStore::new(&self.root);
                let item = store.read_work_item(wi).unwrap();
                assert_eq!(
                    item.attempts.iter().filter(|a| a.id == attempt_id).count(),
                    1,
                    "exactly one bound Attempt exists at launch"
                );
                let attempt = item
                    .attempts
                    .iter()
                    .find(|attempt| attempt.id == attempt_id)
                    .unwrap();
                assert_eq!(
                    attempt.coder_mapping.write.coder,
                    crate::coder::CoderKind::Codex
                );
                assert_eq!(attempt.coder_mapping.write.model, "scheduled-write");
                assert_eq!(
                    attempt.coder_mapping.review.coder,
                    crate::coder::CoderKind::Pi
                );
                assert_eq!(attempt.coder_mapping.review.model, "scheduled-review");
                Ok(AttemptOutcome::CandidateReady)
            }
        }

        let dispatch = pick_next_queued(dir.path()).unwrap().unwrap();
        let runner = InspectRunner {
            root: dir.path().to_path_buf(),
        };
        dispatch_one(dir.path(), &dispatch, &runner).unwrap();
    }

    #[test]
    fn bound_claim_transitions_to_running_on_launch() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 1);

        struct RunningCheckRunner {
            root: std::path::PathBuf,
        }
        impl AttemptRunner for RunningCheckRunner {
            fn run(
                &self,
                _p: &Path,
                wi: &str,
                _a: &str,
                _tester_no_sandbox: bool,
            ) -> Result<AttemptOutcome> {
                let ledger = queue::read_ledger(&self.root, wi).unwrap().unwrap();
                assert_eq!(
                    ledger.active().unwrap().status,
                    DispatchStatus::Running,
                    "dispatch is running during launch"
                );
                Ok(AttemptOutcome::CandidateReady)
            }
        }

        let dispatch = pick_next_queued(dir.path()).unwrap().unwrap();
        let runner = RunningCheckRunner {
            root: dir.path().to_path_buf(),
        };
        dispatch_one(dir.path(), &dispatch, &runner).unwrap();
    }

    #[test]
    fn passing_attempt_becomes_candidate_ready_without_land() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 1);

        let dispatch = pick_next_queued(dir.path()).unwrap().unwrap();
        dispatch_one(dir.path(), &dispatch, &PassingRunner).unwrap();

        let ledger = queue::read_ledger(dir.path(), "wi-1").unwrap().unwrap();
        assert_eq!(
            ledger.latest().unwrap().status,
            DispatchStatus::CandidateReady
        );
    }

    #[test]
    fn scheduler_never_invokes_merge() {
        let dir = tempfile::tempdir().unwrap();
        setup_project(dir.path());
        write_ready_work_item(dir.path(), "wi-1");
        add_queued(dir.path(), "wi-1", 1);

        let dispatch = pick_next_queued(dir.path()).unwrap().unwrap();
        dispatch_one(dir.path(), &dispatch, &PassingRunner).unwrap();

        // The Merge Candidate is created but left pending: the scheduler never
        // lands it.
        let store = WorkModelStore::new(dir.path());
        let item = store.read_work_item("wi-1").unwrap();
        let candidate = item.merge_candidates.last().expect("candidate created");
        assert_eq!(
            candidate.merge_state.status,
            MergeCandidateMergeStatus::Pending,
            "scheduler must not land the Merge Candidate"
        );
    }
}
