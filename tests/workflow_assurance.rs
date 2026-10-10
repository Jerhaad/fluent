use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn fluent(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fluent"))
        .current_dir(dir)
        .env("FLUENT_NO_UPDATE_CHECK", "1")
        .env("FLUENT_TEST_HOST_SANDBOX_PREFLIGHT", "pass")
        .args(args)
        .output()
        .unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn event(seq: u64, boundary: &str, operation: &str, predecessors: &[u64], facts: Value) -> Value {
    json!({"sequence":seq,"actor":"operator","operation-id":operation,
        "generation":null,"boundary":boundary,"causal-predecessors":predecessors,
        "raw-facts":facts})
}

fn capture() -> (TempDir, Value) {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    assert!(
        run(&project, "git", &["init", "-b", "main"])
            .status
            .success()
    );
    assert!(
        run(
            &project,
            "git",
            &["config", "user.email", "test@example.com"]
        )
        .status
        .success()
    );
    assert!(
        run(&project, "git", &["config", "user.name", "Test"])
            .status
            .success()
    );
    fs::write(project.join("README.md"), "fixture\n").unwrap();
    assert!(run(&project, "git", &["add", "README.md"]).status.success());
    assert!(
        run(&project, "git", &["commit", "-m", "Initialize fixture"])
            .status
            .success()
    );
    let work = format!(
        "foundation{}",
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(['.', '-'], "")
    );
    let create = fluent(
        &project,
        &["work-item", "create", &work, "--title", "Trace fixture"],
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let show1 = fluent(&project, &["work-item", "show", &work]);
    assert!(show1.status.success());
    let intake: Value = serde_json::from_slice(&show1.stdout).unwrap();
    let attempt = fluent(&project, &["attempt", "create", &work, "attempt-1"]);
    assert!(
        attempt.status.success(),
        "{}",
        String::from_utf8_lossy(&attempt.stderr)
    );
    let show2 = fluent(&project, &["work-item", "show", &work]);
    assert!(show2.status.success());
    let planned: Value = serde_json::from_slice(&show2.stdout).unwrap();
    let cancel = fluent(&project, &["attempt", "cancel", &work, "attempt-1"]);
    assert!(
        cancel.status.success(),
        "{}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    let show3 = fluent(&project, &["work-item", "show", &work]);
    assert!(show3.status.success());
    let canceled: Value = serde_json::from_slice(&show3.stdout).unwrap();
    let run_attempt = fluent(
        &project,
        &["task", "run", &work, "attempt-1", "attempt-1-write-1"],
    );
    let show4 = fluent(&project, &["work-item", "show", &work]);
    assert!(show4.status.success());
    let after_refusal: Value = serde_json::from_slice(&show4.stdout).unwrap();
    let artifacts = [
        (&show1, "intake.json"),
        (&show2, "planned.json"),
        (&show3, "canceled.json"),
        (&show4, "refused.json"),
    ]
    .into_iter()
    .map(|(out, path)| {
        fs::write(tmp.path().join(path), &out.stdout).unwrap();
        json!({"path":path,"sha256":digest(&out.stdout)})
    })
    .collect::<Vec<_>>();
    let binary = fs::read(env!("CARGO_BIN_EXE_fluent")).unwrap();
    let trace = json!({"schema-version":1,"scenario":"foundation-public-cancel",
    "fluent-commit":String::from_utf8_lossy(&run(Path::new(env!("CARGO_MANIFEST_DIR")),"git", &["rev-parse","HEAD"]).stdout).trim(),
    "fluent-binary-sha256":digest(&binary),"model-profile":"foundation",
    "collection-complete":true,"artifacts":artifacts,"events":[
        event(1,"intake","create-work",&[],json!({"exit-code":create.status.code(),"work":intake})),
        event(2,"attempt-create","create-attempt",&[1],json!({"exit-code":attempt.status.code(),"work":planned})),
        event(3,"cancel","cancel-attempt",&[2],json!({"exit-code":cancel.status.code(),"work":canceled})),
        event(4,"run-refused","run-attempt",&[3],json!({"exit-code":run_attempt.status.code(),"stdout":String::from_utf8_lossy(&run_attempt.stdout),"stderr":String::from_utf8_lossy(&run_attempt.stderr),"work":after_refusal}))
    ]});
    (tmp, trace)
}

fn project(trace: &Value, dir: &Path) -> Output {
    let path = dir.join("trace.json");
    fs::write(&path, serde_json::to_vec_pretty(trace).unwrap()).unwrap();
    Command::new("python3")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/formal/workflow/project.py"))
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn workflow_foundation_public_trace() {
    let (tmp, trace) = capture();
    let result = project(&trace, tmp.path());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/formal/workflow/foundation");
    fs::create_dir_all(&target).unwrap();
    fs::write(
        target.join("production-trace.json"),
        serde_json::to_vec_pretty(&trace).unwrap(),
    )
    .unwrap();
    for entry in trace["artifacts"].as_array().unwrap() {
        let name = entry["path"].as_str().unwrap();
        fs::copy(tmp.path().join(name), target.join(name)).unwrap();
    }
}

#[test]
fn workflow_foundation_projection_rejects_bad_observations() {
    let (tmp, trace) = capture();
    let original_canceled = fs::read(tmp.path().join("canceled.json")).unwrap();
    let reject = |bad: &Value, expected: &str| {
        let result = project(bad, tmp.path());
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    let mut incomplete = trace.clone();
    incomplete["collection-complete"] = json!(false);
    reject(&incomplete, "collection incomplete");
    let mut malformed = trace.clone();
    malformed["events"][1]["boundary"] = json!("unknown");
    reject(&malformed, "event identity or boundary differs");
    let mut ordering = trace.clone();
    ordering["events"][3]["causal-predecessors"] = json!([]);
    reject(&ordering, "missing or ambiguous causal predecessor");
    let mut unrelated_failure = trace.clone();
    unrelated_failure["events"][3]["raw-facts"]["stderr"] =
        json!("Error: task setup failed for another reason\n");
    reject(&unrelated_failure, "cancellation refusal not observed");
    let mut identity = trace.clone();
    identity["events"][2]["raw-facts"]["work"]["id"] = json!("other");
    let canceled_bytes = serde_json::to_vec(&identity["events"][2]["raw-facts"]["work"]).unwrap();
    fs::write(tmp.path().join("canceled.json"), &canceled_bytes).unwrap();
    identity["artifacts"][2]["sha256"] = json!(digest(&canceled_bytes));
    reject(&identity, "work identity changed");
    fs::write(tmp.path().join("canceled.json"), &original_canceled).unwrap();
    let mut invented = trace.clone();
    invented["events"][3]["raw-facts"]["work"]["attempts"][0]["status"] = json!("complete");
    let refused_bytes = serde_json::to_vec(&invented["events"][3]["raw-facts"]["work"]).unwrap();
    fs::write(tmp.path().join("refused.json"), &refused_bytes).unwrap();
    invented["artifacts"][3]["sha256"] = json!(digest(&refused_bytes));
    reject(&invented, "run advanced canceled state");
}

#[test]
fn workflow_release_build_public_workflow() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let built = run(
        root,
        "cargo",
        &["build", "--offline", "--release", "--no-default-features"],
    );
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let binary = root.join("target/release/fluent");
    let version = run(root, binary.to_str().unwrap(), &["version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("fluent 0.6.0"));
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    assert!(
        run(project, "git", &["init", "-b", "main"])
            .status
            .success()
    );
    assert!(
        run(
            project,
            "git",
            &["config", "user.email", "test@example.com"]
        )
        .status
        .success()
    );
    assert!(
        run(project, "git", &["config", "user.name", "Test"])
            .status
            .success()
    );
    fs::write(project.join("README.md"), "fixture\n").unwrap();
    assert!(run(project, "git", &["add", "README.md"]).status.success());
    assert!(
        run(project, "git", &["commit", "-m", "Initialize fixture"])
            .status
            .success()
    );
    for args in [
        vec!["init"],
        vec![
            "work-item",
            "create",
            "ordinary-release",
            "--title",
            "Ordinary release",
        ],
        vec!["attempt", "create", "ordinary-release", "attempt-1"],
        vec!["attempt", "cancel", "ordinary-release", "attempt-1"],
    ] {
        let result = run(project, binary.to_str().unwrap(), &args);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let shown = run(
        project,
        binary.to_str().unwrap(),
        &["work-item", "show", "ordinary-release"],
    );
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let work: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(work["attempts"][0]["status"], "needs-user");
    assert!(work["attempts"][0]["cancellation"]["requested_at"].is_string());
    let refused = run(
        project,
        binary.to_str().unwrap(),
        &[
            "task",
            "run",
            "ordinary-release",
            "attempt-1",
            "attempt-1-write-1",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("cancel"));
    let after = run(
        project,
        binary.to_str().unwrap(),
        &["work-item", "show", "ordinary-release"],
    );
    assert!(after.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&after.stdout).unwrap(),
        work
    );
    let queued = run(
        project,
        binary.to_str().unwrap(),
        &[
            "work-item",
            "create",
            "queued-release",
            "--title",
            "Queued release",
        ],
    );
    assert!(
        queued.status.success(),
        "{}",
        String::from_utf8_lossy(&queued.stderr)
    );
    let add = run(
        project,
        binary.to_str().unwrap(),
        &["queue", "add", "queued-release"],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let list = run(project, binary.to_str().unwrap(), &["queue", "list"]);
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("queued-release"));
}

#[test]
fn workflow_assurance_report_rejects_missing_and_misbound_evidence() {
    let tmp = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let report = source.join("tests/formal/workflow/report.py");
    let output = tmp.path().join("report.json");
    let args = [
        "--results",
        tmp.path().to_str().unwrap(),
        "--candidate",
        "candidate-a",
        "--binary",
        env!("CARGO_BIN_EXE_fluent"),
        "--output",
        output.to_str().unwrap(),
    ];
    let command = std::iter::once(report.to_str().unwrap())
        .chain(args)
        .collect::<Vec<_>>();
    let missing = run(source, "python3", &command);
    assert!(!missing.status.success());
    let result: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(result["pass"], false);
    assert!(result["missing"].as_array().unwrap().len() > 0);
    assert!(
        result["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap().starts_with("connection: "))
    );
    let suite = tmp.path().join("foundation");
    fs::create_dir(&suite).unwrap();
    fs::write(suite.join("case.log"), "Model checking completed\n").unwrap();
    fs::write(
        suite.join("result.json"),
        json!({"schema-version":1,"suite":"foundation",
        "candidate-commit":"candidate-b",
        "fluent-binary-sha256":digest(&fs::read(env!("CARGO_BIN_EXE_fluent")).unwrap()),
        "pass":true,
        "cases":[{"case":"foundation.cfg","pass":true,"log":"case.log"}]})
        .to_string(),
    )
    .unwrap();
    let misbound = run(source, "python3", &command);
    assert!(!misbound.status.success());
    let result: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(result["suites"]["foundation"]["pass"], false);
    let partial = json!({"schema-version":1,"suite":"foundation",
        "candidate-commit":"candidate-a",
        "fluent-binary-sha256":digest(&fs::read(env!("CARGO_BIN_EXE_fluent")).unwrap()),
        "pass":true,
        "cases":[{"case":"foundation.cfg","pass":true,"log":"case.log"}]});
    fs::write(suite.join("result.json"), partial.to_string()).unwrap();
    let incomplete = run(source, "python3", &command);
    assert!(!incomplete.status.success());
    let result: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(result["suites"]["foundation"]["pass"], false);
}

#[test]
fn workflow_integrated_rejects_missing_model_tool() {
    let tmp = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checker = source.join("tests/formal/workflow/check.py");
    let missing_jar = tmp.path().join("missing.jar");
    let result = run(
        source,
        "python3",
        &[
            checker.to_str().unwrap(),
            "--suite=integrated",
            &format!("--jar={}", missing_jar.display()),
        ],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("pinned TLC 1.7.4 JAR"));
}

#[test]
fn workflow_integrated_queue_survives_other_attempt_cancellation() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    assert!(
        run(project, "git", &["init", "-b", "main"])
            .status
            .success()
    );
    assert!(
        run(
            project,
            "git",
            &["config", "user.email", "test@example.com"]
        )
        .status
        .success()
    );
    assert!(
        run(project, "git", &["config", "user.name", "Test"])
            .status
            .success()
    );
    fs::write(project.join("README.md"), "fixture\n").unwrap();
    assert!(run(project, "git", &["add", "README.md"]).status.success());
    assert!(
        run(project, "git", &["commit", "-m", "Initialize fixture"])
            .status
            .success()
    );
    assert!(fluent(project, &["init"]).status.success());
    for work in ["foreground", "queued"] {
        let created = fluent(project, &["work-item", "create", work, "--title", work]);
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    let attempt = fluent(project, &["attempt", "create", "foreground", "attempt-1"]);
    assert!(
        attempt.status.success(),
        "{}",
        String::from_utf8_lossy(&attempt.stderr)
    );
    let queued = fluent(project, &["queue", "add", "queued"]);
    assert!(
        queued.status.success(),
        "{}",
        String::from_utf8_lossy(&queued.stderr)
    );
    let before_queue = fluent(project, &["work-item", "show", "queued"]);
    assert!(before_queue.status.success());
    let cancel = fluent(project, &["attempt", "cancel", "foreground", "attempt-1"]);
    assert!(
        cancel.status.success(),
        "{}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    let foreground = fluent(project, &["work-item", "show", "foreground"]);
    assert!(foreground.status.success());
    let work: Value = serde_json::from_slice(&foreground.stdout).unwrap();
    assert_eq!(work["attempts"][0]["status"], "needs-user");
    assert!(work["attempts"][0]["cancellation"]["requested_at"].is_string());
    let queue = fluent(project, &["queue", "list"]);
    assert!(queue.status.success());
    assert!(String::from_utf8_lossy(&queue.stdout).contains("queued"));
    let status = fluent(project, &["status"]);
    assert!(status.status.success());
    let after_queue = fluent(project, &["work-item", "show", "queued"]);
    assert!(after_queue.status.success());
    assert_eq!(before_queue.stdout, after_queue.stdout);
    let refused = fluent(
        project,
        &[
            "task",
            "run",
            "foreground",
            "attempt-1",
            "attempt-1-write-1",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("cancel"));
    let after_foreground = fluent(project, &["work-item", "show", "foreground"]);
    assert!(after_foreground.status.success());
    assert_eq!(foreground.stdout, after_foreground.stdout);
}
