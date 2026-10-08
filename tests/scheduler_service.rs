use fluent::scheduler_service::{self, BuildIdentity, FakeServiceManager, ServiceManager};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tempfile::TempDir;

// ─────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────

fn make_fake_exe(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-fluent");
    fs::write(&path, b"#!/bin/sh\necho fluent\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn fake_build() -> BuildIdentity {
    BuildIdentity {
        version: "0.1.0-test".to_string(),
        hash: "abc123def456".to_string(),
    }
}

fn make_event(seq: u64, attempt_id: &str, kind: &str) -> fluent::scheduler_service::AttemptEvent {
    fluent::scheduler_service::AttemptEvent {
        seq,
        kind: kind.to_string(),
        attempt_id: attempt_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
}

fn create_execution_ready_work(project: &std::path::Path, id: &str) {
    let item = fluent::work_model::WorkItem {
        id: id.into(),
        title: format!("Execute {id}"),
        authorization: fluent::work_model::ExecutionAuthorization::execution_ready(
            fluent::work_model::ExecutionAuthority::Human,
        ),
        ..Default::default()
    };
    fluent::work_model::WorkModelStore::new(project)
        .create_work_item(&item)
        .unwrap();
}

fn persist_runner_needs_user(project: &std::path::Path, work_item_id: &str, attempt_id: &str) {
    fluent::work_model::WorkModelStore::new(project)
        .mutate_work_item(work_item_id, |item| {
            let attempt = item
                .attempts
                .iter_mut()
                .find(|a| a.id == attempt_id)
                .unwrap();
            attempt.status = fluent::work_model::AttemptStatus::NeedsUser;
            attempt.pause_kind = Some(fluent::work_model::PauseKind::WriterCompletion);
            if let Some(task) = attempt.tasks.first_mut() {
                task.status = fluent::work_model::TaskStatus::NeedsUser;
            }
            Ok(())
        })
        .unwrap();
}

fn registered_service(
    home: &std::path::Path,
    project: &std::path::Path,
    runner: Arc<dyn fluent::scheduler::AttemptRunner>,
) -> scheduler_service::SchedulerService {
    let identity = scheduler_service::assign_checkout_identity(project).unwrap();
    scheduler_service::register_checkout(home, project, &identity).unwrap();
    scheduler_service::SchedulerService::new(home, fake_build(), runner)
}

fn framed_exchange(
    service: &mut scheduler_service::SchedulerService,
    request: serde_json::Value,
) -> serde_json::Value {
    let (mut server, mut client) = UnixStream::pair().unwrap();
    let payload = serde_json::to_vec(&request).unwrap();
    client
        .write_all(&(payload.len() as u32).to_le_bytes())
        .unwrap();
    client.write_all(&payload).unwrap();
    service.serve_connection(&mut server).unwrap();
    let mut length = [0_u8; 4];
    client.read_exact(&mut length).unwrap();
    let mut response = vec![0; u32::from_le_bytes(length) as usize];
    client.read_exact(&mut response).unwrap();
    serde_json::from_slice(&response).unwrap()
}

fn execution_request(
    generation: u32,
    project: &std::path::Path,
    build: &BuildIdentity,
    request_id: &str,
    work_item_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "submit_execution",
        "generation": generation,
        "build": build,
        "project_root": project,
        "request": {
            "id": request_id,
            "work_item_id": work_item_id,
            "attempt_id": format!("requested-{request_id}"),
            "created_at": "2026-08-18T00:00:00Z"
        }
    })
}

#[derive(Default)]
struct NeedsUserRunner {
    calls: Mutex<Vec<String>>,
}

impl fluent::scheduler::AttemptRunner for NeedsUserRunner {
    fn run(
        &self,
        project_root: &std::path::Path,
        work_item_id: &str,
        attempt_id: &str,
        _tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        self.calls.lock().unwrap().push(work_item_id.into());
        persist_runner_needs_user(project_root, work_item_id, attempt_id);
        Ok(fluent::scheduler::AttemptOutcome::NeedsUser)
    }
}

#[derive(Default)]
struct BlockingState {
    started: usize,
    releases: usize,
}

#[derive(Default)]
struct BlockingRunner {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
}

impl BlockingRunner {
    fn wait_for_started(&self, expected: usize) {
        let (lock, changed) = &*self.state;
        let state = lock.lock().unwrap();
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                state.started < expected
            })
            .unwrap();
        assert!(!timeout.timed_out(), "worker did not start");
        assert_eq!(state.started, expected);
    }

    fn release_one(&self) {
        let (lock, changed) = &*self.state;
        lock.lock().unwrap().releases += 1;
        changed.notify_all();
    }
}

impl fluent::scheduler::AttemptRunner for BlockingRunner {
    fn run(
        &self,
        project_root: &std::path::Path,
        work_item_id: &str,
        attempt_id: &str,
        _tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.started += 1;
        changed.notify_all();
        while state.releases == 0 {
            state = changed.wait(state).unwrap();
        }
        state.releases -= 1;
        drop(state);
        persist_runner_needs_user(project_root, work_item_id, attempt_id);
        Ok(fluent::scheduler::AttemptOutcome::NeedsUser)
    }
}

// ─────────────────────────────────────────────────
// Step 1: isolated-home setup — identity, registry, socket path, state root
//
// Note on socket health exchange: Unix domain socket creation is blocked by
// the execution environment sandbox (EPERM on bind). The protocol, framing,
// FakeSocketListener, and send_health_request are implemented in the module
// and exercised by code review. See Untestable note in progress.md.
// ─────────────────────────────────────────────────

#[test]
fn checkout_identity_is_assigned_and_stable() {
    let project = TempDir::new().unwrap();
    let id1 = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    assert!(!id1.0.is_empty(), "identity must be non-empty");

    let id2 = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    assert_eq!(id1, id2, "identity must be stable across calls");
}

#[test]
fn checkout_identity_differs_across_projects() {
    let p1 = TempDir::new().unwrap();
    let p2 = TempDir::new().unwrap();
    let id1 = scheduler_service::assign_checkout_identity(p1.path()).unwrap();
    let id2 = scheduler_service::assign_checkout_identity(p2.path()).unwrap();
    assert_ne!(
        id1, id2,
        "different checkouts must get different identities"
    );
}

#[test]
fn checkout_is_registered_and_read_back() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let identity = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    scheduler_service::register_checkout(home.path(), project.path(), &identity).unwrap();

    let registry = scheduler_service::read_registry(home.path()).unwrap();
    let canonical = project.path().canonicalize().unwrap();
    assert_eq!(
        registry.get(&canonical).unwrap(),
        &identity,
        "registered identity must be retrievable"
    );
}

#[test]
fn checkout_register_unregister_round_trip() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let canonical = project.path().canonicalize().unwrap();
    let identity = scheduler_service::assign_checkout_identity(project.path()).unwrap();

    scheduler_service::register_checkout(home.path(), project.path(), &identity).unwrap();
    let registered = scheduler_service::read_registry(home.path()).unwrap();
    assert_eq!(
        registered.get(&canonical),
        Some(&identity),
        "registered checkout must appear in the registry"
    );

    scheduler_service::unregister_checkout(home.path(), project.path()).unwrap();
    let unregistered = scheduler_service::read_registry(home.path()).unwrap();
    assert!(
        !unregistered.contains_key(&canonical),
        "unregistered checkout must be absent from the registry"
    );
}

#[test]
fn registration_is_atomic_and_overwrites_stale_entry() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let id1 = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    scheduler_service::register_checkout(home.path(), project.path(), &id1).unwrap();

    let id2 = fluent::scheduler_service::CheckoutIdentity("newidentity".to_string());
    scheduler_service::register_checkout(home.path(), project.path(), &id2).unwrap();

    let registry = scheduler_service::read_registry(home.path()).unwrap();
    let canonical = project.path().canonicalize().unwrap();
    assert_eq!(registry.get(&canonical).unwrap(), &id2);
}

#[test]
#[cfg(unix)]
fn service_state_root_is_user_private() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let identity = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    scheduler_service::register_checkout(home.path(), project.path(), &identity).unwrap();

    let root = scheduler_service::service_state_root(home.path());
    let meta = fs::metadata(&root).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "state root must be accessible only by the owner"
    );
}

#[test]
fn service_state_root_is_under_home_not_system_wide() {
    let home = TempDir::new().unwrap();
    let root = scheduler_service::service_state_root(home.path());
    assert!(
        root.starts_with(home.path()),
        "state root must be confined to the user home directory"
    );
}

