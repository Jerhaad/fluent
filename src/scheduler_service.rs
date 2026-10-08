//! Persistent scheduler service foundation: checkout identity, user registry,
//! service-manager boundary, versioned socket health, immutable Attempt
//! execution requests, sequenced events, and a private executor contract.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

/// Integer protocol generation. Bump when the request/response schema changes
/// incompatibly.
pub const PROTOCOL_GENERATION: u32 = 1;

/// Maximum size of a single protocol frame in bytes (64 KiB). Frames that
/// declare or carry more than this many bytes are rejected before allocation.
pub const MAX_FRAME_SIZE: usize = 64 * 1024;

static SERVICE_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Install the scheduler service's graceful signal boundary before identity,
/// registry, or executable hashing can delay entry into the service loop.
pub fn prepare_run_signal_handler() {
    SERVICE_SHUTDOWN.store(false, Ordering::Release);
    let _ = ctrlc::set_handler(|| {
        SERVICE_SHUTDOWN.store(true, Ordering::Release);
    });
}

// ─────────────────────────────────────────────────
// Filesystem layout
// ─────────────────────────────────────────────────

/// Private user-state root: `~/.config/fluent/scheduler/`.
pub fn service_state_root(home_dir: &Path) -> PathBuf {
    home_dir.join(".config/fluent/scheduler")
}

/// Resolve the scheduler service's writable home for this process.
///
/// Production always uses the operator's home. A test-support binary with the
/// explicit host-preflight seam runs inside the configured Tester sandbox, which
/// grants the fixture checkout but intentionally withholds the operator's home.
/// Keep that service state inside the fixture so parallel scheduler tests neither
/// escape their sandbox nor share a singleton socket.
pub fn runtime_home(_project_root: &Path) -> Result<PathBuf> {
    #[cfg(feature = "test-support")]
    if std::env::var_os("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT").is_some() {
        // Keep state relative to the fixture checkout so parallel tests remain
        // isolated without writing the operator's home.
        return Ok(PathBuf::from(".fluent/work/scheduler/service-home"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("scheduler service requires a home directory")
}

fn registry_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("registry.json")
}

/// Unix socket path for the scheduler service.
pub fn socket_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("service.sock")
}

/// Path of the managed (staged) service executable.
pub fn managed_executable_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("service")
}

fn desired_build_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("build/desired.json")
}

fn observed_build_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("build/observed.json")
}

fn checkout_identity_path(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/identity")
}

fn requests_dir(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/requests")
}

fn events_dir(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/events")
}

fn dispatch_tokens_dir(project_root: &Path) -> PathBuf {
    project_root.join(".fluent/work/scheduler/dispatches")
}

fn registry_lock_path(home_dir: &Path) -> PathBuf {
    service_state_root(home_dir).join("registry.lock")
}

// ─────────────────────────────────────────────────
// Advisory lock helpers
// ─────────────────────────────────────────────────

struct RegistryLock {
    _file: fs::File,
}

fn acquire_registry_lock(home_dir: &Path) -> Result<RegistryLock> {
    let state_root = service_state_root(home_dir);
    fs::create_dir_all(&state_root)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(registry_lock_path(home_dir))?;
    flock(&file, FlockOperation::LockExclusive)?;
    Ok(RegistryLock { _file: file })
}

struct RequestLock {
    _file: fs::File,
}

fn request_lock_path(project_root: &Path, request_id: &str) -> PathBuf {
    requests_dir(project_root).join(format!("{request_id}.lock"))
}

fn acquire_request_lock(project_root: &Path, request_id: &str) -> Result<RequestLock> {
    let dir = requests_dir(project_root);
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(request_lock_path(project_root, request_id))?;
    flock(&file, FlockOperation::LockExclusive)?;
    Ok(RequestLock { _file: file })
}

struct DispatchLock {
    _file: fs::File,
}

fn dispatch_lock_path(project_root: &Path, request_id: &str) -> PathBuf {
    dispatch_tokens_dir(project_root).join(format!("{request_id}.lock"))
}

fn acquire_dispatch_lock(project_root: &Path, request_id: &str) -> Result<DispatchLock> {
    let dir = dispatch_tokens_dir(project_root);
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(dispatch_lock_path(project_root, request_id))?;
    flock(&file, FlockOperation::LockExclusive)?;
    Ok(DispatchLock { _file: file })
}

// ─────────────────────────────────────────────────
// Checkout identity
// ─────────────────────────────────────────────────

/// A stable, random identifier for one checkout of a project. Stored at
/// `.fluent/work/scheduler/identity` and never changes after creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutIdentity(pub String);

/// Assign a checkout identity to the project, creating one if absent. Returns
/// the existing identity on repeated calls. The file lives under a directory
/// that should be git-ignored.
pub fn assign_checkout_identity(project_root: &Path) -> Result<CheckoutIdentity> {
    let path = checkout_identity_path(project_root);
    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read checkout identity {}", path.display()))?;
        let s = raw.trim().to_string();
        if !s.is_empty() {
            return Ok(CheckoutIdentity(s));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let identity = generate_random_hex(16)?;
    atomic_write(&path, identity.as_bytes())?;
    Ok(CheckoutIdentity(identity))
}

// ─────────────────────────────────────────────────
// User registry
// ─────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    /// canonical checkout path → checkout identity hex string
    checkouts: HashMap<String, String>,
}

/// Atomically pair this checkout's canonical path with its identity in the
/// user-private registry at `~/.config/fluent/scheduler/registry.json`.
pub fn register_checkout(
    home_dir: &Path,
    project_root: &Path,
    identity: &CheckoutIdentity,
) -> Result<()> {
    let canonical = project_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", project_root.display()))?;
    let state_root = service_state_root(home_dir);
    fs::create_dir_all(&state_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700))?;
    }
    let _lock = acquire_registry_lock(home_dir)?;
    let mut reg = read_registry_raw(home_dir).unwrap_or_default();
    reg.checkouts
        .insert(canonical.to_string_lossy().into_owned(), identity.0.clone());
    atomic_write(&registry_path(home_dir), &serde_json::to_vec_pretty(&reg)?)?;
    Ok(())
}

