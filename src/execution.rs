//! Persist and control the host-owned process identity for a local coder Task.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const EXECUTION_FILE: &str = "execution.json";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionState {
    Launching,
    Running,
    Settled,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub schema_version: u32,
    pub state: ExecutionState,
    pub owner_pid: u32,
    pub leader_pid: u32,
    pub process_group_id: u32,
    pub started_at: String,
    pub heartbeat_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<String>,
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn mutate_record<T>(
    path: &Path,
    mutate: impl FnOnce(&mut ExecutionRecord) -> Result<T>,
) -> Result<T> {
    let _lock = crate::lease::acquire_blocking(&lock_path(path))
        .with_context(|| format!("locking local execution record {}", path.display()))?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading local execution record {}", path.display()))?;
    let mut record: ExecutionRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing local execution record {}", path.display()))?;
    let result = mutate(&mut record)?;
    crate::atomic_write::atomic_write(path, &serde_json::to_vec_pretty(&record)?)
        .with_context(|| format!("writing local execution record {}", path.display()))?;
    Ok(result)
}

fn identity_lock_path(path: &Path) -> PathBuf {
    path.with_file_name("execution-owner.lock")
}

fn identity_is_held(path: &Path) -> Result<bool> {
    let file = match File::open(identity_lock_path(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("opening execution identity lock"),
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
            Ok(false)
        }
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::WouldBlock => {
            Ok(true)
        }
        Err(error) => Err(std::io::Error::from(error)).context("checking execution identity lock"),
    }
}