#[test]
fn socket_path_is_within_user_private_state_root() {
    let home = TempDir::new().unwrap();
    let sock = scheduler_service::socket_path(home.path());
    let root = scheduler_service::service_state_root(home.path());
    assert!(
        sock.starts_with(&root),
        "socket path must be under the user-private state root, not a system-wide path"
    );
}

#[test]
fn fake_service_manager_starts_as_not_installed() {
    let manager = FakeServiceManager::new();
    assert!(!manager.is_installed());
    assert!(!manager.is_enabled());
    assert!(!manager.is_running());
}

#[test]
fn fake_service_manager_install_records_build_and_socket() {
    let home = TempDir::new().unwrap();
    let build = fake_build();
    let sock = scheduler_service::socket_path(home.path());
    let manager = FakeServiceManager::new();

    manager
        .install_or_update(&PathBuf::from("/fake/exe"), &build, &sock)
        .unwrap();

    assert!(manager.is_installed());
    assert_eq!(manager.installed_build().as_ref(), Some(&build));
    assert_eq!(manager.installed_sock_path().as_ref(), Some(&sock));
}

// ─────────────────────────────────────────────────
// Step 2: atomic executable staging, build state, lifecycle transitions
// ─────────────────────────────────────────────────

#[test]
fn executable_is_staged_atomically_and_returns_build_identity() {
    let home = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    let exe = make_fake_exe(src.path());

    let build = scheduler_service::stage_executable(&exe, home.path()).unwrap();
    assert_eq!(
        scheduler_service::executable_build_identity(&exe).unwrap(),
        build,
        "foreground and managed scheduler paths must share build identity"
    );
    assert!(!build.version.is_empty(), "version must be non-empty");
    assert_eq!(build.hash.len(), 64, "SHA-256 hash must be 64 hex chars");

    let staged = scheduler_service::managed_executable_path(home.path());
    assert!(staged.exists(), "staged executable must exist");
    assert_eq!(
        fs::read(&staged).unwrap(),
        fs::read(&exe).unwrap(),
        "staged content must match source"
    );
}

#[test]
fn desired_and_observed_build_identities_are_recorded_and_read() {
    let home = TempDir::new().unwrap();
    let build = fake_build();

    assert!(
        scheduler_service::read_desired_build(home.path())
            .unwrap()
            .is_none()
    );
    assert!(
        scheduler_service::read_observed_build(home.path())
            .unwrap()
            .is_none()
    );

    scheduler_service::record_desired_build(home.path(), &build).unwrap();
    scheduler_service::record_observed_build(home.path(), &build).unwrap();

    assert_eq!(
        scheduler_service::read_desired_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build)
    );
    assert_eq!(
        scheduler_service::read_observed_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build)
    );
}

#[test]
fn service_manager_install_then_enable_disable_drain_transitions() {
    let home = TempDir::new().unwrap();
    let build = fake_build();
    let sock = scheduler_service::socket_path(home.path());
    let manager = FakeServiceManager::new();

    assert!(!manager.is_installed());

    manager
        .install_or_update(&PathBuf::from("/fake/exe"), &build, &sock)
        .unwrap();
    assert!(manager.is_installed());

    manager.enable().unwrap();
    assert!(manager.is_enabled());

    manager.start().unwrap();
    assert!(manager.is_running());

    manager.disable().unwrap();
    assert!(!manager.is_enabled(), "disabled must clear enabled flag");
    assert!(
        manager.is_running(),
        "disable must not stop the running service"
    );

    manager.drain().unwrap();
    assert!(!manager.is_running(), "drain must stop the service");
}

#[test]
fn enable_requires_prior_install() {
    let manager = FakeServiceManager::new();
    assert!(
        manager.enable().is_err(),
        "enable without install must fail"
    );
}

#[test]
fn start_requires_prior_install() {
    let manager = FakeServiceManager::new();
    assert!(manager.start().is_err(), "start without install must fail");
}

#[test]
fn start_is_idempotent_when_already_running() {
    let home = TempDir::new().unwrap();
    let build = fake_build();
    let sock = scheduler_service::socket_path(home.path());
    let manager = FakeServiceManager::new();
    manager
        .install_or_update(&PathBuf::from("/fake/exe"), &build, &sock)
        .unwrap();
    manager.start().unwrap();
    manager.start().unwrap();
    assert!(manager.is_running());
    manager.stop().unwrap();
}

#[test]
fn setup_service_registers_checkout_and_records_desired_build() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    let exe = make_fake_exe(src.path());
    let manager = FakeServiceManager::new();

    let build =
        scheduler_service::setup_service(project.path(), home.path(), &exe, &manager).unwrap();

    let canonical = project.path().canonicalize().unwrap();
    let registry = scheduler_service::read_registry(home.path()).unwrap();
    assert!(
        registry.contains_key(&canonical),
        "checkout must be registered after setup"
    );
    assert!(
        manager.is_installed(),
        "service must be installed after setup"
    );
    assert_eq!(
        scheduler_service::read_desired_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build),
        "desired build must be recorded after setup"
    );
}

#[test]
fn registration_survives_enable_and_disable_transitions() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    let exe = make_fake_exe(src.path());
    let manager = FakeServiceManager::new();

    scheduler_service::setup_service(project.path(), home.path(), &exe, &manager).unwrap();
    manager.enable().unwrap();
    manager.disable().unwrap();

    let canonical = project.path().canonicalize().unwrap();
    let registry = scheduler_service::read_registry(home.path()).unwrap();
    assert!(
        registry.contains_key(&canonical),
        "registration must survive enable/disable"
    );
}

#[test]
fn desired_build_updates_when_executable_changes() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    let manager = FakeServiceManager::new();

    let exe1 = make_fake_exe(src.path());
    let build1 =
        scheduler_service::setup_service(project.path(), home.path(), &exe1, &manager).unwrap();

    let exe2 = src.path().join("fake-fluent-v2");
    fs::write(&exe2, b"#!/bin/sh\necho fluent-v2\n").unwrap();
    let build2 =
        scheduler_service::setup_service(project.path(), home.path(), &exe2, &manager).unwrap();

    assert_ne!(
        build1.hash, build2.hash,
        "a new exe must produce a new build hash"
    );
    assert_eq!(
        scheduler_service::read_desired_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build2),
        "desired build must reflect the latest staging"
    );
}

// ─────────────────────────────────────────────────
// Step 3: execution request, idempotent dispatch, events, executor contract
// ─────────────────────────────────────────────────

#[test]
fn attempt_execution_request_round_trips_through_persistence() {
    let project = TempDir::new().unwrap();
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-abc", "attempt-1").unwrap();
    let original_id = request.id.clone();

    scheduler_service::persist_request(project.path(), &request).unwrap();
    let loaded = scheduler_service::load_request(project.path(), &original_id).unwrap();

    assert_eq!(
        loaded, request,
        "loaded request must equal the persisted one"
    );
}

#[test]
fn attempt_execution_request_has_nonempty_generated_id() {
    let r = fluent::scheduler_service::AttemptExecutionRequest::new("wi-1", "attempt-1").unwrap();
    assert!(!r.id.is_empty(), "generated request id must be non-empty");
    assert!(!r.created_at.is_empty(), "created_at must be non-empty");
}

#[test]
fn dispatch_submission_is_exact_bound_and_idempotent() {
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-1");
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-1", "attempt-1").unwrap();
    scheduler_service::persist_request(project.path(), &request).unwrap();

    let token1 = scheduler_service::submit_dispatch(project.path(), &request).unwrap();
    let token2 = scheduler_service::submit_dispatch(project.path(), &request).unwrap();

    assert_eq!(token1.dispatch_id, token2.dispatch_id);
    assert!(!token1.already_queued);
    assert!(token2.already_queued);
    assert_eq!(token1.work_item_id, "wi-1");
    assert_eq!(token1.attempt_id, "attempt-1");
    assert_eq!(token1.request_id, request.id);
}

#[test]
fn dispatch_requires_persisted_request() {
    let project = TempDir::new().unwrap();
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-1", "attempt-1").unwrap();
    let result = scheduler_service::submit_dispatch(project.path(), &request);
    assert!(
        result.is_err(),
        "dispatch must fail if the request is not persisted"
    );
}

#[test]
fn events_are_appended_and_replayed_in_sequence_order() {
    let project = TempDir::new().unwrap();
    let attempt_id = "attempt-1";

    let e2 = make_event(2, attempt_id, "completed");
    let e0 = make_event(0, attempt_id, "started");
    let e1 = make_event(1, attempt_id, "progress");

    scheduler_service::append_event(project.path(), &e2).unwrap();
    scheduler_service::append_event(project.path(), &e0).unwrap();
    scheduler_service::append_event(project.path(), &e1).unwrap();

    let events = scheduler_service::replay_events(project.path(), attempt_id).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[1].seq, 1);
    assert_eq!(events[2].seq, 2);
    assert_eq!(events[0].kind, "started");
    assert_eq!(events[2].kind, "completed");
}