/// Remove a checkout from the user registry.
pub fn unregister_checkout(home_dir: &Path, project_root: &Path) -> Result<()> {
    let canonical = project_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", project_root.display()))?;
    let _lock = acquire_registry_lock(home_dir)?;
    let mut reg = read_registry_raw(home_dir).unwrap_or_default();
    reg.checkouts
        .remove(&canonical.to_string_lossy().into_owned());
    let reg_path = registry_path(home_dir);
    if let Some(parent) = reg_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&reg_path, &serde_json::to_vec_pretty(&reg)?)?;
    Ok(())
}

/// Read all registered checkouts. Returns an empty map if the registry is
/// absent.
pub fn read_registry(home_dir: &Path) -> Result<HashMap<PathBuf, CheckoutIdentity>> {
    let reg = read_registry_raw(home_dir).unwrap_or_default();
    Ok(reg
        .checkouts
        .into_iter()
        .map(|(path, id)| (PathBuf::from(path), CheckoutIdentity(id)))
        .collect())
}

fn read_registry_raw(home_dir: &Path) -> Result<RegistryFile> {
    let path = registry_path(home_dir);
    let bytes = fs::read(&path).with_context(|| format!("read registry {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse registry {}", path.display()))
}

// ─────────────────────────────────────────────────
// Build identity
// ─────────────────────────────────────────────────

/// Build identity: version string + hex-encoded SHA-256 of the binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub hash: String,
}

/// Compute the immutable build identity of an executable without staging it.
pub fn executable_build_identity(executable: &Path) -> Result<BuildIdentity> {
    let bytes = fs::read(executable)
        .with_context(|| format!("read executable {}", executable.display()))?;
    Ok(build_identity_for_bytes(&bytes))
}

fn build_identity_for_bytes(bytes: &[u8]) -> BuildIdentity {
    use sha2::{Digest, Sha256};
    BuildIdentity {
        version: crate::version::version_tag(),
        hash: format!("{:x}", Sha256::digest(&bytes)),
    }
}

/// Copy the source executable to the managed service path atomically. Returns
/// the build identity of the staged binary. An interrupted copy leaves no
/// partial target because it writes through a temp file.
pub fn stage_executable(source_exe: &Path, home_dir: &Path) -> Result<BuildIdentity> {
    let bytes = fs::read(source_exe)
        .with_context(|| format!("read source executable {}", source_exe.display()))?;
    let build = build_identity_for_bytes(&bytes);
    let dest = managed_executable_path(home_dir);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&dest, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o700))?;
    }
    Ok(build)
}

/// Record the desired build identity (what we intend the service to run).
pub fn record_desired_build(home_dir: &Path, build: &BuildIdentity) -> Result<()> {
    let path = desired_build_path(home_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(build)?)?;
    Ok(())
}

/// Record the observed build identity (the last known running version).
pub fn record_observed_build(home_dir: &Path, build: &BuildIdentity) -> Result<()> {
    let path = observed_build_path(home_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(build)?)?;
    Ok(())
}

pub fn read_desired_build(home_dir: &Path) -> Result<Option<BuildIdentity>> {
    read_optional_build(desired_build_path(home_dir))
}

pub fn read_observed_build(home_dir: &Path) -> Result<Option<BuildIdentity>> {
    read_optional_build(observed_build_path(home_dir))
}

fn read_optional_build(path: PathBuf) -> Result<Option<BuildIdentity>> {
    match fs::read(&path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parse build state {}", path.display())
            })?))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read build state {}", path.display())),
    }
}

// ─────────────────────────────────────────────────
// ServiceManager trait
// ─────────────────────────────────────────────────

/// Platform-neutral interface for managing the scheduler service process.
/// The production LaunchAgent implementation belongs to the `init-status-release`
/// Work Item. Only a `FakeServiceManager` is provided here.
pub trait ServiceManager: Send + Sync {
    fn install_or_update(
        &self,
        executable: &Path,
        build: &BuildIdentity,
        sock_path: &Path,
    ) -> Result<()>;
    fn enable(&self) -> Result<()>;
    fn disable(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    /// Drain: stop accepting new work and wait for in-flight work before
    /// stopping. The fake drains immediately (no in-flight work).
    fn drain(&self) -> Result<()>;
    fn is_installed(&self) -> bool;
    fn is_enabled(&self) -> bool;
    fn is_running(&self) -> bool;
}

// ─────────────────────────────────────────────────
// FakeServiceManager
// ─────────────────────────────────────────────────

struct FakeInner {
    installed: bool,
    enabled: bool,
    running: bool,
    build: Option<BuildIdentity>,
    sock_path: Option<PathBuf>,
}

/// In-process fake for tests. Tracks lifecycle state without binding real
/// sockets or spawning platform daemons. Use `FakeSocketListener` when the
/// test needs an actual health exchange over a Unix socket.
pub struct FakeServiceManager {
    inner: Mutex<FakeInner>,
}

impl FakeServiceManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeInner {
                installed: false,
                enabled: false,
                running: false,
                build: None,
                sock_path: None,
            }),
        }
    }

    pub fn installed_build(&self) -> Option<BuildIdentity> {
        self.inner.lock().unwrap().build.clone()
    }

    pub fn installed_sock_path(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().sock_path.clone()
    }
}

