#![cfg(all(unix, feature = "test-support"))]

use fluent::execution::{self, CancelOutcome, ExecutionRecord, ExecutionState};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "{} was not produced", path.display());
}

#[test]
fn launch_handoff_owner_helper() {
    let Ok(dir) = std::env::var("FLUENT_TEST_LAUNCH_DIR") else {
        return;
    };
    let dir = Path::new(&dir);
    let transcript = dir.join("transcript.jsonl");
    let marker = dir.join("payload");
    let script = std::env::var("FLUENT_TEST_LAUNCH_SCRIPT")
        .unwrap_or_else(|_| format!("touch '{}'; sleep 30", marker.display()));
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _child = execution::spawn(&mut command, &transcript).unwrap();
}

#[test]
fn launch_handoff_preserves_owned_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 30 &")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut leader = execution::spawn(&mut command, &transcript).unwrap();
    leader.wait().unwrap();
    let path = dir.path().join("execution.json");
    assert!(execution::execution_is_live(&path).unwrap());
    let mut second = Command::new("/usr/bin/true");
    assert!(execution::spawn(&mut second, &transcript).is_err());
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
}

#[test]
fn launch_handoff_settled_record_preserves_owned_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 30 &")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut leader = execution::spawn(&mut command, &transcript).unwrap();
    let mut heartbeat = execution::ExecutionHeartbeat::start(&transcript, leader.id()).unwrap();
    leader.wait().unwrap();
    let path = dir.path().join("execution.json");
    heartbeat.settle().unwrap();
    let record: ExecutionRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record.state, ExecutionState::Settled);
    assert!(execution::execution_is_live(&path).unwrap());
    let mut second = Command::new("/usr/bin/true");
    assert!(execution::spawn(&mut second, &transcript).is_err());
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
    assert!(!execution::execution_is_live(&path).unwrap());
}