#[test]
fn events_for_different_attempts_are_stored_independently() {
    let project = TempDir::new().unwrap();

    scheduler_service::append_event(project.path(), &make_event(0, "attempt-1", "started"))
        .unwrap();
    scheduler_service::append_event(project.path(), &make_event(0, "attempt-2", "started"))
        .unwrap();
    scheduler_service::append_event(project.path(), &make_event(1, "attempt-1", "completed"))
        .unwrap();

    let a1 = scheduler_service::replay_events(project.path(), "attempt-1").unwrap();
    let a2 = scheduler_service::replay_events(project.path(), "attempt-2").unwrap();
    assert_eq!(a1.len(), 2, "attempt-1 must have 2 events");
    assert_eq!(a2.len(), 1, "attempt-2 must have 1 event");
}

#[test]
fn replay_returns_empty_for_unknown_attempt() {
    let project = TempDir::new().unwrap();
    let events = scheduler_service::replay_events(project.path(), "attempt-999").unwrap();
    assert!(
        events.is_empty(),
        "replay of unknown attempt must return empty"
    );
}

#[test]
fn executor_validates_well_formed_request_without_launching() {
    let project = TempDir::new().unwrap();
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-exec", "attempt-1").unwrap();
    scheduler_service::persist_request(project.path(), &request).unwrap();

    let validated =
        scheduler_service::validate_executor_request(project.path(), &request.id).unwrap();
    assert_eq!(validated.request.work_item_id, "wi-exec");
    assert_eq!(validated.request.attempt_id, "attempt-1");
}

#[test]
fn executor_rejects_missing_request() {
    let project = TempDir::new().unwrap();
    let result = scheduler_service::validate_executor_request(project.path(), "nonexistent-id");
    assert!(result.is_err(), "validation must fail for missing request");
}

#[test]
fn executor_rejects_request_with_empty_work_item_id() {
    let project = TempDir::new().unwrap();
    let path = project
        .path()
        .join(".fluent/work/scheduler/requests/bad-req.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"id":"bad-req","work_item_id":"","attempt_id":"attempt-1","created_at":"2024-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let result = scheduler_service::validate_executor_request(project.path(), "bad-req");
    assert!(result.is_err(), "validation must reject empty work_item_id");
}

#[test]
fn executor_rejects_request_with_empty_attempt_id() {
    let project = TempDir::new().unwrap();
    let path = project
        .path()
        .join(".fluent/work/scheduler/requests/bad-req2.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"id":"bad-req2","work_item_id":"wi-1","attempt_id":"","created_at":"2024-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let result = scheduler_service::validate_executor_request(project.path(), "bad-req2");
    assert!(result.is_err(), "validation must reject empty attempt_id");
}

#[test]
fn executor_rejects_request_with_mismatched_id() {
    let project = TempDir::new().unwrap();
    let path = project
        .path()
        .join(".fluent/work/scheduler/requests/requested-id.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"id":"stored-id","work_item_id":"wi-1","attempt_id":"attempt-1","created_at":"2024-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let error = scheduler_service::validate_executor_request(project.path(), "requested-id")
        .err()
        .expect("validation must reject a mismatched request id");

    assert!(
        error.to_string().contains("request id mismatch"),
        "validation must fail at the request id check: {error:#}"
    );
}

#[test]
fn executor_rejects_request_with_empty_created_at() {
    let project = TempDir::new().unwrap();
    let path = project
        .path()
        .join(".fluent/work/scheduler/requests/bad-created-at.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"id":"bad-created-at","work_item_id":"wi-1","attempt_id":"attempt-1","created_at":""}"#,
    )
    .unwrap();

    let error = scheduler_service::validate_executor_request(project.path(), "bad-created-at")
        .err()
        .expect("validation must reject an empty created_at");

    assert!(
        error.to_string().contains("created_at is empty"),
        "validation must fail at the created_at check: {error:#}"
    );
}

// ─────────────────────────────────────────────────
// Step 4: failure isolation and resilience
// ─────────────────────────────────────────────────

#[test]
fn interrupted_staging_leaves_managed_executable_intact() {
    let home = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();

    let exe1 = make_fake_exe(src.path());
    scheduler_service::stage_executable(&exe1, home.path()).unwrap();

    let bad = src.path().join("does-not-exist");
    let result = scheduler_service::stage_executable(&bad, home.path());
    assert!(result.is_err(), "staging a missing file must fail");

    let managed = scheduler_service::managed_executable_path(home.path());
    assert!(
        managed.exists(),
        "managed executable must still exist after failed staging"
    );
    assert_eq!(
        fs::read(&managed).unwrap(),
        fs::read(&exe1).unwrap(),
        "managed executable must retain original content after failed staging"
    );
}

#[test]
fn malformed_registry_entry_does_not_prevent_healthy_project_registration() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let reg_path = home.path().join(".config/fluent/scheduler/registry.json");
    fs::create_dir_all(reg_path.parent().unwrap()).unwrap();
    fs::write(
        &reg_path,
        r#"{"checkouts":{"/nonexistent/path":"deadbeef"}}"#,
    )
    .unwrap();

    let identity = scheduler_service::assign_checkout_identity(project.path()).unwrap();
    scheduler_service::register_checkout(home.path(), project.path(), &identity).unwrap();

    let registry = scheduler_service::read_registry(home.path()).unwrap();
    let canonical = project.path().canonicalize().unwrap();
    assert!(
        registry.contains_key(&canonical),
        "healthy project must be registered despite stale entry"
    );
    assert!(
        registry.contains_key(&std::path::PathBuf::from("/nonexistent/path")),
        "stale entry must be preserved in registry"
    );
}

#[test]
fn two_projects_register_and_track_independently() {
    let home = TempDir::new().unwrap();
    let p1 = TempDir::new().unwrap();
    let p2 = TempDir::new().unwrap();

    let id1 = scheduler_service::assign_checkout_identity(p1.path()).unwrap();
    let id2 = scheduler_service::assign_checkout_identity(p2.path()).unwrap();
    assert_ne!(id1, id2, "two projects must get distinct identities");

    scheduler_service::register_checkout(home.path(), p1.path(), &id1).unwrap();
    scheduler_service::register_checkout(home.path(), p2.path(), &id2).unwrap();

    let registry = scheduler_service::read_registry(home.path()).unwrap();
    let c1 = p1.path().canonicalize().unwrap();
    let c2 = p2.path().canonicalize().unwrap();
    assert_eq!(registry.get(&c1).unwrap(), &id1);
    assert_eq!(registry.get(&c2).unwrap(), &id2);
}

#[test]
fn one_project_registration_does_not_affect_other_projects_entry() {
    let home = TempDir::new().unwrap();
    let p1 = TempDir::new().unwrap();
    let p2 = TempDir::new().unwrap();

    let id1 = scheduler_service::assign_checkout_identity(p1.path()).unwrap();
    let id2 = scheduler_service::assign_checkout_identity(p2.path()).unwrap();
    scheduler_service::register_checkout(home.path(), p1.path(), &id1).unwrap();
    scheduler_service::register_checkout(home.path(), p2.path(), &id2).unwrap();

    let id1b = fluent::scheduler_service::CheckoutIdentity("updated".to_string());
    scheduler_service::register_checkout(home.path(), p1.path(), &id1b).unwrap();

    let registry = scheduler_service::read_registry(home.path()).unwrap();
    let c2 = p2.path().canonicalize().unwrap();
    assert_eq!(
        registry.get(&c2).unwrap(),
        &id2,
        "p2 identity must be unchanged after p1 update"
    );
}

#[test]
fn duplicate_start_via_fake_manager_is_idempotent() {
    let home = TempDir::new().unwrap();
    let build = fake_build();
    let sock = scheduler_service::socket_path(home.path());
    let manager = FakeServiceManager::new();
    manager
        .install_or_update(&PathBuf::from("/fake/exe"), &build, &sock)
        .unwrap();

    manager.start().unwrap();
    assert!(manager.is_running());
    manager.start().unwrap();
    assert!(
        manager.is_running(),
        "duplicate start must leave service running"
    );

    manager.stop().unwrap();
    assert!(!manager.is_running());
}

#[test]
fn drain_stops_service_while_disable_only_marks_not_autostart() {
    let home = TempDir::new().unwrap();
    let build = fake_build();
    let sock = scheduler_service::socket_path(home.path());
    let manager = FakeServiceManager::new();
    manager
        .install_or_update(&PathBuf::from("/fake/exe"), &build, &sock)
        .unwrap();
    manager.enable().unwrap();
    manager.start().unwrap();

    manager.disable().unwrap();
    assert!(!manager.is_enabled());
    assert!(manager.is_running(), "disable must leave service running");

    manager.drain().unwrap();
    assert!(!manager.is_running(), "drain must stop the running service");
}

#[test]
fn observed_build_is_recorded_independently_of_desired_build() {
    let home = TempDir::new().unwrap();

    let build_a = BuildIdentity {
        version: "1.0.0".to_string(),
        hash: "aaa".to_string(),
    };
    let build_b = BuildIdentity {
        version: "1.1.0".to_string(),
        hash: "bbb".to_string(),
    };

    scheduler_service::record_desired_build(home.path(), &build_b).unwrap();
    scheduler_service::record_observed_build(home.path(), &build_a).unwrap();

    assert_eq!(
        scheduler_service::read_desired_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build_b)
    );
    assert_eq!(
        scheduler_service::read_observed_build(home.path())
            .unwrap()
            .as_ref(),
        Some(&build_a)
    );
}

// ─────────────────────────────────────────────────
// Step 1 (contract closure): concurrent registry mutations
// ─────────────────────────────────────────────────

// Sentinel test invoked as a child process by
// concurrent_registry_mutations_preserve_every_successful_change. When
// FLUENT_REGISTRY_CHILD is absent (normal nextest runs), returns immediately
// as a harmless no-op.
#[test]
fn __registry_child_worker() {
    let action = match std::env::var("FLUENT_REGISTRY_CHILD") {
        Ok(a) => a,
        Err(_) => return,
    };
    let home = std::env::var("FLUENT_REGISTRY_CHILD_HOME").expect("FLUENT_REGISTRY_CHILD_HOME");
    let project_str =
        std::env::var("FLUENT_REGISTRY_CHILD_PROJECT").expect("FLUENT_REGISTRY_CHILD_PROJECT");
    let project = std::path::Path::new(&project_str);
    match action.as_str() {
        "register" => {
            let id = std::env::var("FLUENT_REGISTRY_CHILD_IDENTITY")
                .expect("FLUENT_REGISTRY_CHILD_IDENTITY");
            let identity = fluent::scheduler_service::CheckoutIdentity(id);
            fluent::scheduler_service::register_checkout(
                std::path::Path::new(&home),
                project,
                &identity,
            )
            .expect("register_checkout in child process");
        }
        "unregister" => {
            fluent::scheduler_service::unregister_checkout(std::path::Path::new(&home), project)
                .expect("unregister_checkout in child process");
        }
        other => panic!("unknown FLUENT_REGISTRY_CHILD action: {other}"),
    }
}

fn spawn_registry_child(
    action: &str,
    home: &std::path::Path,
    project: &std::path::Path,
    identity: Option<&str>,
) -> std::process::Child {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("__registry_child_worker")
        .env("FLUENT_REGISTRY_CHILD", action)
        .env("FLUENT_REGISTRY_CHILD_HOME", home)
        .env("FLUENT_REGISTRY_CHILD_PROJECT", project)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(id) = identity {
        cmd.env("FLUENT_REGISTRY_CHILD_IDENTITY", id);
    }
    cmd.spawn().expect("spawn registry child process")
}

#[test]
fn concurrent_registry_mutations_preserve_every_successful_change() {
    const N: usize = 8;
    const REMOVED: usize = N / 2;
    let home = TempDir::new().unwrap();
    let projects: Vec<TempDir> = (0..N).map(|_| TempDir::new().unwrap()).collect();
    let identities: Vec<String> = (0..N).map(|i| format!("ident{i:08x}")).collect();

    // Spawn N child processes concurrently; each registers a distinct checkout.
    let mut children: Vec<_> = projects
        .iter()
        .zip(identities.iter())
        .map(|(project, identity)| {
            spawn_registry_child("register", home.path(), project.path(), Some(identity))
        })
        .collect();

    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().expect("wait for child process");
        assert!(status.success(), "child process {i} exited with {status:?}");
    }

    // All N checkouts must appear in the registry with correct identities.
    let registry = scheduler_service::read_registry(home.path()).unwrap();
    for (i, (project, identity)) in projects.iter().zip(identities.iter()).enumerate() {
        let canonical = project.path().canonicalize().unwrap();
        let registered = registry.get(&canonical).unwrap_or_else(|| {
            panic!("project {i} is missing from the registry after concurrent registration")
        });
        assert_eq!(
            registered.0, *identity,
            "project {i} has the wrong identity in the registry"
        );
    }

    // Concurrently remove a deterministic half of the registered checkouts.
    let mut children: Vec<_> = projects
        .iter()
        .take(REMOVED)
        .map(|project| spawn_registry_child("unregister", home.path(), project.path(), None))
        .collect();

    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().expect("wait for unregister child process");
        assert!(
            status.success(),
            "unregister child process {i} exited with {status:?}"
        );
    }

    // Every selected checkout must be gone, while untouched entries retain
    // their original identities.
    let registry = scheduler_service::read_registry(home.path()).unwrap();
    for (i, project) in projects.iter().take(REMOVED).enumerate() {
        let canonical = project.path().canonicalize().unwrap();
        assert!(
            !registry.contains_key(&canonical),
            "project {i} remains in the registry after concurrent unregistration"
        );
    }
    for (i, (project, identity)) in projects
        .iter()
        .zip(identities.iter())
        .enumerate()
        .skip(REMOVED)
    {
        let canonical = project.path().canonicalize().unwrap();
        assert_eq!(
            registry.get(&canonical),
            Some(&fluent::scheduler_service::CheckoutIdentity(
                identity.clone()
            )),
            "untouched project {i} lost or changed its identity after concurrent unregistration"
        );
    }
}