impl Default for FakeServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceManager for FakeServiceManager {
    fn install_or_update(
        &self,
        _executable: &Path,
        build: &BuildIdentity,
        sock_path: &Path,
    ) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        s.installed = true;
        s.build = Some(build.clone());
        s.sock_path = Some(sock_path.to_path_buf());
        Ok(())
    }

    fn enable(&self) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        if !s.installed {
            bail!("cannot enable: service not installed");
        }
        s.enabled = true;
        Ok(())
    }

    fn disable(&self) -> Result<()> {
        self.inner.lock().unwrap().enabled = false;
        Ok(())
    }

    fn start(&self) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        if !s.installed {
            bail!("cannot start: service not installed");
        }
        s.running = true;
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.inner.lock().unwrap().running = false;
        Ok(())
    }

    fn drain(&self) -> Result<()> {
        self.stop()
    }

    fn is_installed(&self) -> bool {
        self.inner.lock().unwrap().installed
    }
    fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled
    }
    fn is_running(&self) -> bool {
        self.inner.lock().unwrap().running
    }
}

// ─────────────────────────────────────────────────
// FakeSocketListener
// ─────────────────────────────────────────────────

/// A no-op socket listener for testing the health exchange protocol in
/// environments that permit Unix domain socket creation.
pub struct FakeSocketListener {
    sock_path: PathBuf,
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeSocketListener {
    /// Bind a Unix socket at `sock_path` and start serving health responses.
    pub fn start(sock_path: &Path, build: BuildIdentity) -> Result<Self> {
        let _ = fs::remove_file(sock_path);
        if let Some(parent) = sock_path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let listener = UnixListener::bind(sock_path)
            .with_context(|| format!("bind socket {}", sock_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(sock_path, fs::Permissions::from_mode(0o600))?;
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag = stop_flag.clone();
        let handle = std::thread::spawn(move || {
            run_fake_listener(listener, build, flag);
        });
        Ok(Self {
            sock_path: sock_path.to_path_buf(),
            stop_flag,
            handle: Some(handle),
        })
    }
}

impl Drop for FakeSocketListener {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ─────────────────────────────────────────────────
// Socket protocol
// ─────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SocketRequest {
    Health {
        generation: u32,
    },
    SubmitExecution {
        generation: u32,
        build: BuildIdentity,
        project_root: PathBuf,
        request: AttemptExecutionRequest,
    },
    ExecutionStatus {
        generation: u32,
        build: BuildIdentity,
        project_root: PathBuf,
        request_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SocketResponse {
    HealthOk {
        generation: u32,
        build: BuildIdentity,
    },
    ExecutionSubmitted {
        generation: u32,
        token: DispatchToken,
    },
    ExecutionStatus {
        generation: u32,
        request_id: String,
        work_item_id: String,
        dispatch_id: String,
        status: String,
        attempt_id: Option<String>,
    },
    Error {
        generation: u32,
        message: String,
    },
}

/// The payload of a successful health exchange.
#[derive(Debug, Clone)]
pub struct HealthResponse {
    pub generation: u32,
    pub build: BuildIdentity,
}

/// Authoritative state of the real queue dispatch created for one service
/// request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionStatus {
    pub request_id: String,
    pub work_item_id: String,
    pub dispatch_id: String,
    pub status: String,
    pub attempt_id: Option<String>,
}

/// Send a health request over the Unix socket and return the response.
pub fn send_health_request(sock_path: &Path) -> Result<HealthResponse> {
    let mut stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connect to {}", sock_path.display()))?;
    let req = SocketRequest::Health {
        generation: PROTOCOL_GENERATION,
    };
    frame_write(&mut stream, &serde_json::to_vec(&req)?)?;
    let resp_bytes = frame_read(&mut stream)?;
    match serde_json::from_slice::<SocketResponse>(&resp_bytes).context("parse health response")? {
        SocketResponse::HealthOk { generation, build } => Ok(HealthResponse { generation, build }),
        SocketResponse::Error { message, .. } => bail!("scheduler service: {message}"),
        _ => bail!("scheduler service returned an unexpected health response"),
    }
}

/// Submit one immutable execution request through the versioned scheduler
/// service boundary. Only the service writes request and dispatch storage.
pub fn send_execution_request(
    sock_path: &Path,
    project_root: &Path,
    request: &AttemptExecutionRequest,
) -> Result<DispatchToken> {
    let build = send_health_request(sock_path)?.build;
    let response = exchange(
        sock_path,
        &SocketRequest::SubmitExecution {
            generation: PROTOCOL_GENERATION,
            build,
            project_root: project_root.to_path_buf(),
            request: request.clone(),
        },
    )?;
    match response {
        SocketResponse::ExecutionSubmitted { generation, token }
            if generation == PROTOCOL_GENERATION =>
        {
            Ok(token)
        }
        SocketResponse::Error { message, .. } => bail!("scheduler service: {message}"),
        _ => bail!("scheduler service returned an unexpected execution response"),
    }
}

/// Read scheduler-owned execution status through the versioned service API.
pub fn send_execution_status(
    sock_path: &Path,
    project_root: &Path,
    request_id: &str,
) -> Result<ExecutionStatus> {
    let build = send_health_request(sock_path)?.build;
    let response = exchange(
        sock_path,
        &SocketRequest::ExecutionStatus {
            generation: PROTOCOL_GENERATION,
            build,
            project_root: project_root.to_path_buf(),
            request_id: request_id.to_owned(),
        },
    )?;
    match response {
        SocketResponse::ExecutionStatus {
            generation,
            request_id,
            work_item_id,
            dispatch_id,
            status,
            attempt_id,
        } if generation == PROTOCOL_GENERATION => Ok(ExecutionStatus {
            request_id,
            work_item_id,
            dispatch_id,
            status,
            attempt_id,
        }),
        SocketResponse::Error { message, .. } => bail!("scheduler service: {message}"),
        _ => bail!("scheduler service returned an unexpected status response"),
    }
}

fn exchange(sock_path: &Path, request: &SocketRequest) -> Result<SocketResponse> {
    let mut stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connect to {}", sock_path.display()))?;
    exchange_stream(&mut stream, request)
}

fn exchange_stream(stream: &mut UnixStream, request: &SocketRequest) -> Result<SocketResponse> {
    frame_write(stream, &serde_json::to_vec(request)?)?;
    serde_json::from_slice(&frame_read(stream)?).context("parse scheduler response")
}

fn frame_write(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame payload {} bytes exceeds maximum {} bytes",
                payload.len(),
                MAX_FRAME_SIZE
            ),
        ));
    }
    let len = payload.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(payload)
}