#[test]
fn launch_handoff_cancel_serializes_with_payload_start() {
    use fluent::work_model::{
        AttemptCancellation, AttemptStatus, PauseKind, WorkItem, WorkModelStore,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = WorkModelStore::new(dir.path());
    let mut item = WorkItem::planned("work-1", "Preserve launch stop");
    item.add_initial_attempt("attempt-1").unwrap();
    store.create_work_item(&item).unwrap();
    let record = ExecutionRecord {
        schema_version: 2,
        state: ExecutionState::Launching,
        owner_pid: std::process::id(),
        leader_pid: 0,
        process_group_id: 0,
        started_at: "reserved".into(),
        heartbeat_at: "reserved".into(),
        settled_at: None,
        attempt_owner: Some(execution::AttemptExecutionOwner {
            project_root: dir.path().into(),
            work_item_id: "work-1".into(),
            attempt_id: "attempt-1".into(),
        }),
        launch_generation: Some("00000000000000000000000000000000".into()),
    };
    std::fs::write(
        dir.path().join("execution.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let barrier = dir.path().join("allow-exec");
    let script = format!("sleep 2; touch '{}'", dir.path().join("payload").display());
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "launch_handoff_owner_helper", "--nocapture"])
        .env("FLUENT_TEST_LAUNCH_DIR", dir.path())
        .env("FLUENT_TEST_LAUNCH_CHILD_BARRIER", &barrier)
        .env("FLUENT_TEST_LAUNCH_SCRIPT", script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let receipt = dir.path().join("execution-receipt");
    wait_for(&receipt);
    let path = dir.path().join("execution.json");
    assert!(execution::execution_is_live(&path).unwrap());
    owner.kill().unwrap();
    owner.wait().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let project = dir.path().to_path_buf();
    let canceler = std::thread::spawn(move || {
        let result = WorkModelStore::new(&project).mutate_work_item("work-1", |item| {
            let attempt = &mut item.attempts[0];
            attempt.cancellation = Some(AttemptCancellation {
                requested_at: fluent::work_model::now_iso8601(),
                settled_at: None,
                resumed_at: None,
            });
            attempt.status = AttemptStatus::NeedsUser;
            attempt.pause_kind = Some(PauseKind::Interrupted);
            Ok(())
        });
        tx.send(result).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
    assert!(!dir.path().join("payload").exists());
    std::fs::write(&barrier, b"go").unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    canceler.join().unwrap();
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
    assert!(!dir.path().join("payload").exists());
}

fn crashed_parent(dir: &Path) {
    let barrier = dir.join("parent-spawned");
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "launch_handoff_owner_helper", "--nocapture"])
        .env("FLUENT_TEST_LAUNCH_DIR", dir)
        .env("FLUENT_TEST_LAUNCH_PARENT_BARRIER", &barrier)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
    wait_for(&barrier);
}

#[test]
fn launch_handoff_publishes_identity_before_payload() {
    let dir = tempfile::tempdir().unwrap();
    crashed_parent(dir.path());
    wait_for(&dir.path().join("payload"));
    let path = dir.path().join("execution.json");
    let record: ExecutionRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record.state, ExecutionState::Launching);
    assert_eq!(record.schema_version, 2);
    let receipt = std::fs::read_to_string(dir.path().join("execution-receipt")).unwrap();
    assert!(receipt.starts_with(&format!("2 {} ", record.launch_generation.unwrap())));
    assert!(execution::execution_is_live(&path).unwrap());
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
}

#[test]
fn launch_handoff_recovers_child_after_owner_crash() {
    let dir = tempfile::tempdir().unwrap();
    crashed_parent(dir.path());
    let path = dir.path().join("execution.json");
    assert!(execution::execution_is_live(&path).unwrap());
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
    assert!(!execution::execution_is_live(&path).unwrap());
}

#[test]
fn launch_handoff_rejects_unbound_identity() {
    let dir = tempfile::tempdir().unwrap();
    crashed_parent(dir.path());
    let path = dir.path().join("execution.json");
    let receipt_path = dir.path().join("execution-receipt");
    let original = std::fs::read(&receipt_path).unwrap();
    std::fs::write(&receipt_path, b"partial").unwrap();
    assert!(
        execution::cancel(&path)
            .unwrap_err()
            .to_string()
            .contains("refusing to signal")
    );
    assert!(execution::execution_is_live(&path).unwrap());
    let mut other = Command::new("/usr/bin/true");
    assert!(execution::spawn(&mut other, &dir.path().join("transcript.jsonl")).is_err());
    std::fs::write(&receipt_path, original).unwrap();
    let saved = std::fs::read(&path).unwrap();
    let mut record: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    record["launch_generation"] = serde_json::json!("00000000000000000000000000000000");
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(execution::cancel(&path).is_err());
    std::fs::write(&path, &saved).unwrap();
    record = serde_json::from_slice(&saved).unwrap();
    record["leader_pid"] = serde_json::json!(std::process::id());
    record["process_group_id"] = serde_json::json!(std::process::id());
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(execution::cancel(&path).is_err());
    std::fs::write(&path, saved).unwrap();
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
}

#[test]
fn launch_handoff_failed_publication_never_runs_payload() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("execution-receipt")).unwrap();
    let marker = dir.path().join("payload");
    let mut command = Command::new("/usr/bin/touch");
    command.arg(&marker);
    assert!(execution::spawn(&mut command, &dir.path().join("transcript.jsonl")).is_err());
    assert!(!marker.exists());
    assert!(!execution::execution_is_live(&dir.path().join("execution.json")).unwrap());
}

#[test]
fn launch_handoff_legacy_records_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let path = dir.path().join("execution.json");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 30")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    execution::configure_identity_lock(&mut command, &transcript).unwrap();
    let mut child = command.spawn().unwrap();
    let record = ExecutionRecord {
        schema_version: 1,
        state: ExecutionState::Launching,
        owner_pid: std::process::id(),
        leader_pid: 0,
        process_group_id: 0,
        started_at: "legacy".into(),
        heartbeat_at: "legacy".into(),
        settled_at: None,
        attempt_owner: None,
        launch_generation: None,
    };
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(execution::execution_is_live(&path).unwrap());
    assert!(execution::cancel(&path).is_err());
    assert!(child.try_wait().unwrap().is_none());
    let mut known = record;
    known.state = ExecutionState::Running;
    known.leader_pid = child.id();
    known.process_group_id = child.id();
    std::fs::write(&path, serde_json::to_vec(&known).unwrap()).unwrap();
    assert_eq!(execution::cancel(&path).unwrap(), CancelOutcome::Canceled);
    child.wait().unwrap();
}