#[test]
fn execution_protocol_rejects_unsupported_generation() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-version");
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );

    let response = framed_exchange(
        &mut service,
        execution_request(
            99,
            project.path(),
            &fake_build(),
            "request-version",
            "wi-version",
        ),
    );

    assert_eq!(response["kind"], "error");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("protocol generation")
    );
    assert!(fluent::queue::list(project.path()).unwrap().is_empty());
}

#[test]
fn execution_status_reports_missing_request() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );

    let response = framed_exchange(
        &mut service,
        serde_json::json!({
            "kind": "execution_status",
            "generation": scheduler_service::PROTOCOL_GENERATION,
            "build": fake_build(),
            "project_root": project.path(),
            "request_id": "missing-request"
        }),
    );

    assert_eq!(response["kind"], "error");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("missing-request")
    );
}

#[test]
fn execution_submission_replays_one_real_queue_dispatch() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-replay");
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    let request = execution_request(
        scheduler_service::PROTOCOL_GENERATION,
        project.path(),
        &fake_build(),
        "request-replay",
        "wi-replay",
    );

    let first = framed_exchange(&mut service, request.clone());
    let replay = framed_exchange(&mut service, request);

    assert_eq!(first["kind"], "execution_submitted");
    assert_eq!(first["token"]["already_queued"], false);
    assert_eq!(replay["token"]["already_queued"], true);
    assert_eq!(
        first["token"]["dispatch_id"],
        replay["token"]["dispatch_id"]
    );
    assert_eq!(fluent::queue::list(project.path()).unwrap().len(), 1);
}

#[test]
fn execution_submission_rejects_unregistered_project_and_stale_build() {
    let home = TempDir::new().unwrap();
    let registered = TempDir::new().unwrap();
    let unregistered = TempDir::new().unwrap();
    create_execution_ready_work(registered.path(), "wi-build");
    create_execution_ready_work(unregistered.path(), "wi-project");
    let mut service = registered_service(
        home.path(),
        registered.path(),
        Arc::new(NeedsUserRunner::default()),
    );

    let invalid_project = framed_exchange(
        &mut service,
        execution_request(
            scheduler_service::PROTOCOL_GENERATION,
            unregistered.path(),
            &fake_build(),
            "request-project",
            "wi-project",
        ),
    );
    let stale_build = framed_exchange(
        &mut service,
        execution_request(
            scheduler_service::PROTOCOL_GENERATION,
            registered.path(),
            &BuildIdentity {
                version: "stale".into(),
                hash: "stale".into(),
            },
            "request-build",
            "wi-build",
        ),
    );
    scheduler_service::record_desired_build(
        home.path(),
        &BuildIdentity {
            version: "new".into(),
            hash: "new-build".into(),
        },
    )
    .unwrap();
    let stale_service = framed_exchange(
        &mut service,
        execution_request(
            scheduler_service::PROTOCOL_GENERATION,
            registered.path(),
            &fake_build(),
            "request-stale-service",
            "wi-build",
        ),
    );

    assert!(
        invalid_project["message"]
            .as_str()
            .unwrap()
            .contains("registered")
    );
    assert!(stale_build["message"].as_str().unwrap().contains("build"));
    assert!(
        stale_service["message"]
            .as_str()
            .unwrap()
            .contains("desired build")
    );
    assert!(fluent::queue::list(registered.path()).unwrap().is_empty());
    assert!(fluent::queue::list(unregistered.path()).unwrap().is_empty());
}