fn frame_read(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer declared frame length {} bytes exceeds maximum {} bytes",
                len, MAX_FRAME_SIZE
            ),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

/// Read a raw protocol frame from a Unix stream.
///
/// Exposed only under the `test-support` feature so integration tests can
/// drive oversized-frame rejection through a `UnixStream::pair()` without
/// binding a socket path.
#[cfg(feature = "test-support")]
pub fn frame_read_test_support(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    frame_read(stream)
}

/// Write a raw protocol frame to a Unix stream.
///
/// Exposed only under the `test-support` feature for the same reason as
/// `frame_read_test_support`.
#[cfg(feature = "test-support")]
pub fn frame_write_test_support(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    frame_write(stream, payload)
}

fn run_fake_listener(listener: UnixListener, build: BuildIdentity, stop_flag: Arc<AtomicBool>) {
    for stream_result in listener.incoming() {
        if stop_flag.load(Ordering::Acquire) {
            break;
        }
        let Ok(mut stream) = stream_result else {
            continue;
        };
        if stop_flag.load(Ordering::Acquire) {
            break;
        }
        let _ = serve_health_connection(&mut stream, &build);
    }
}

fn serve_health_connection(stream: &mut UnixStream, build: &BuildIdentity) -> Result<()> {
    let payload = frame_read(stream)?;
    let resp = match serde_json::from_slice::<SocketRequest>(&payload) {
        Ok(SocketRequest::Health { generation }) if generation == PROTOCOL_GENERATION => {
            SocketResponse::HealthOk {
                generation: PROTOCOL_GENERATION,
                build: build.clone(),
            }
        }
        Ok(_) => SocketResponse::Error {
            generation: PROTOCOL_GENERATION,
            message: "fake scheduler listener supports health requests only".into(),
        },
        Err(error) => SocketResponse::Error {
            generation: PROTOCOL_GENERATION,
            message: format!("invalid scheduler request: {error}"),
        },
    };
    frame_write(stream, &serde_json::to_vec(&resp)?)?;
    Ok(())
}

/// Production scheduler service state. It validates every project and build,
/// admits requests to the real durable queue, and owns capacity-limited
/// Attempt workers for every registered checkout.
pub struct SchedulerService {
    home_dir: PathBuf,
    build: BuildIdentity,
    runner: Arc<dyn crate::scheduler::AttemptRunner>,
    workers: Vec<ServiceWorker>,
    coordinators: HashMap<PathBuf, crate::scheduler::CoordinatorLease>,
    prepared_projects: HashSet<PathBuf>,
    stopping_projects: HashSet<PathBuf>,
    prepare_host_execution: bool,
    reused_coordinator: Option<crate::scheduler::CoordinatorMetadata>,
    poll_failures: Vec<String>,
}

struct ServiceWorker {
    project: PathBuf,
    token: crate::queue::DispatchToken,
    handle: JoinHandle<Result<()>>,
}

impl SchedulerService {
    pub fn new(
        home_dir: &Path,
        build: BuildIdentity,
        runner: Arc<dyn crate::scheduler::AttemptRunner>,
    ) -> Self {
        Self {
            home_dir: home_dir.to_path_buf(),
            build,
            runner,
            workers: Vec::new(),
            coordinators: HashMap::new(),
            prepared_projects: HashSet::new(),
            stopping_projects: HashSet::new(),
            prepare_host_execution: false,
            reused_coordinator: None,
            poll_failures: Vec::new(),
        }
    }

    fn new_production(
        home_dir: &Path,
        build: BuildIdentity,
        runner: Arc<dyn crate::scheduler::AttemptRunner>,
    ) -> Self {
        let mut service = Self::new(home_dir, build, runner);
        service.prepare_host_execution = true;
        service
    }

    /// Serve one bounded protocol exchange.
    pub fn serve_connection(&mut self, stream: &mut UnixStream) -> Result<()> {
        let payload = frame_read(stream)?;
        let response = match serde_json::from_slice::<SocketRequest>(&payload) {
            Ok(SocketRequest::Health { generation }) if generation == PROTOCOL_GENERATION => {
                SocketResponse::HealthOk {
                    generation: PROTOCOL_GENERATION,
                    build: self.build.clone(),
                }
            }
            Ok(SocketRequest::SubmitExecution {
                generation,
                build,
                project_root,
                request,
            }) if generation == PROTOCOL_GENERATION => {
                let submission = (|| {
                    let project = self.validate_project(&project_root, &build)?;
                    persist_request(&project, &request)?;
                    let token = submit_dispatch(&project, &request)?;
                    Ok::<_, anyhow::Error>((project, token))
                })();
                match submission {
                    Ok((project, token)) => {
                        self.stopping_projects.remove(&project);
                        SocketResponse::ExecutionSubmitted {
                            generation: PROTOCOL_GENERATION,
                            token,
                        }
                    }
                    Err(error) => SocketResponse::Error {
                        generation: PROTOCOL_GENERATION,
                        message: error.to_string(),
                    },
                }
            }
            Ok(SocketRequest::ExecutionStatus {
                generation,
                build,
                project_root,
                request_id,
            }) if generation == PROTOCOL_GENERATION => {
                match self
                    .validate_project(&project_root, &build)
                    .and_then(|project| execution_status(&project, &request_id))
                {
                    Ok(status) => SocketResponse::ExecutionStatus {
                        generation: PROTOCOL_GENERATION,
                        request_id: status.request_id,
                        work_item_id: status.work_item_id,
                        dispatch_id: status.dispatch_id,
                        status: status.status,
                        attempt_id: status.attempt_id,
                    },
                    Err(error) => SocketResponse::Error {
                        generation: PROTOCOL_GENERATION,
                        message: error.to_string(),
                    },
                }
            }
            Ok(_) => SocketResponse::Error {
                generation: PROTOCOL_GENERATION,
                message: "unsupported scheduler protocol generation".into(),
            },
            Err(error) => SocketResponse::Error {
                generation: PROTOCOL_GENERATION,
                message: format!("invalid scheduler request: {error}"),
            },
        };
        frame_write(stream, &serde_json::to_vec(&response)?)?;
        Ok(())
    }

