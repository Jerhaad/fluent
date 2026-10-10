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