#[test]
fn scheduler_service_applies_capacity_before_consuming_real_dispatches() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".fluent")).unwrap();
    fs::write(
        project.path().join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: 1\n",
    )
    .unwrap();
    create_execution_ready_work(project.path(), "wi-capacity-1");
    create_execution_ready_work(project.path(), "wi-capacity-2");
    let runner = Arc::new(BlockingRunner::default());
    let mut service = registered_service(home.path(), project.path(), runner.clone());
    for (request_id, work_item_id) in [
        ("request-capacity-1", "wi-capacity-1"),
        ("request-capacity-2", "wi-capacity-2"),
    ] {
        let response = framed_exchange(
            &mut service,
            execution_request(
                scheduler_service::PROTOCOL_GENERATION,
                project.path(),
                &fake_build(),
                request_id,
                work_item_id,
            ),
        );
        assert_eq!(response["kind"], "execution_submitted");
    }

    service.poll_once().unwrap();
    runner.wait_for_started(1);
    assert!(matches!(
        fluent::scheduler::status(project.path()).unwrap(),
        fluent::scheduler::SchedulerStatus::Running(_)
    ));
    let statuses = fluent::queue::list(project.path()).unwrap();
    assert_eq!(
        statuses
            .iter()
            .filter(|dispatch| matches!(
                dispatch.status,
                fluent::queue::DispatchStatus::Claimed | fluent::queue::DispatchStatus::Running
            ))
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|dispatch| dispatch.status == fluent::queue::DispatchStatus::Queued)
            .count(),
        1
    );
    runner.release_one();
    service.join_workers().unwrap();

    service.poll_once().unwrap();
    runner.wait_for_started(2);
    runner.release_one();
    service.join_workers().unwrap();
    assert!(
        fluent::queue::list(project.path())
            .unwrap()
            .iter()
            .all(|dispatch| { dispatch.status == fluent::queue::DispatchStatus::NeedsUser })
    );
    let first = fluent::work_model::WorkModelStore::new(project.path())
        .read_work_item("wi-capacity-1")
        .unwrap();
    let second = fluent::work_model::WorkModelStore::new(project.path())
        .read_work_item("wi-capacity-2")
        .unwrap();
    assert_eq!(first.attempts.len(), 1);
    assert_eq!(second.attempts.len(), 1);
    drop(service);
    assert!(matches!(
        fluent::scheduler::status(project.path()).unwrap(),
        fluent::scheduler::SchedulerStatus::Stopped
    ));
}

#[test]
fn scheduler_service_honors_project_stop_until_new_submission() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-restart-service");
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    service.poll_once().unwrap();
    assert!(matches!(
        fluent::scheduler::status(project.path()).unwrap(),
        fluent::scheduler::SchedulerStatus::Running(_)
    ));

    let project_path = project.path().to_path_buf();
    let stop = std::thread::spawn(move || fluent::scheduler::stop(&project_path).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !matches!(
        fluent::scheduler::status(project.path()).unwrap(),
        fluent::scheduler::SchedulerStatus::Stopped
    ) {
        service.poll_once().unwrap();
        assert!(std::time::Instant::now() < deadline, "service did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.join().unwrap();
    service.poll_once().unwrap();
    assert!(matches!(
        fluent::scheduler::status(project.path()).unwrap(),
        fluent::scheduler::SchedulerStatus::Stopped
    ));

    let response = framed_exchange(
        &mut service,
        execution_request(
            scheduler_service::PROTOCOL_GENERATION,
            project.path(),
            &fake_build(),
            "request-restart-service",
            "wi-restart-service",
        ),
    );
    assert_eq!(response["kind"], "execution_submitted");
    service.poll_once().unwrap();
    service.join_workers().unwrap();
    assert_eq!(
        fluent::queue::show(project.path(), "wi-restart-service")
            .unwrap()
            .status,
        fluent::queue::DispatchStatus::NeedsUser
    );
}

#[test]
fn execution_status_reads_authoritative_queue_outcome() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-status");
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    framed_exchange(
        &mut service,
        execution_request(
            scheduler_service::PROTOCOL_GENERATION,
            project.path(),
            &fake_build(),
            "request-status",
            "wi-status",
        ),
    );
    service.poll_once().unwrap();
    service.join_workers().unwrap();

    let response = framed_exchange(
        &mut service,
        serde_json::json!({
            "kind": "execution_status",
            "generation": scheduler_service::PROTOCOL_GENERATION,
            "build": fake_build(),
            "project_root": project.path(),
            "request_id": "request-status"
        }),
    );

    assert_eq!(response["kind"], "execution_status");
    assert_eq!(response["status"], "needs-user");
    assert_eq!(response["work_item_id"], "wi-status");
    assert!(
        response["attempt_id"]
            .as_str()
            .unwrap()
            .starts_with("attempt-")
    );
}

#[test]
fn scheduler_recovery_uncreated_attempt() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-orphan");
    fluent::queue::add(project.path(), "wi-orphan", Some(0)).unwrap();
    let claimed = fluent::queue::claim(project.path(), "wi-orphan", "attempt-orphan")
        .unwrap()
        .unwrap();
    let runner = Arc::new(NeedsUserRunner::default());
    let mut service = registered_service(home.path(), project.path(), runner.clone());
    service.poll_once().unwrap();
    service.join_workers().unwrap();
    let dispatch = fluent::queue::show(project.path(), "wi-orphan").unwrap();
    assert_eq!(dispatch.dispatch_id, claimed.dispatch_id);
    assert_eq!(dispatch.status, fluent::queue::DispatchStatus::NeedsUser);
    assert_eq!(runner.calls.lock().unwrap().as_slice(), &["wi-orphan"]);
    assert_eq!(
        fluent::work_model::WorkModelStore::new(project.path())
            .read_work_item("wi-orphan")
            .unwrap()
            .attempts
            .len(),
        1
    );
}

#[test]
fn scheduler_isolation_continues_healthy_projects() {
    for fault in ["configuration", "corrupt-work", "coordinator"] {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let bad = root.path().join("00-bad");
        let good = root.path().join("10-good");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::create_dir_all(&good).unwrap();
        let runner = Arc::new(NeedsUserRunner::default());
        let mut service = registered_service(&home, &bad, runner.clone());
        let identity = scheduler_service::assign_checkout_identity(&good).unwrap();
        scheduler_service::register_checkout(&home, &good, &identity).unwrap();
        fs::create_dir_all(&bad.join(".fluent")).unwrap();
        create_execution_ready_work(&bad, "wi-bad");
        fluent::queue::add(&bad, "wi-bad", Some(0)).unwrap();
        match fault {
            "configuration" => fs::write(
                bad.join(".fluent/config.yaml"),
                "scheduler:\n  local-concurrency: invalid\n",
            )
            .unwrap(),
            "corrupt-work" => {
                let path = fluent::work_model::WorkModelStore::new(&bad)
                    .work_item_path("wi-bad")
                    .unwrap();
                fs::write(path, "invalid json").unwrap();
            }
            "coordinator" => {
                let path = bad.join(".fluent/work/scheduler/coordinator.lock");
                fs::create_dir_all(&path).unwrap();
            }
            _ => unreachable!(),
        }
        create_execution_ready_work(&good, "wi-good");
        fluent::queue::add(&good, "wi-good", Some(0)).unwrap();
        service.poll_once().unwrap();
        service.join_workers().unwrap();
        assert_eq!(
            fluent::queue::show(&good, "wi-good").unwrap().status,
            fluent::queue::DispatchStatus::NeedsUser
        );
        let bad_dispatch = fluent::queue::show(&bad, "wi-bad").unwrap();
        assert_eq!(
            bad_dispatch.status,
            if fault == "corrupt-work" {
                fluent::queue::DispatchStatus::Blocked
            } else {
                fluent::queue::DispatchStatus::Queued
            },
            "{fault}"
        );
        if fault == "corrupt-work" {
            let path = fluent::work_model::WorkModelStore::new(&bad)
                .work_item_path("wi-bad")
                .unwrap();
            assert_eq!(fs::read_to_string(path).unwrap(), "invalid json");
        } else {
            assert!(
                service
                    .last_poll_failures()
                    .iter()
                    .any(|error| error.contains(&bad.display().to_string()))
            );
        }
    }
}