    fn validate_project(&self, project_root: &Path, build: &BuildIdentity) -> Result<PathBuf> {
        if build != &self.build {
            bail!(
                "scheduler request build {} ({}) does not match service build {} ({})",
                build.version,
                build.hash,
                self.build.version,
                self.build.hash
            );
        }
        if let Some(desired) = read_desired_build(&self.home_dir)? {
            if desired != self.build {
                bail!(
                    "scheduler service build {} ({}) is stale; desired build is {} ({})",
                    self.build.version,
                    self.build.hash,
                    desired.version,
                    desired.hash
                );
            }
        }
        let canonical = project_root.canonicalize().with_context(|| {
            format!("canonicalize scheduler project {}", project_root.display())
        })?;
        let registry = read_registry(&self.home_dir)?;
        let expected = registry.get(&canonical).with_context(|| {
            format!(
                "scheduler project {} is not registered",
                canonical.display()
            )
        })?;
        let actual = fs::read_to_string(checkout_identity_path(&canonical))
            .with_context(|| format!("read checkout identity for {}", canonical.display()))?;
        if actual.trim() != expected.0 {
            bail!(
                "scheduler project {} has stale checkout identity",
                canonical.display()
            );
        }
        Ok(canonical)
    }

    /// Fill every registered project's configured capacity with claims from
    /// the real queue and launch those claims on service-owned workers.
    pub fn poll_once(&mut self) -> Result<()> {
        self.poll_failures.clear();
        self.reconcile_stops()?;
        let mut projects = read_registry(&self.home_dir)?
            .into_keys()
            .collect::<Vec<_>>();
        projects.sort();
        for project in projects {
            if self.stopping_projects.contains(&project) {
                continue;
            }
            if let Err(error) = self.poll_project(&project) {
                if error.is::<crate::os::HostSandboxPreflightError>() {
                    return Err(error).with_context(|| {
                        format!("project {} prepare host execution", project.display())
                    });
                }
                let diagnostic = format!("project {} poll: {error:#}", project.display());
                eprintln!("[scheduler] {diagnostic}");
                self.poll_failures.push(diagnostic);
            }
        }
        Ok(())
    }

    /// Diagnostics from the most recent poll, including its project context.
    pub fn last_poll_failures(&self) -> &[String] {
        &self.poll_failures
    }

    fn poll_project(&mut self, project: &Path) -> Result<()> {
        if !self.coordinators.contains_key(project) {
            match crate::scheduler::start_or_reuse(&project, &self.build)
                .context("start coordinator")?
            {
                crate::scheduler::CoordinatorStart::Elected(coordinator) => {
                    if self.prepare_host_execution {
                        crate::scheduler::prepare_service_execution(&project)
                            .context("prepare execution")?;
                        self.prepared_projects.insert(project.to_path_buf());
                    }
                    self.coordinators.insert(project.to_path_buf(), coordinator);
                }
                crate::scheduler::CoordinatorStart::Reused(metadata) => {
                    self.reused_coordinator = Some(metadata);
                    return Ok(());
                }
            }
        }
        crate::scheduler::recover_and_reconcile(project)
            .with_context(|| format!("recover project {}", project.display()))?;
        let capacity =
            crate::scheduler::configured_capacity(&project).context("read scheduler capacity")?;
        while crate::scheduler::project_local_capacity_used(project).context("count live claims")?
            < capacity
        {
            let Some((token, lease)) =
                crate::scheduler::claim_next(project).context("claim next dispatch")?
            else {
                break;
            };
            let runner = self.runner.clone();
            let worker_project = project.to_path_buf();
            let worker_token = token.clone();
            let worker = std::thread::spawn(move || {
                crate::scheduler::run_claimed_dispatch(
                    &worker_project,
                    worker_token,
                    lease,
                    runner.as_ref(),
                )
            });
            self.workers.push(ServiceWorker {
                project: project.to_path_buf(),
                token,
                handle: worker,
            });
        }
        Ok(())
    }

    fn reap_finished(&mut self) -> Result<()> {
        let mut pending = Vec::new();
        for worker in self.workers.drain(..) {
            if worker.handle.is_finished() {
                if let Err(error) = settle_worker(worker) {
                    eprintln!("[scheduler] {error:#}");
                }
            } else {
                pending.push(worker);
            }
        }
        self.workers = pending;
        Ok(())
    }

    fn reconcile_stops(&mut self) -> Result<()> {
        self.reap_finished()?;
        let projects = self.coordinators.keys().cloned().collect::<Vec<_>>();
        for project in projects {
            let stop_requested = match crate::scheduler::service_stop_requested(
                &project,
                self.coordinators
                    .get_mut(&project)
                    .expect("known coordinator"),
            ) {
                Ok(requested) => requested,
                Err(error) => {
                    eprintln!(
                        "[scheduler] project {} check stop: {error:#}",
                        project.display()
                    );
                    continue;
                }
            };
            let has_workers = self.workers.iter().any(|worker| worker.project == project);
            if stop_requested {
                self.stopping_projects.insert(project.clone());
            }
            if self.stopping_projects.contains(&project) && !has_workers {
                if self.prepared_projects.contains(&project) {
                    if let Err(error) = crate::scheduler::finish_service_execution(&project) {
                        eprintln!(
                            "[scheduler] project {} finish execution: {error:#}",
                            project.display()
                        );
                        continue;
                    }
                    self.prepared_projects.remove(&project);
                }
                self.coordinators.remove(&project);
            }
        }
        Ok(())
    }