#[cfg(unix)]
fn process_group_exists(process_group_id: u32) -> Result<bool> {
    let group = i32::try_from(process_group_id).context("process group id is out of range")?;
    let result = unsafe { libc::kill(-group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("checking local coder process group"),
    }
}

#[cfg(unix)]
fn leader_matches_process_group(leader_pid: u32, process_group_id: u32) -> Result<bool> {
    let leader = i32::try_from(leader_pid).context("leader pid is out of range")?;
    let expected_group =
        i32::try_from(process_group_id).context("process group id is out of range")?;
    let actual_group = unsafe { libc::getpgid(leader) };
    if actual_group >= 0 {
        return Ok(actual_group == expected_group);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // A group leader may exit while its descendants remain in the group.
        // The inherited identity lock plus the still-live group remains the
        // durable binding in that case.
        return Ok(true);
    }
    Err(error).context("checking local coder group leader")
}

#[cfg(not(unix))]
fn process_group_exists(_process_group_id: u32) -> Result<bool> {
    bail!("local execution cancellation requires Unix process groups")
}

#[cfg(not(unix))]
fn leader_matches_process_group(_leader_pid: u32, _process_group_id: u32) -> Result<bool> {
    bail!("local execution cancellation requires Unix process groups")
}

fn recorded_identity_is_signalable(path: &Path, record: &ExecutionRecord) -> Result<bool> {
    Ok(identity_is_held(path)?
        && process_group_exists(record.process_group_id)?
        && leader_matches_process_group(record.leader_pid, record.process_group_id)?)
}

#[cfg(unix)]
fn signal_process_group(process_group_id: u32, signal: i32) -> Result<()> {
    let group = i32::try_from(process_group_id).context("process group id is out of range")?;
    let result = unsafe { libc::kill(-group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("signaling local coder process group")
}

#[cfg(not(unix))]
fn signal_process_group(_process_group_id: u32, _signal: i32) -> Result<()> {
    bail!("local execution cancellation requires Unix process groups")
}

fn wait_for_identity_release(path: &Path, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !identity_is_held(path)? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(!identity_is_held(path)?)
}

pub fn record_path_for_transcript(transcript: &Path) -> PathBuf {
    transcript.with_file_name(EXECUTION_FILE)
}

pub fn record_path_for_artifact(project_root: &Path, artifact_area: &str) -> PathBuf {
    project_root.join(artifact_area).join(EXECUTION_FILE)
}

pub struct ExecutionHeartbeat {
    path: PathBuf,
    last_write: Instant,
}

/// Configure a lock that the child opens before exec and its descendants inherit.
/// Contention then proves that the original process tree still owns the recorded
/// identity even if the Fluent owner has exited.
#[cfg(unix)]
pub fn configure_identity_lock(command: &mut Command, transcript: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    let record_path = record_path_for_transcript(transcript);
    if let Some(parent) = record_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = std::ffi::CString::new(identity_lock_path(&record_path).as_os_str().as_bytes())
        .context("execution identity path contains a NUL byte")?;
    unsafe {
        command.pre_exec(move || {
            let descriptor = libc::open(path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Never let Command::spawn hang inside pre_exec behind a stale
            // descendant from an earlier launch in this artifact area.
            if libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn configure_identity_lock(_command: &mut Command, _transcript: &Path) -> Result<()> {
    bail!("local execution identity requires Unix process groups")
}

/// Publish a cancellable owner before the Task becomes visibly executing.
/// The caller holds its Work Item mutation boundary through reservation.
pub(crate) fn prepare_launch(transcript: &Path) -> Result<()> {
    let path = record_path_for_transcript(transcript);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::lease::acquire_blocking(&lock_path(&path))?;
    anyhow::ensure!(
        !identity_is_held(&path)?,
        "prior local execution still owns its identity"
    );
    let now = crate::work_model::now_iso8601();
    let record = ExecutionRecord {
        schema_version: 1,
        state: ExecutionState::Launching,
        owner_pid: std::process::id(),
        leader_pid: 0,
        process_group_id: 0,
        started_at: now.clone(),
        heartbeat_at: now,
        settled_at: None,
    };
    crate::atomic_write::atomic_write(&path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(())
}

thread_local! {
    static TASK_OWNER: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Attribute nested deterministic commands, including baseline checks, to their Task.
pub(crate) struct TaskOwner(Option<PathBuf>);
impl TaskOwner {
    pub(crate) fn enter(transcript: PathBuf) -> Self {
        Self(TASK_OWNER.with(|owner| owner.replace(Some(transcript))))
    }
}
impl Drop for TaskOwner {
    fn drop(&mut self) {
        TASK_OWNER.with(|owner| owner.replace(self.0.take()));
    }
}
fn owned_transcript(transcript: &Path) -> PathBuf {
    TASK_OWNER
        .with(|owner| owner.borrow().clone())
        .unwrap_or_else(|| transcript.to_path_buf())
}

/// Serialize the stop check, child creation, and child identity publication.
/// Cancellation observes either a stopped launch or the fully registered child.
pub fn spawn(command: &mut Command, transcript: &Path) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let owned = owned_transcript(transcript);
    let transcript = owned.as_path();
    command.process_group(0);
    configure_identity_lock(command, transcript)?;
    let path = record_path_for_transcript(transcript);
    let _lock = crate::lease::acquire_blocking(&lock_path(&path))?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let record: ExecutionRecord = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                record.state != ExecutionState::Canceled,
                "local execution was canceled before launch"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let child = command.spawn()?;
    if let Err(error) = write_running_record(&path, child.id()) {
        return match settle_failed_launch(child, transcript) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("launch cleanup failed: {cleanup:#}"))),
        };
    }
    Ok(child)
}

/// Sweep and reap a spawned child when setup fails before normal supervision.
pub(crate) fn settle_failed_launch(child: std::process::Child, transcript: &Path) -> Result<()> {
    let (_, _, disposition) =
        crate::coder::wait_owned_process_tree(child, Duration::ZERO).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        disposition.is_verified_swept(),
        "failed launch process cleanup was not confirmed: {disposition:?}"
    );
    anyhow::ensure!(
        wait_for_identity_release(
            &record_path_for_transcript(&owned_transcript(transcript)),
            TERMINATION_GRACE,
        )?,
        "failed launch still owns its execution identity"
    );
    Ok(())
}

/// Capture a deterministic Task command under the same cancellation identity.
/// Keep ordinary Tester leak detection authoritative after a natural exit.
pub(crate) fn output(command: &mut Command, transcript: &Path) -> Result<std::process::Output> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = spawn(command, transcript)?;
    let result = child.wait_with_output();
    let path = record_path_for_transcript(&owned_transcript(transcript));
    if result.is_err() {
        cancel(&path)?;
    }
    if !identity_is_held(&path)? {
        mutate_record(&path, |record| {
            if record.state == ExecutionState::Running {
                record.state = ExecutionState::Settled;
                record.settled_at = Some(crate::work_model::now_iso8601());
            }
            Ok(())
        })?;
    }
    result.context("capture local Task command")
}

fn write_running_record(path: &Path, leader_pid: u32) -> Result<()> {
    let now = crate::work_model::now_iso8601();
    let record = ExecutionRecord {
        schema_version: 1,
        state: ExecutionState::Running,
        owner_pid: std::process::id(),
        leader_pid,
        process_group_id: leader_pid,
        started_at: now.clone(),
        heartbeat_at: now,
        settled_at: None,
    };
    crate::atomic_write::atomic_write(path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(())
}

impl ExecutionHeartbeat {
    pub fn start(transcript: &Path, leader_pid: u32) -> Result<Self> {
        let path = record_path_for_transcript(transcript);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = crate::lease::acquire_blocking(&lock_path(&path))?;
        if path.exists() {
            let record: ExecutionRecord = serde_json::from_slice(&std::fs::read(&path)?)?;
            anyhow::ensure!(
                record.state != ExecutionState::Canceled,
                "local execution was canceled after launch"
            );
        }
        write_running_record(&path, leader_pid)?;
        Ok(Self {
            path,
            last_write: Instant::now(),
        })
    }

    pub fn heartbeat(&mut self) -> Result<()> {
        if self.last_write.elapsed() < HEARTBEAT_INTERVAL {
            return Ok(());
        }
        mutate_record(&self.path, |record| {
            if record.state == ExecutionState::Running {
                record.heartbeat_at = crate::work_model::now_iso8601();
            }
            Ok(())
        })?;
        self.last_write = Instant::now();
        Ok(())
    }

    pub fn settle(&mut self) -> Result<()> {
        mutate_record(&self.path, |record| {
            if record.state == ExecutionState::Running {
                record.state = ExecutionState::Settled;
                record.settled_at = Some(crate::work_model::now_iso8601());
            }
            Ok(())
        })
    }
}

pub fn execution_is_live(path: &Path) -> Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("reading local execution record"),
    };
    let record: ExecutionRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing local execution record {}", path.display()))?;
    if record.state != ExecutionState::Running {
        return Ok(false);
    }
    // The inherited lock alone proves that part of the original launch still
    // exists. A damaged group field must keep recovery blocked rather than make
    // the Task look reclaimable.
    identity_is_held(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Canceled,
    AlreadyStopped,
}

pub fn cancel(path: &Path) -> Result<CancelOutcome> {
    let _lock = crate::lease::acquire_blocking(&lock_path(path))
        .with_context(|| format!("locking local execution record {}", path.display()))?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading local execution record {}", path.display()))?;
    let mut record: ExecutionRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing local execution record {}", path.display()))?;
    if !matches!(
        record.state,
        ExecutionState::Launching | ExecutionState::Running
    ) {
        return Ok(CancelOutcome::AlreadyStopped);
    }

    if !identity_is_held(path)? {
        record.state = ExecutionState::Canceled;
        record.settled_at = Some(crate::work_model::now_iso8601());
        crate::atomic_write::atomic_write(path, &serde_json::to_vec_pretty(&record)?)?;
        return Ok(CancelOutcome::AlreadyStopped);
    }
    if !recorded_identity_is_signalable(path, &record)? {
        bail!(
            "local execution identity is still held, but leader {} and process group {} do not match; refusing to signal or recover the Task",
            record.leader_pid,
            record.process_group_id
        );
    }

    signal_process_group(record.process_group_id, libc::SIGTERM)?;
    if !wait_for_identity_release(path, TERMINATION_GRACE)? {
        signal_process_group(record.process_group_id, libc::SIGKILL)?;
        if !wait_for_identity_release(path, TERMINATION_GRACE)? {
            bail!(
                "local coder process group {} did not stop",
                record.process_group_id
            );
        }
    }
    record.state = ExecutionState::Canceled;
    record.settled_at = Some(crate::work_model::now_iso8601());
    crate::atomic_write::atomic_write(path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(CancelOutcome::Canceled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn failed_launch_settles_owned_process_tree() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("stdout.log");
        prepare_launch(&transcript).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30 & echo ready; wait"])
            .stdout(std::process::Stdio::piped());
        let mut child = spawn(&mut command, &transcript).unwrap();
        let pid = child.id();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        assert!(identity_is_held(&record_path_for_transcript(&transcript)).unwrap());
        settle_failed_launch(child, &transcript).unwrap();
        assert!(!identity_is_held(&record_path_for_transcript(&transcript)).unwrap());
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn liveness_probe_does_not_create_identity_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("execution.json");
        let record = ExecutionRecord {
            schema_version: 1,
            state: ExecutionState::Running,
            owner_pid: std::process::id(),
            leader_pid: std::process::id(),
            process_group_id: std::process::id(),
            started_at: "started".into(),
            heartbeat_at: "heartbeat".into(),
            settled_at: None,
        };
        let bytes = serde_json::to_vec_pretty(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(!execution_is_live(&path).unwrap());
        assert!(!identity_lock_path(&path).exists());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let missing = dir.path().join("missing/execution.json");
        assert!(!identity_is_held(&missing).unwrap());
        assert!(!execution_is_live(&missing).unwrap());
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn liveness_probe_observes_existing_held_identity_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let path = record_path_for_transcript(&transcript);
        let lock = identity_lock_path(&path);
        std::fs::write(&lock, b"preserve identity bytes").unwrap();
        let mut command = Command::new("/bin/cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .process_group(0);
        configure_identity_lock(&mut command, &transcript).unwrap();
        let mut child = command.spawn().unwrap();
        let record = ExecutionRecord {
            schema_version: 1,
            state: ExecutionState::Running,
            owner_pid: std::process::id(),
            leader_pid: child.id(),
            process_group_id: child.id(),
            started_at: "started".into(),
            heartbeat_at: "heartbeat".into(),
            settled_at: None,
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).unwrap();
        let live = execution_is_live(&path);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(live.unwrap());
        assert_eq!(std::fs::read(&lock).unwrap(), b"preserve identity bytes");
        assert!(!execution_is_live(&path).unwrap());
    }

    #[test]
    fn cancel_terminates_owned_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        command.process_group(0);
        let path = record_path_for_transcript(&transcript);
        configure_identity_lock(&mut command, &transcript).unwrap();
        let mut child = command.spawn().unwrap();
        let _heartbeat = ExecutionHeartbeat::start(&transcript, child.id()).unwrap();

        assert!(
            identity_is_held(&path).unwrap(),
            "child must inherit identity"
        );
        assert!(
            process_group_exists(child.id()).unwrap(),
            "child group must exist"
        );
        assert_eq!(cancel(&path).unwrap(), CancelOutcome::Canceled);
        assert!(!execution_is_live(&path).unwrap());
        let _ = child.wait();
    }

    #[test]
    fn stale_record_never_signals_reused_identity() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        command.process_group(0);
        let path = record_path_for_transcript(&transcript);
        configure_identity_lock(&mut command, &transcript).unwrap();
        let mut child = command.spawn().unwrap();
        let heartbeat = ExecutionHeartbeat::start(&transcript, child.id()).unwrap();
        // Model a stale or corrupted record by pointing it at this test process's
        // group while the original child continues to hold the identity lock. The
        // two independent identity checks no longer agree, so cancellation must not
        // signal either process group.
        mutate_record(&path, |record| {
            record.process_group_id = std::process::id();
            Ok(())
        })
        .unwrap();

        assert!(execution_is_live(&path).unwrap());
        let error = cancel(&path).unwrap_err();
        assert!(error.to_string().contains("refusing to signal"));
        assert!(child.try_wait().unwrap().is_none());
        drop(heartbeat);
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }

    #[test]
    fn identity_lock_contention_fails_a_new_spawn_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let mut first = Command::new("/bin/sh");
        first.arg("-c").arg("sleep 30");
        first.process_group(0);
        configure_identity_lock(&mut first, &transcript).unwrap();
        let mut first = first.spawn().unwrap();

        let mut second = Command::new("/usr/bin/true");
        second.process_group(0);
        configure_identity_lock(&mut second, &transcript).unwrap();
        let started = Instant::now();
        assert!(second.spawn().is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        unsafe { libc::kill(-(first.id() as i32), libc::SIGKILL) };
        let _ = first.wait();
    }
}