fn create_bound_attempt(
    project: &std::path::Path,
    work_item_id: &str,
    attempt_id: &str,
    status: fluent::work_model::AttemptStatus,
) {
    let store = fluent::work_model::WorkModelStore::new(project);
    let mut item = store.read_work_item(work_item_id).unwrap();
    item.add_initial_attempt(attempt_id).unwrap();
    item.attempts[0].status = status;
    store.write_work_item(&item).unwrap();
}

#[derive(Default)]
struct RecordingRunner(Mutex<Vec<String>>);

impl fluent::scheduler::AttemptRunner for RecordingRunner {
    fn run(
        &self,
        project: &std::path::Path,
        work_item_id: &str,
        attempt_id: &str,
        _tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        self.0.lock().unwrap().push(attempt_id.into());
        persist_runner_needs_user(project, work_item_id, attempt_id);
        Ok(fluent::scheduler::AttemptOutcome::NeedsUser)
    }
}

#[test]
fn scheduler_recovery_preserves_bound_attempt() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-resume");
    fluent::queue::add(project.path(), "wi-resume", Some(0)).unwrap();
    fluent::queue::claim(project.path(), "wi-resume", "attempt-preserved")
        .unwrap()
        .unwrap();
    create_bound_attempt(
        project.path(),
        "wi-resume",
        "attempt-preserved",
        fluent::work_model::AttemptStatus::Executing,
    );
    let runner = Arc::new(RecordingRunner::default());
    let mut service = registered_service(home.path(), project.path(), runner.clone());
    service.poll_once().unwrap();
    service.join_workers().unwrap();
    assert_eq!(runner.0.lock().unwrap().as_slice(), &["attempt-preserved"]);
    let item = fluent::work_model::WorkModelStore::new(project.path())
        .read_work_item("wi-resume")
        .unwrap();
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(item.attempts[0].id, "attempt-preserved");
}

#[test]
fn scheduler_recovery_publishes_existing_outcome() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-finished");
    fluent::queue::add(project.path(), "wi-finished", Some(0)).unwrap();
    fluent::queue::claim(project.path(), "wi-finished", "attempt-finished")
        .unwrap()
        .unwrap();
    create_bound_attempt(
        project.path(),
        "wi-finished",
        "attempt-finished",
        fluent::work_model::AttemptStatus::Failed,
    );
    let runner = Arc::new(RecordingRunner::default());
    let mut service = registered_service(home.path(), project.path(), runner.clone());
    service.poll_once().unwrap();
    service.join_workers().unwrap();
    assert_eq!(
        fluent::queue::show(project.path(), "wi-finished")
            .unwrap()
            .status,
        fluent::queue::DispatchStatus::Failed
    );
    assert!(runner.0.lock().unwrap().is_empty());
}

#[test]
fn scheduler_service_fences_recovery() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-live");
    fluent::queue::add(project.path(), "wi-live", Some(0)).unwrap();
    let token = fluent::queue::claim(project.path(), "wi-live", "attempt-live")
        .unwrap()
        .unwrap();
    let _lease = fluent::lease::acquire(&fluent::queue::dispatch_lease_path(
        project.path(),
        "wi-live",
    ))
    .unwrap();
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    service.poll_once().unwrap();
    let after = fluent::queue::show(project.path(), "wi-live").unwrap();
    assert_eq!(after.status, fluent::queue::DispatchStatus::Claimed);
    assert_eq!(after.generation, token.generation);
    drop(_lease);
    let newer_generation = Arc::new(Mutex::new(None));
    let observed = newer_generation.clone();
    service.set_recovery_observer(Arc::new(move |root, dispatch| {
        if dispatch.work_item_id == "wi-live" {
            let newer = fluent::queue::mark_running(root, &token).unwrap();
            *observed.lock().unwrap() = Some(newer.generation);
        }
    }));
    service.poll_once().unwrap();
    let newer_generation = newer_generation.lock().unwrap().unwrap();
    let after = fluent::queue::show(project.path(), "wi-live").unwrap();
    assert_eq!(after.status, fluent::queue::DispatchStatus::Running);
    assert_eq!(after.generation, newer_generation);
}

#[test]
fn scheduler_service_fences_unverified_ownership() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-unverified");
    fluent::queue::add(project.path(), "wi-unverified", Some(0)).unwrap();
    let token = fluent::queue::claim(project.path(), "wi-unverified", "attempt-unverified")
        .unwrap()
        .unwrap();
    let lease_path = fluent::queue::dispatch_lease_path(project.path(), "wi-unverified");
    fs::create_dir_all(lease_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&lease_path, &lease_path).unwrap();
    assert!(fluent::lease::is_leased_checked(&lease_path).is_err());
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    service.poll_once().unwrap();
    let after = fluent::queue::show(project.path(), "wi-unverified").unwrap();
    assert_eq!(after.status, fluent::queue::DispatchStatus::Claimed);
    assert_eq!(after.generation, token.generation);
}

#[test]
fn scheduler_isolation_reports_failure_context() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".fluent")).unwrap();
    fs::write(
        project.path().join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: invalid\n",
    )
    .unwrap();
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    service.poll_once().unwrap();
    let diagnostic = &service.last_poll_failures()[0];
    assert!(diagnostic.contains(&project.path().display().to_string()));
    assert!(diagnostic.contains("read scheduler capacity"));

    #[cfg(feature = "test-support")]
    {
        let known = TempDir::new().unwrap();
        create_execution_ready_work(known.path(), "wi-worker-fail");
        fluent::queue::add(known.path(), "wi-worker-fail", Some(0)).unwrap();
        let mut service =
            registered_service(home.path(), known.path(), Arc::new(HostFailureRunner));
        service.poll_once().unwrap();
        let dispatch = fluent::queue::show(known.path(), "wi-worker-fail").unwrap();
        let diagnostic = format!("{:#}", service.join_workers().unwrap_err());
        assert!(diagnostic.contains(&known.path().display().to_string()));
        assert!(diagnostic.contains("worker"));
        assert!(diagnostic.contains("wi-worker-fail"));
        assert!(diagnostic.contains(&dispatch.dispatch_id));
    }
}

#[test]
fn scheduler_isolation_bounds_retries_by_poll() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".fluent")).unwrap();
    fs::write(
        project.path().join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: invalid\n",
    )
    .unwrap();
    create_execution_ready_work(project.path(), "wi-repair");
    fluent::queue::add(project.path(), "wi-repair", Some(0)).unwrap();
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    for _ in 0..2 {
        service.poll_once().unwrap();
        assert_eq!(service.last_poll_failures().len(), 1);
        assert_eq!(
            fluent::queue::show(project.path(), "wi-repair")
                .unwrap()
                .status,
            fluent::queue::DispatchStatus::Queued
        );
    }
    fs::write(
        project.path().join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: 1\n",
    )
    .unwrap();
    service.poll_once().unwrap();
    service.join_workers().unwrap();
    assert!(service.last_poll_failures().is_empty());
    assert_eq!(
        fluent::queue::show(project.path(), "wi-repair")
            .unwrap()
            .status,
        fluent::queue::DispatchStatus::NeedsUser
    );
}

#[test]
#[cfg(feature = "test-support")]
fn scheduler_isolation_bounds_failed_stop_checks_by_poll() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let mut service = registered_service(
        home.path(),
        project.path(),
        Arc::new(NeedsUserRunner::default()),
    );
    service.poll_once().unwrap();
    let checks = Arc::new(AtomicUsize::new(0));
    let observed = checks.clone();
    service.set_stop_check(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("fixture stop check failure")
    }));
    let start = std::time::Instant::now();
    let interval = Duration::from_secs(1);
    let mut next_poll = start + interval;
    for iteration in 0..5 {
        service
            .tick_for_test(
                start + Duration::from_millis(iteration * 50),
                &mut next_poll,
                interval,
            )
            .unwrap();
    }
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    service
        .tick_for_test(start + interval, &mut next_poll, interval)
        .unwrap();
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    for iteration in 1..5 {
        service
            .tick_for_test(
                start + interval + Duration::from_millis(iteration * 50),
                &mut next_poll,
                interval,
            )
            .unwrap();
    }
    assert_eq!(checks.load(Ordering::SeqCst), 2);
}