    /// Join all current workers. Integration tests use this to observe the
    /// durable terminal dispatch after a deterministic fake runner returns.
    pub fn join_workers(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for worker in self.workers.drain(..) {
            if let Err(error) = settle_worker(worker) {
                failures.push(format!("{error:#}"));
            }
        }
        if !failures.is_empty() {
            bail!("{}", failures.join("; "));
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    fn take_reused_coordinator(&mut self) -> Option<crate::scheduler::CoordinatorMetadata> {
        self.reused_coordinator.take()
    }

    #[cfg(feature = "test-support")]
    fn owns_coordinator(&self) -> bool {
        !self.coordinators.is_empty()
    }
}

fn settle_worker(worker: ServiceWorker) -> Result<()> {
    let context = format!(
        "project {} work item {} dispatch {} generation {} attempt {} worker",
        worker.project.display(),
        worker.token.work_item_id,
        worker.token.dispatch_id,
        worker.token.generation,
        worker.token.bound_attempt_id,
    );
    worker
        .handle
        .join()
        .map_err(|_| anyhow::anyhow!("{context} panicked"))?
        .with_context(|| format!("{context} failed"))
}

impl Drop for SchedulerService {
    fn drop(&mut self) {
        let _ = self.join_workers();
        for project in self.prepared_projects.drain() {
            let _ = crate::scheduler::finish_service_execution(&project);
        }
    }
}

/// Serve the production scheduler socket until the owning process exits.
/// The scheduler command runs this listener alongside its queue coordinator.
pub fn run_service(home_dir: &Path, build: BuildIdentity, poll_seconds: u64) -> Result<()> {
    run_service_with_runner(
        home_dir,
        build,
        poll_seconds,
        Arc::new(crate::scheduler::CliAttemptRunner),
        true,
    )
}

/// Run the production listener with an injected external Attempt runner in
/// host tests. The registry, poll loop, claims, and worker settlement are shared.
#[cfg(feature = "test-support")]
pub fn run_service_with_test_runner(
    home_dir: &Path,
    build: BuildIdentity,
    poll_seconds: u64,
    runner: Arc<dyn crate::scheduler::AttemptRunner>,
) -> Result<()> {
    run_service_with_runner(home_dir, build, poll_seconds, runner, false)
}

fn run_service_with_runner(
    home_dir: &Path,
    build: BuildIdentity,
    poll_seconds: u64,
    runner: Arc<dyn crate::scheduler::AttemptRunner>,
    prepare_host_execution: bool,
) -> Result<()> {
    if SERVICE_SHUTDOWN.load(Ordering::Acquire) {
        return Ok(());
    }
    #[cfg(feature = "test-support")]
    if std::env::var_os("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT").is_some() {
        return run_transportless_service(
            home_dir,
            build,
            poll_seconds,
            runner,
            prepare_host_execution,
        );
    }
    let path = socket_path(home_dir);
    if let Ok(health) = send_health_request(&path) {
        eprintln!(
            "[scheduler] reusing live coordinator running {} ({})",
            health.build.version, health.build.hash
        );
        return Ok(());
    }
    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind scheduler service {}", path.display()))?;
    listener.set_nonblocking(true)?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let mut service = SchedulerService::new_production(home_dir, build, runner);
    service.prepare_host_execution = prepare_host_execution;
    let poll_interval = Duration::from_secs(poll_seconds.max(1));
    let mut next_poll = Instant::now();
    loop {
        if SERVICE_SHUTDOWN.load(Ordering::Acquire) {
            return Ok(());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = service.serve_connection(&mut stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("accept scheduler service connection"),
        }
        service.reconcile_stops()?;
        if Instant::now() >= next_poll {
            service.poll_once()?;
            next_poll = Instant::now() + poll_interval;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run the production service coordinator without its Unix socket when the
/// configured test-support sandbox explicitly controls host preflight. The
/// outer Tester sandbox forbids `bind(2)`, while the scheduler behavior under
/// test needs the real registry, host gate, claims, capacity, workers, stop
/// requests, and durable queue reconciliation.
#[cfg(feature = "test-support")]
fn run_transportless_service(
    home_dir: &Path,
    build: BuildIdentity,
    poll_seconds: u64,
    runner: Arc<dyn crate::scheduler::AttemptRunner>,
    prepare_host_execution: bool,
) -> Result<()> {
    let mut service = SchedulerService::new_production(home_dir, build, runner);
    service.prepare_host_execution = prepare_host_execution;
    let poll_interval = Duration::from_secs(poll_seconds.max(1));
    let mut next_poll = Instant::now();
    let mut owned_coordinator = false;
    loop {
        if SERVICE_SHUTDOWN.load(Ordering::Acquire) {
            return Ok(());
        }
        service.reconcile_stops()?;
        if owned_coordinator && !service.owns_coordinator() {
            return Ok(());
        }
        if Instant::now() >= next_poll {
            service.poll_once()?;
            if let Some(metadata) = service.take_reused_coordinator() {
                eprintln!(
                    "[scheduler] reusing live coordinator {} running {} ({})",
                    metadata.instance_id, metadata.build.version, metadata.build.hash
                );
                return Ok(());
            }
            owned_coordinator |= service.owns_coordinator();
            next_poll = Instant::now() + poll_interval;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ─────────────────────────────────────────────────
// Service lifecycle
// ─────────────────────────────────────────────────

/// The outcome of `start_or_reuse_service`.
pub enum ServiceStartOutcome {
    /// A new service instance was started.
    Started(HealthResponse),
    /// An existing healthy instance answered the health check.
    Reused(HealthResponse),
}

/// Set up the service for a project: assign a checkout identity, register it
/// in the user registry, stage the current executable, record the desired
/// build, and install through the service manager. Returns the staged build
/// identity.
pub fn setup_service(
    project_root: &Path,
    home_dir: &Path,
    source_exe: &Path,
    manager: &dyn ServiceManager,
) -> Result<BuildIdentity> {
    let identity = assign_checkout_identity(project_root)?;
    register_checkout(home_dir, project_root, &identity)?;
    let build = stage_executable(source_exe, home_dir)?;
    record_desired_build(home_dir, &build)?;
    let sock = socket_path(home_dir);
    manager.install_or_update(&managed_executable_path(home_dir), &build, &sock)?;
    Ok(build)
}

/// Start the service if not already running, or reuse a healthy existing
/// instance. On a fresh start, records the observed build identity.
pub fn start_or_reuse_service(
    home_dir: &Path,
    build: &BuildIdentity,
    manager: &dyn ServiceManager,
) -> Result<ServiceStartOutcome> {
    let sock = socket_path(home_dir);
    if let Ok(health) = send_health_request(&sock) {
        return Ok(ServiceStartOutcome::Reused(health));
    }
    manager.start()?;
    let health = wait_for_health(&sock, std::time::Duration::from_secs(5))?;
    record_observed_build(home_dir, build)?;
    Ok(ServiceStartOutcome::Started(health))
}

fn wait_for_health(sock_path: &Path, timeout: std::time::Duration) -> Result<HealthResponse> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(health) = send_health_request(sock_path) {
            return Ok(health);
        }
        if std::time::Instant::now() >= deadline {
            bail!("service did not become healthy within {:?}", timeout);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ─────────────────────────────────────────────────
// AttemptExecutionRequest
// ─────────────────────────────────────────────────

/// A frozen, immutable record of one Attempt execution to dispatch. Must be
/// persisted before claim so a crash between persist and launch is recoverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptExecutionRequest {
    pub id: String,
    pub work_item_id: String,
    pub attempt_id: String,
    pub created_at: String,
}

impl AttemptExecutionRequest {
    pub fn new(work_item_id: impl Into<String>, attempt_id: impl Into<String>) -> Result<Self> {
        Ok(Self {
            id: generate_random_hex(16)?,
            work_item_id: work_item_id.into(),
            attempt_id: attempt_id.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

fn request_path(project_root: &Path, request_id: &str) -> PathBuf {
    requests_dir(project_root).join(format!("{request_id}.json"))
}

/// Persist a frozen `AttemptExecutionRequest`. Idempotent: writing the same id
/// twice with identical content succeeds. Rejects a write that would change
/// any field of an already-persisted request.
pub fn persist_request(project_root: &Path, request: &AttemptExecutionRequest) -> Result<()> {
    let path = request_path(project_root, &request.id);
    let _lock = acquire_request_lock(project_root, &request.id)?;
    if path.exists() {
        let bytes =
            fs::read(&path).with_context(|| format!("read existing request {}", path.display()))?;
        let existing: AttemptExecutionRequest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse existing request {}", path.display()))?;
        if existing == *request {
            return Ok(());
        }
        bail!(
            "request {} is already persisted with different content",
            request.id
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(request)?)?;
    Ok(())
}

/// Load a previously persisted `AttemptExecutionRequest` by id.
pub fn load_request(project_root: &Path, request_id: &str) -> Result<AttemptExecutionRequest> {
    let path = request_path(project_root, request_id);
    let bytes = fs::read(&path).with_context(|| format!("read request {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse request {}", path.display()))
}

// ─────────────────────────────────────────────────
// Dispatch boundary
// ─────────────────────────────────────────────────

/// Token returned by a successful idempotent dispatch submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchToken {
    pub request_id: String,
    pub work_item_id: String,
    pub attempt_id: String,
    /// Identity of the real durable queue dispatch admitted for this request.
    pub dispatch_id: String,
    /// True when an identical request replay found its existing dispatch.
    #[serde(default)]
    pub already_queued: bool,
}

fn dispatch_token_path(project_root: &Path, request_id: &str) -> PathBuf {
    dispatch_tokens_dir(project_root).join(format!("{request_id}.json"))
}

/// Submit an `AttemptExecutionRequest` through an exact-bound idempotent
/// dispatch boundary. The same request id always returns the same token.
/// The request must be persisted before calling this function, and the
/// submitted request's id, work_item_id, and attempt_id must exactly match
/// the persisted record. Any existing token is also verified against the
/// persisted request before being returned.
pub fn submit_dispatch(
    project_root: &Path,
    request: &AttemptExecutionRequest,
) -> Result<DispatchToken> {
    let _lock = acquire_dispatch_lock(project_root, &request.id)?;

    // Load and verify the persisted request.
    let persisted_path = request_path(project_root, &request.id);
    if !persisted_path.exists() {
        bail!(
            "request {} must be persisted before dispatch; {} not found",
            request.id,
            persisted_path.display()
        );
    }
    let persisted_bytes = fs::read(&persisted_path)
        .with_context(|| format!("read persisted request {}", persisted_path.display()))?;
    let persisted: AttemptExecutionRequest = serde_json::from_slice(&persisted_bytes)
        .with_context(|| format!("parse persisted request {}", persisted_path.display()))?;

    // Submitted request must match the persisted record on all identity fields.
    if request.id != persisted.id
        || request.work_item_id != persisted.work_item_id
        || request.attempt_id != persisted.attempt_id
    {
        bail!(
            "submitted request does not match persisted request {} \
             (work_item_id or attempt_id mismatch)",
            persisted.id
        );
    }

    let token_path = dispatch_token_path(project_root, &request.id);
    if token_path.exists() {
        let bytes = fs::read(&token_path)
            .with_context(|| format!("read dispatch token {}", token_path.display()))?;
        let token: DispatchToken = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse dispatch token {}", token_path.display()))?;
        // Existing token must be exactly bound to the persisted request.
        if token.request_id != persisted.id
            || token.work_item_id != persisted.work_item_id
            || token.attempt_id != persisted.attempt_id
            || token.dispatch_id.is_empty()
        {
            bail!(
                "existing dispatch token does not match persisted request {} \
                 (token identity mismatch)",
                persisted.id
            );
        }
        let ledger = crate::queue::read_ledger(project_root, &persisted.work_item_id)?
            .context("real queue ledger is missing for persisted scheduler request")?;
        if !ledger
            .dispatches
            .iter()
            .any(|dispatch| dispatch.dispatch_id == token.dispatch_id)
        {
            bail!(
                "real queue dispatch {} is missing for scheduler request {}",
                token.dispatch_id,
                persisted.id
            );
        }
        return Ok(DispatchToken {
            already_queued: true,
            ..token
        });
    }

    crate::queue::add(project_root, &persisted.work_item_id, None)?;
    let dispatch = crate::queue::show(project_root, &persisted.work_item_id)?;
    let token = DispatchToken {
        request_id: persisted.id.clone(),
        work_item_id: persisted.work_item_id.clone(),
        attempt_id: persisted.attempt_id.clone(),
        dispatch_id: dispatch.dispatch_id,
        already_queued: false,
    };
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&token_path, &serde_json::to_vec_pretty(&token)?)?;
    Ok(token)
}

fn execution_status(project_root: &Path, request_id: &str) -> Result<ExecutionStatus> {
    let request = load_request(project_root, request_id)?;
    let token_path = dispatch_token_path(project_root, request_id);
    let token: DispatchToken = serde_json::from_slice(
        &fs::read(&token_path)
            .with_context(|| format!("read dispatch token {}", token_path.display()))?,
    )
    .with_context(|| format!("parse dispatch token {}", token_path.display()))?;
    let ledger = crate::queue::read_ledger(project_root, &request.work_item_id)?
        .context("real queue ledger is missing for scheduler request")?;
    let dispatch = ledger
        .dispatches
        .iter()
        .find(|dispatch| dispatch.dispatch_id == token.dispatch_id)
        .with_context(|| {
            format!(
                "real queue dispatch {} is missing for scheduler request {request_id}",
                token.dispatch_id
            )
        })?;
    Ok(ExecutionStatus {
        request_id: request.id,
        work_item_id: request.work_item_id,
        dispatch_id: dispatch.dispatch_id.clone(),
        status: dispatch.status.as_str().into(),
        attempt_id: dispatch.bound_attempt_id.clone(),
    })
}

// ─────────────────────────────────────────────────
// AttemptEvent
// ─────────────────────────────────────────────────

/// A bounded, sequenced event in the lifecycle of one Attempt execution.
/// Events are appended as NDJSON and replayed in sequence order. Raw coder
/// transcripts are never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptEvent {
    /// Monotonically increasing sequence number within this Attempt's log.
    pub seq: u64,
    /// Event kind string (e.g. `"started"`, `"completed"`, `"failed"`).
    pub kind: String,
    pub attempt_id: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn events_path(project_root: &Path, attempt_id: &str) -> PathBuf {
    events_dir(project_root).join(format!("{attempt_id}.ndjson"))
}

/// Append one event to the Attempt's append-only event log. The caller is
/// responsible for supplying a monotonically increasing `seq` field.
pub fn append_event(project_root: &Path, event: &AttemptEvent) -> Result<()> {
    let path = events_path(project_root, &event.attempt_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open event log {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("append to event log {}", path.display()))?;
    Ok(())
}

/// Replay all persisted events for an Attempt, sorted by sequence number.
pub fn replay_events(project_root: &Path, attempt_id: &str) -> Result<Vec<AttemptEvent>> {
    let path = events_path(project_root, attempt_id);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let mut events: Vec<AttemptEvent> = content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    serde_json::from_str(line)
                        .with_context(|| format!("parse event in {}", path.display()))
                })
                .collect::<Result<_>>()?;
            events.sort_by_key(|e| e.seq);
            Ok(events)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("read event log {}", path.display())),
    }
}

// ─────────────────────────────────────────────────
// Executor contract
// ─────────────────────────────────────────────────

/// A validated execution request. The request is well-formed, durably
/// persisted, and its ids are consistent.
pub struct ValidatedExecutionRequest {
    pub request: AttemptExecutionRequest,
}

/// Validate an `AttemptExecutionRequest` for private executor invocation
/// without launching a coder. Returns `Err` if the request is missing,
/// malformed, or structurally invalid.
pub fn validate_executor_request(
    project_root: &Path,
    request_id: &str,
) -> Result<ValidatedExecutionRequest> {
    let request = load_request(project_root, request_id)?;
    if request.id != request_id {
        bail!(
            "request id mismatch: file contains {:?}, expected {:?}",
            request.id,
            request_id
        );
    }
    if request.work_item_id.is_empty() {
        bail!("work_item_id is empty in request {request_id}");
    }
    if request.attempt_id.is_empty() {
        bail!("attempt_id is empty in request {request_id}");
    }
    if request.created_at.is_empty() {
        bail!("created_at is empty in request {request_id}");
    }
    Ok(ValidatedExecutionRequest { request })
}

// ─────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────

pub(crate) fn generate_random_hex(n_bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; n_bytes];
    let mut f = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    f.read_exact(&mut buf).context("read random bytes")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}