#[test]
fn launch_handoff_old_reader_rejects_new_protocol() {
    use fluent::work_model::{WorkItem, WorkModelStore};
    use sha2::{Digest, Sha256};
    let binary =
        std::path::PathBuf::from(std::env::var_os("FLUENT_LAUNCH_HANDOFF_OLD_BIN").expect(
            "build tests/fixtures/build-launch-handoff-old-reader.sh before this selection",
        ));
    let identity_path = binary
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("identity.json");
    let identity: serde_json::Value =
        serde_json::from_slice(&std::fs::read(identity_path).unwrap()).unwrap();
    assert_eq!(
        identity["source_commit"],
        "edc0febfba85b91c58ff753aefdc4a4606aaaa6e"
    );
    assert_eq!(
        identity["binary_sha256"],
        format!("{:x}", Sha256::digest(std::fs::read(&binary).unwrap()))
    );
    let dir = tempfile::tempdir().unwrap();
    let store = WorkModelStore::new(dir.path());
    let mut item = WorkItem::planned("launch-reader", "Preserve launch identity");
    item.add_initial_attempt("attempt-1").unwrap();
    store.create_work_item(&item).unwrap();
    let area = &item.attempts[0].tasks[0]
        .artifact_area
        .as_ref()
        .unwrap()
        .path;
    let artifact = dir.path().join(area);
    std::fs::create_dir_all(&artifact).unwrap();
    let execution = serde_json::json!({
        "schema_version": 2, "state": "launching", "owner_pid": 1,
        "leader_pid": 0, "process_group_id": 0, "started_at": "now",
        "heartbeat_at": "now", "launch_generation": "00000000000000000000000000000001"
    });
    std::fs::write(
        artifact.join("execution.json"),
        serde_json::to_vec(&execution).unwrap(),
    )
    .unwrap();
    let model_path = dir.path().join(".fluent/work/items/launch-reader.json");
    let before = std::fs::read(&model_path).unwrap();
    let execution_path = artifact.join("execution.json");
    let execution_before = std::fs::read(&execution_path).unwrap();
    for arguments in [
        vec!["attempt", "cancel", "launch-reader", "attempt-1"],
        vec!["work-item", "upgrade", "launch-reader"],
    ] {
        let output = Command::new(&binary)
            .current_dir(dir.path())
            .env("FLUENT_NO_UPDATE_CHECK", "1")
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("schema"));
        assert_eq!(std::fs::read(&model_path).unwrap(), before);
        assert_eq!(std::fs::read(&execution_path).unwrap(), execution_before);
    }
}

#[test]
fn launch_handoff_route_shared_spawn_preserves_command_context() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let marker = dir.path().join("route-output");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(r#"printf '%s:%s' "$FLUENT_ROUTE_VALUE" "$(pwd)" > "$FLUENT_ROUTE_MARKER""#)
        .env("FLUENT_ROUTE_VALUE", "kept")
        .env("FLUENT_ROUTE_MARKER", &marker)
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = execution::spawn(&mut command, &transcript).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("kept:{}", dir.path().canonicalize().unwrap().display())
    );
    let record: ExecutionRecord =
        serde_json::from_slice(&std::fs::read(dir.path().join("execution.json")).unwrap()).unwrap();
    assert_eq!(record.schema_version, 2);
    assert!(dir.path().join("execution-receipt").exists());
}

#[test]
fn launch_handoff_route_nested_commands_get_new_generations() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let path = dir.path().join("execution.json");
    let mut first = Command::new("/usr/bin/true");
    assert!(
        execution::spawn(&mut first, &transcript)
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
    let first: ExecutionRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut second = Command::new("/usr/bin/true");
    assert!(
        execution::spawn(&mut second, &transcript)
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
    let second: ExecutionRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_ne!(first.launch_generation, second.launch_generation);
}

#[test]
fn launch_handoff_route_siblings_keep_separate_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let launch = |name: &str| {
        let area = dir.path().join(name);
        std::fs::create_dir_all(&area).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = execution::spawn(&mut command, &area.join("transcript.jsonl")).unwrap();
        (child, area.join("execution.json"))
    };
    let (mut first, first_path) = launch("first");
    let (mut second, second_path) = launch("second");
    assert_eq!(
        execution::cancel(&first_path).unwrap(),
        CancelOutcome::Canceled
    );
    first.wait().unwrap();
    assert!(execution::execution_is_live(&second_path).unwrap());
    assert!(second.try_wait().unwrap().is_none());
    assert_eq!(
        execution::cancel(&second_path).unwrap(),
        CancelOutcome::Canceled
    );
    second.wait().unwrap();
}