#[test]
fn scheduler_isolation_keeps_prior_claim_with_bad_sibling() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".fluent")).unwrap();
    fs::write(
        project.path().join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: 2\n",
    )
    .unwrap();
    create_execution_ready_work(project.path(), "wi-first");
    create_execution_ready_work(project.path(), "wi-bad");
    fluent::queue::add(project.path(), "wi-first", Some(2)).unwrap();
    fluent::queue::add(project.path(), "wi-bad", Some(1)).unwrap();
    let bad_work = fluent::work_model::WorkModelStore::new(project.path())
        .work_item_path("wi-bad")
        .unwrap();
    assert!(bad_work.exists());
    fs::write(&bad_work, "invalid json").unwrap();
    let runner = Arc::new(BlockingRunner::default());
    let mut service = registered_service(home.path(), project.path(), runner.clone());
    service.poll_once().unwrap();
    runner.wait_for_started(1);
    assert_eq!(
        fluent::queue::show(project.path(), "wi-first")
            .unwrap()
            .status,
        fluent::queue::DispatchStatus::Running
    );
    runner.release_one();
    service.join_workers().unwrap();
    assert_eq!(
        fluent::queue::show(project.path(), "wi-first")
            .unwrap()
            .status,
        fluent::queue::DispatchStatus::NeedsUser
    );
}

struct MixedWorkerRunner {
    blocked: BlockingRunner,
    failure_started: Arc<(Mutex<bool>, Condvar)>,
    panic_worker: bool,
}

impl fluent::scheduler::AttemptRunner for MixedWorkerRunner {
    fn run(
        &self,
        project: &std::path::Path,
        work_item_id: &str,
        attempt_id: &str,
        tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        if work_item_id == "wi-fail" {
            let (lock, changed) = &*self.failure_started;
            *lock.lock().unwrap() = true;
            changed.notify_all();
            if self.panic_worker {
                panic!("fixture worker panic");
            }
            anyhow::bail!("fixture worker error");
        }
        fluent::scheduler::AttemptRunner::run(
            &self.blocked,
            project,
            work_item_id,
            attempt_id,
            tester_no_sandbox,
        )
    }
}

#[test]
fn scheduler_workers_preserve_others_after_failure() {
    for panic_worker in [false, true] {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        fs::create_dir_all(project.path().join(".fluent")).unwrap();
        fs::write(
            project.path().join(".fluent/config.yaml"),
            "scheduler:\n  local-concurrency: 2\n",
        )
        .unwrap();
        for (id, priority) in [("wi-fail", 2), ("wi-healthy", 1)] {
            create_execution_ready_work(project.path(), id);
            fluent::queue::add(project.path(), id, Some(priority)).unwrap();
        }
        let runner = Arc::new(MixedWorkerRunner {
            blocked: BlockingRunner::default(),
            failure_started: Arc::new((Mutex::new(false), Condvar::new())),
            panic_worker,
        });
        let mut service = registered_service(home.path(), project.path(), runner.clone());
        service.poll_once().unwrap();
        runner.blocked.wait_for_started(1);
        let (lock, changed) = &*runner.failure_started;
        let (failed, timeout) = changed
            .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(2), |failed| {
                !*failed
            })
            .unwrap();
        assert!(!timeout.timed_out() && *failed);
        drop(failed);
        let release = runner.clone();
        let joiner = std::thread::spawn(move || service.join_workers());
        release.blocked.release_one();
        let error = joiner.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("wi-fail"));
        assert!(error.contains("worker"));
        assert!(error.contains(&project.path().display().to_string()));
        let failed_dispatch = fluent::queue::show(project.path(), "wi-fail").unwrap();
        assert!(error.contains(&failed_dispatch.dispatch_id));
        assert_eq!(
            fluent::queue::show(project.path(), "wi-healthy")
                .unwrap()
                .status,
            fluent::queue::DispatchStatus::NeedsUser
        );
        assert!(
            !fluent::lease::is_leased_checked(&fluent::queue::dispatch_lease_path(
                project.path(),
                "wi-healthy"
            ))
            .unwrap()
        );
    }
}

#[test]
#[ignore]
#[cfg(feature = "test-support")]
fn __scheduler_service_process_fixture() {
    let home = std::env::var_os("FLUENT_SCHEDULER_TEST_HOME").expect("fixture home");
    if std::env::var_os("FLUENT_SCHEDULER_TEST_CRASH_PREFIX").is_some() {
        scheduler_service::run_service_with_test_runner(
            std::path::Path::new(&home),
            fake_build(),
            1,
            Arc::new(CrashPrefixRunner),
        )
        .unwrap();
    } else if std::env::var_os("FLUENT_SCHEDULER_TEST_WORKER_ERROR").is_some() {
        scheduler_service::run_service_with_test_runner(
            std::path::Path::new(&home),
            fake_build(),
            1,
            Arc::new(HostFailureRunner),
        )
        .unwrap();
    } else {
        scheduler_service::run_service_with_test_runner(
            std::path::Path::new(&home),
            fake_build(),
            1,
            Arc::new(NeedsUserRunner::default()),
        )
        .unwrap();
    }
}

#[cfg(feature = "test-support")]
struct CrashPrefixRunner;

#[cfg(feature = "test-support")]
impl fluent::scheduler::AttemptRunner for CrashPrefixRunner {
    fn run(
        &self,
        _project: &std::path::Path,
        _work_item_id: &str,
        _attempt_id: &str,
        _tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(unix)]
#[cfg(feature = "test-support")]
#[test]
fn scheduler_recovery_restarts_after_owned_child_crash() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    let identity = scheduler_service::assign_checkout_identity(&project).unwrap();
    scheduler_service::register_checkout(&home, &project, &identity).unwrap();
    create_execution_ready_work(&project, "wi-owned-crash");
    fluent::queue::add(&project, "wi-owned-crash", Some(0)).unwrap();

    let spawn = |crash: bool| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("__scheduler_service_process_fixture")
            .env("FLUENT_SCHEDULER_TEST_HOME", &home)
            .env("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT", "1");
        if crash {
            command.env("FLUENT_SCHEDULER_TEST_CRASH_PREFIX", "1");
        } else {
            command.env_remove("FLUENT_SCHEDULER_TEST_CRASH_PREFIX");
        }
        command.spawn().unwrap()
    };
    let mut first = spawn(true);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let running = loop {
        match fluent::queue::show(&project, "wi-owned-crash") {
            Ok(dispatch) if dispatch.status == fluent::queue::DispatchStatus::Running => {
                break dispatch;
            }
            _ if std::time::Instant::now() >= deadline => {
                first.kill().unwrap();
                first.wait().unwrap();
                panic!("first service did not own a running dispatch");
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert!(
        fluent::lease::is_leased_checked(&fluent::queue::dispatch_lease_path(
            &project,
            "wi-owned-crash"
        ))
        .unwrap()
    );
    let attempt_id = running.bound_attempt_id.clone().unwrap();
    first.kill().unwrap();
    first.wait().unwrap();

    let mut replacement = spawn(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let settled = loop {
        match fluent::queue::show(&project, "wi-owned-crash") {
            Ok(dispatch) if dispatch.status == fluent::queue::DispatchStatus::NeedsUser => {
                break dispatch;
            }
            _ if std::time::Instant::now() >= deadline => {
                replacement.kill().unwrap();
                replacement.wait().unwrap();
                panic!("replacement service did not recover the owned dispatch");
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    replacement.kill().unwrap();
    replacement.wait().unwrap();
    assert_eq!(settled.dispatch_id, running.dispatch_id);
    assert_eq!(
        settled.bound_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    let item = fluent::work_model::WorkModelStore::new(&project)
        .read_work_item("wi-owned-crash")
        .unwrap();
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(item.attempts[0].id, attempt_id);
}

#[cfg(feature = "test-support")]
struct HostFailureRunner;

#[cfg(feature = "test-support")]
impl fluent::scheduler::AttemptRunner for HostFailureRunner {
    fn run(
        &self,
        project: &std::path::Path,
        work_item_id: &str,
        attempt_id: &str,
        _tester_no_sandbox: bool,
    ) -> anyhow::Result<fluent::scheduler::AttemptOutcome> {
        if work_item_id == "wi-worker-fail" {
            anyhow::bail!("fixture worker failure");
        }
        persist_runner_needs_user(project, work_item_id, attempt_id);
        Ok(fluent::scheduler::AttemptOutcome::NeedsUser)
    }
}

#[cfg(unix)]
#[cfg(feature = "test-support")]
#[test]
#[ignore]
fn scheduler_host_recovery_crash_restart_and_project_isolation() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let bad = root.path().join("00-bad");
    let healthy = root.path().join("10-healthy");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(bad.join(".fluent")).unwrap();
    fs::create_dir_all(&healthy).unwrap();
    fs::write(
        bad.join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: invalid\n",
    )
    .unwrap();
    let bad_identity = scheduler_service::assign_checkout_identity(&bad).unwrap();
    scheduler_service::register_checkout(&home, &bad, &bad_identity).unwrap();
    let healthy_identity = scheduler_service::assign_checkout_identity(&healthy).unwrap();
    scheduler_service::register_checkout(&home, &healthy, &healthy_identity).unwrap();
    create_execution_ready_work(&healthy, "wi-crash");
    fluent::queue::add(&healthy, "wi-crash", Some(0)).unwrap();
    fluent::queue::claim(&healthy, "wi-crash", "attempt-crash")
        .unwrap()
        .unwrap();
    create_bound_attempt(
        &healthy,
        "wi-crash",
        "attempt-crash",
        fluent::work_model::AttemptStatus::Failed,
    );

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("__scheduler_service_process_fixture")
        .env("FLUENT_SCHEDULER_TEST_HOME", &home)
        .env_remove("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        match fluent::queue::show(&healthy, "wi-crash") {
            Ok(dispatch) if dispatch.status == fluent::queue::DispatchStatus::Failed => {
                break Ok(());
            }
            _ if std::time::Instant::now() >= deadline => {
                break Err("real service did not reconcile crash claim");
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();
    result.unwrap();
    assert!(matches!(
        fluent::scheduler::status(&healthy).unwrap(),
        fluent::scheduler::SchedulerStatus::Stopped
    ));
}

#[cfg(unix)]
#[cfg(feature = "test-support")]
#[test]
#[ignore]
fn scheduler_host_recovery_worker_error_keeps_other_work() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(project.join(".fluent")).unwrap();
    fs::write(
        project.join(".fluent/config.yaml"),
        "scheduler:\n  local-concurrency: 2\n",
    )
    .unwrap();
    let identity = scheduler_service::assign_checkout_identity(&project).unwrap();
    scheduler_service::register_checkout(&home, &project, &identity).unwrap();
    for id in ["wi-worker-fail", "wi-worker-healthy"] {
        create_execution_ready_work(&project, id);
        fluent::queue::add(&project, id, Some(0)).unwrap();
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("__scheduler_service_process_fixture")
        .env("FLUENT_SCHEDULER_TEST_HOME", &home)
        .env("FLUENT_SCHEDULER_TEST_WORKER_ERROR", "1")
        .env_remove("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        match fluent::queue::show(&project, "wi-worker-healthy") {
            Ok(dispatch) if dispatch.status == fluent::queue::DispatchStatus::NeedsUser => {
                break Ok(());
            }
            _ if std::time::Instant::now() >= deadline => {
                break Err("healthy worker did not settle");
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let alive = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    result.unwrap();
    assert!(alive, "worker failure terminated shared service");
    assert!(
        !fluent::lease::is_leased_checked(&fluent::queue::dispatch_lease_path(
            &project,
            "wi-worker-healthy"
        ))
        .unwrap()
    );
}

// ─────────────────────────────────────────────────
// Step 2 (contract closure): request immutability and dispatch exact binding
// ─────────────────────────────────────────────────

#[test]
fn request_persistence_is_immutable_and_idempotent() {
    let project = TempDir::new().unwrap();
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-imm", "attempt-1").unwrap();

    // First persist: must succeed.
    scheduler_service::persist_request(project.path(), &request).unwrap();

    // Second persist with identical content: idempotent success.
    scheduler_service::persist_request(project.path(), &request).unwrap();

    // Original content must be unchanged.
    let loaded = scheduler_service::load_request(project.path(), &request.id).unwrap();
    assert_eq!(loaded, request, "loaded request must equal the original");

    // Persist with same ID but different work_item_id: must be rejected.
    let mut different = request.clone();
    different.work_item_id = "wi-other".to_string();
    let result = scheduler_service::persist_request(project.path(), &different);
    assert!(
        result.is_err(),
        "persist with conflicting content must return an error"
    );

    // Original content must survive the rejected overwrite.
    let after_rejection = scheduler_service::load_request(project.path(), &request.id).unwrap();
    assert_eq!(
        after_rejection, request,
        "original request must be preserved after a rejected overwrite"
    );
}

#[test]
fn dispatch_rejects_request_or_token_identity_mismatch() {
    let project = TempDir::new().unwrap();
    create_execution_ready_work(project.path(), "wi-bound");

    // Persist a valid request.
    let request =
        fluent::scheduler_service::AttemptExecutionRequest::new("wi-bound", "attempt-1").unwrap();
    scheduler_service::persist_request(project.path(), &request).unwrap();

    // Submitting with a wrong work_item_id must be rejected.
    let mut tampered_wi = request.clone();
    tampered_wi.work_item_id = "wi-other".to_string();
    assert!(
        scheduler_service::submit_dispatch(project.path(), &tampered_wi).is_err(),
        "dispatch with wrong work_item_id must fail"
    );

    // Submitting with a wrong attempt_id must be rejected.
    let mut tampered_at = request.clone();
    tampered_at.attempt_id = "attempt-other".to_string();
    assert!(
        scheduler_service::submit_dispatch(project.path(), &tampered_at).is_err(),
        "dispatch with wrong attempt_id must fail"
    );

    // Correct submission: creates a token.
    let token = scheduler_service::submit_dispatch(project.path(), &request).unwrap();
    assert_eq!(token.request_id, request.id);
    assert_eq!(token.work_item_id, "wi-bound");
    assert_eq!(token.attempt_id, "attempt-1");

    // Idempotent re-submission with correct request: returns the same token.
    let token2 = scheduler_service::submit_dispatch(project.path(), &request).unwrap();
    assert_eq!(token.dispatch_id, token2.dispatch_id);
    assert!(token2.already_queued);

    // Corrupt the on-disk token to simulate tampering.
    let token_path = project
        .path()
        .join(".fluent/work/scheduler/dispatches")
        .join(format!("{}.json", request.id));
    let corrupted = fluent::scheduler_service::DispatchToken {
        request_id: request.id.clone(),
        work_item_id: "wi-corrupted".to_string(),
        attempt_id: "attempt-1".to_string(),
        dispatch_id: "dispatch-1".to_string(),
        already_queued: false,
    };
    fs::write(&token_path, serde_json::to_vec_pretty(&corrupted).unwrap()).unwrap();

    // Submission must now fail because the stored token doesn't match the
    // persisted request.
    assert!(
        scheduler_service::submit_dispatch(project.path(), &request).is_err(),
        "dispatch must fail when the stored token has a mismatched work_item_id"
    );
}

// ─────────────────────────────────────────────────
// Step 3 (contract closure): oversized frame rejection
// ─────────────────────────────────────────────────

#[test]
#[cfg(feature = "test-support")]
fn oversized_frame_is_rejected_before_payload_read() {
    use std::os::unix::net::UnixStream;

    // Create a connected socket pair without binding — avoids the sandbox
    // restriction on Unix socket bind.
    let (mut server, mut client) = UnixStream::pair().expect("UnixStream::pair");

    // Server: read the request frame, then respond with an oversized length
    // header but no payload. The client must bail before reading any payload.
    let server_handle = std::thread::spawn(move || {
        let mut len_buf = [0u8; 4];
        server.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut req_buf = vec![0u8; len];
        server.read_exact(&mut req_buf).unwrap();
        // Declare MAX_FRAME_SIZE + 1 bytes without writing any payload.
        let oversized: u32 = (fluent::scheduler_service::MAX_FRAME_SIZE as u32) + 1;
        server.write_all(&oversized.to_le_bytes()).unwrap();
    });

    // Client: write a valid request frame, then try to read the oversized response.
    let req = serde_json::to_vec(&serde_json::json!({
        "kind": "health",
        "generation": fluent::scheduler_service::PROTOCOL_GENERATION
    }))
    .unwrap();
    fluent::scheduler_service::frame_write_test_support(&mut client, &req)
        .expect("write valid request frame");
    let result = fluent::scheduler_service::frame_read_test_support(&mut client);
    assert!(
        result.is_err(),
        "frame_read must return an error for an oversized declared length"
    );
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
        "error kind must be InvalidData for an oversized inbound frame"
    );

    server_handle.join().expect("server thread panicked");

    // Also verify that frame_write rejects an outbound payload exceeding the cap.
    let (mut a, _b) = UnixStream::pair().expect("second UnixStream::pair");
    let huge = vec![0u8; fluent::scheduler_service::MAX_FRAME_SIZE + 1];
    let write_result = fluent::scheduler_service::frame_write_test_support(&mut a, &huge);
    assert!(
        write_result.is_err(),
        "frame_write must return an error for a payload exceeding MAX_FRAME_SIZE"
    );
    assert_eq!(
        write_result.unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput,
        "error kind must be InvalidInput for an oversized outbound payload"
    );
}
