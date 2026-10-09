use fluent::content::ContentResolver;
use fluent::git;
use fluent::work_merge_executor::{WorkMergeConfig, merge_candidate};
use fluent::work_model::{
    AttemptCancellation, AttemptLearning, AttemptReviewState, AttemptStatus, LearnerMode,
    PauseKind, TaskOutput, TaskStatus, WorkItem, WorkModelStore,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct LandingFixture {
    _tmp: TempDir,
    root: PathBuf,
    store: WorkModelStore,
    candidate: String,
    mode: LearnerMode,
    bin: PathBuf,
}

fn git_run(root: &Path, args: &[&str]) {
    git::run(root, args, "landing recovery fixture").unwrap();
}

fn head(root: &Path) -> String {
    git::run_stdout(root, &["rev-parse", "main"], "target tip").unwrap()
}

fn fixture() -> LandingFixture {
    fixture_mode(LearnerMode::NoExpertise)
}

fn fixture_mode(mode: LearnerMode) -> LandingFixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir(&root).unwrap();
    git_run(&root, &["init", "-q", "-b", "main"]);
    git_run(&root, &["config", "user.email", "test@example.com"]);
    git_run(&root, &["config", "user.name", "Test"]);
    fs::write(root.join("file.txt"), "base\n").unwrap();
    fs::create_dir(root.join(".fluent")).unwrap();
    fs::write(
        root.join(".fluent/tester.yaml"),
        "commands:\n  - command: true\n    test_harness: shell-harness\n",
    )
    .unwrap();
    let extractor = root.join(".fluent/extract-tester-results");
    fs::write(&extractor, "#!/bin/sh\nprintf '[]\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-q", "-m", "Add baseline"]);
    git_run(&root, &["checkout", "-q", "-b", "work/attempt-1"]);
    fs::write(root.join("file.txt"), "reviewed\n").unwrap();
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-q", "-m", "Add reviewed change"]);
    let candidate = git::run_stdout(&root, &["rev-parse", "HEAD"], "candidate tip").unwrap();
    git_run(&root, &["checkout", "-q", "main"]);
    let workspace = tmp.path().join("work-6-work-1-attempt-1");
    git_run(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            workspace.to_str().unwrap(),
            &candidate,
        ],
    );

    let store = WorkModelStore::new(&root);
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let claude = bin.join("claude");
    fs::write(
        &claude,
        r##"#!/bin/bash
if [ "${1:-} ${2:-} ${3:-}" = "auth status --json" ]; then printf '{"loggedIn":true}\n'; exit 0; fi
if [ "${1:-}" = "--version" ]; then printf '2.1.211 (Claude Code)\n'; exit 0; fi
prompt=""
previous=""
for arg in "$@"; do
  if [ "$previous" = "-p" ]; then prompt="$arg"; break; fi
  previous="$arg"
done
printf 'call\n' >> "$0.calls"
if printf '%s' "$prompt" | grep -q 'Rebase the candidate branch'; then
  target=$(printf '%s' "$prompt" | grep -o 'onto `[^`]*`' | sed 's/onto `//;s/`//')
  git rebase "$target"
fi
"##,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut item = WorkItem {
        id: "work-1".into(),
        title: "Landing recovery".into(),
        learner_mode: mode,
        ..Default::default()
    };
    item.add_initial_attempt("attempt-1").unwrap();
    let handoff_path = root.join(".fluent/work/artifacts/work-1/attempt-1/learner/handoff.json");
    fs::create_dir_all(handoff_path.parent().unwrap()).unwrap();
    let mut handoff = fluent::follow_up::LearnerHandoffV1::new(
        "work-1",
        "attempt-1",
        fluent::follow_up::LearningRecord {
            summary: "Verified landing".into(),
            expertise: Vec::new(),
        },
    );
    handoff.source_merge_candidate_id = Some("attempt-1-merge-candidate".into());
    let handoff_bytes = serde_json::to_vec(&handoff).unwrap();
    fs::write(&handoff_path, &handoff_bytes).unwrap();
    let attempt = &mut item.attempts[0];
    attempt.status = AttemptStatus::Complete;
    attempt.review_state = Some(AttemptReviewState::Passed);
    attempt.learning = Some(AttemptLearning::succeeded(
        1,
        fluent::follow_up::ArtifactRef {
            path: ".fluent/work/artifacts/work-1/attempt-1/learner/handoff.json".into(),
            digest: fluent::follow_up::content_digest(&handoff_bytes),
        },
    ));
    let write = &mut attempt.tasks[0];
    write.status = TaskStatus::Complete;
    write.output = Some(TaskOutput {
        workspace_id: write.workspace_access.writes[0].id.clone(),
        workspace_path: "../work-6-work-1-attempt-1".into(),
        source_branch: "main".into(),
        base_commit: None,
        commit: candidate.clone(),
        no_change: None,
        learner_canonicalization: None,
    });
    item.create_or_get_merge_candidate("attempt-1").unwrap();
    store.create_work_item(&item).unwrap();
    LandingFixture {
        _tmp: tmp,
        root,
        store,
        candidate,
        mode,
        bin,
    }
}

fn land(fx: &LandingFixture) -> anyhow::Result<String> {
    let old_path = std::env::var_os("PATH");
    if fx.mode == LearnerMode::Capture {
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    fx.bin.display(),
                    old_path.as_ref().unwrap().to_string_lossy()
                ),
            );
            std::env::set_var("FLUENT_TEST_HERMETIC_PROVIDERS", "1");
        }
    }
    let resolver = ContentResolver::new(None);
    let result = merge_candidate(WorkMergeConfig {
        project_root: &fx.root,
        store: &fx.store,
        work_item_id: "work-1",
        merge_candidate_id: "attempt-1-merge-candidate",
        resolver: &resolver,
        extra_args: &[],
        coder_kind: if fx.mode == LearnerMode::Capture {
            fluent::coder::CoderKind::Claude
        } else {
            fluent::coder::CoderKind::Codex
        },
        coder_override: None,
        model: None,
        effort: None,
        use_attempt_mapping: false,
        no_sandbox: true,
        run_post_merge_review: false,
    });
    if fx.mode == LearnerMode::Capture {
        unsafe {
            std::env::set_var("PATH", old_path.unwrap());
            std::env::remove_var("FLUENT_TEST_HERMETIC_PROVIDERS");
        }
    }
    Ok(result?.merged_commit)
}

// The environment seam is available only in test-support builds. These tests
// use a disposable Git repository and invoke the same public land executor.
fn interrupt(fx: &LandingFixture, point: &str) {
    let key = format!("FLUENT_TEST_INTERRUPT_{point}_LANDING_GIT");
    unsafe { std::env::set_var(&key, "1") };
    let result = land(fx);
    unsafe { std::env::remove_var(&key) };
    assert!(result.is_err(), "injection must interrupt landing");
}

#[test]
fn landing_recovery_persists_intent_before_git_effect() {
    let fx = fixture();
    let before = head(&fx.root);
    interrupt(&fx, "BEFORE");
    assert_eq!(head(&fx.root), before);
    let item = fx.store.read_work_item("work-1").unwrap();
    let intent = item.merge_candidates[0]
        .merge_state
        .landing_intent
        .as_ref()
        .unwrap();
    assert_eq!(intent.verified_commit, fx.candidate);
    assert_eq!(intent.target_before, before);
    assert_eq!(intent.target_branch, "main");
    assert_eq!(intent.learner_mode, LearnerMode::NoExpertise);
    let status = fluent::work_status::summarize_work_item(&item, Some(&fx.root));
    assert_eq!(status.action, "landing-pending");
    assert!(status.merge.starts_with("publication-pending"));
    assert_eq!(head(&fx.root), before, "status cannot replay landing");
}

#[test]
fn landing_recovery_recognizes_effect_after_owner_crash() {
    let fx = fixture();
    interrupt(&fx, "AFTER");
    assert_eq!(head(&fx.root), fx.candidate);
    assert_eq!(land(&fx).unwrap(), fx.candidate);
    let item = fx.store.read_work_item("work-1").unwrap();
    assert_eq!(
        item.merge_candidates[0]
            .merge_state
            .merged_commit
            .as_deref(),
        Some(fx.candidate.as_str())
    );
}

#[test]
fn landing_recovery_preserves_later_target_commits() {
    let fx = fixture();
    interrupt(&fx, "AFTER");
    fs::write(fx.root.join("later.txt"), "later\n").unwrap();
    git_run(&fx.root, &["add", "."]);
    git_run(&fx.root, &["commit", "-q", "-m", "Add later change"]);
    let later = head(&fx.root);
    assert_eq!(land(&fx).unwrap(), fx.candidate);
    assert_eq!(head(&fx.root), later);
}

#[test]
fn landing_recovery_replay_preserves_operation_and_evidence() {
    let fx = fixture();
    interrupt(&fx, "AFTER");
    let before = fx.store.read_work_item("work-1").unwrap();
    let intent = before.merge_candidates[0]
        .merge_state
        .landing_intent
        .clone();
    land(&fx).unwrap();
    let after = fx.store.read_work_item("work-1").unwrap();
    assert_eq!(after.merge_candidates[0].merge_state.landing_intent, intent);
    assert_eq!(after.attempts.len(), before.attempts.len());

    let fx = fixture_mode(LearnerMode::Capture);
    interrupt(&fx, "AFTER");
    let calls = fs::read(fx.bin.join("claude.calls")).unwrap();
    let before = fx.store.read_work_item("work-1").unwrap();
    let intent = before.merge_candidates[0]
        .merge_state
        .landing_intent
        .clone();
    land(&fx).unwrap();
    assert_eq!(fs::read(fx.bin.join("claude.calls")).unwrap(), calls);
    let after = fx.store.read_work_item("work-1").unwrap();
    assert_eq!(after.merge_candidates[0].merge_state.landing_intent, intent);
    assert_eq!(after.attempts.len(), before.attempts.len());
}

#[test]
fn landing_recovery_rejects_unbound_or_changed_intent() {
    for change in ["branch", "repository", "evidence", "candidate"] {
        let fx = fixture();
        let before = head(&fx.root);
        interrupt(&fx, "BEFORE");
        let mut item = fx.store.read_work_item("work-1").unwrap();
        let intent = item.merge_candidates[0]
            .merge_state
            .landing_intent
            .as_mut()
            .unwrap();
        match change {
            "branch" => intent.target_branch = "other".into(),
            "repository" => intent.repository = "/other/repository".into(),
            "evidence" => intent.evidence_digest = "substituted".into(),
            "candidate" => intent.verified_commit = before.clone(),
            _ => unreachable!(),
        }
        if change == "candidate" {
            let child = fx
                .root
                .join(".fluent/work/merge-candidates/work-1/attempt-1-merge-candidate.json");
            fs::write(
                &child,
                serde_json::to_vec_pretty(&item.merge_candidates[0]).unwrap(),
            )
            .unwrap();
        } else {
            fx.store.write_work_item(&item).unwrap();
        }
        assert!(land(&fx).is_err(), "{change} must block landing");
        assert_eq!(head(&fx.root), before, "{change} changed target");
    }
    // An old crash can leave Executing after Git has already advanced, but
    // without the new authority it cannot publish a successful result.
    let fx = fixture();
    let mut item = fx.store.read_work_item("work-1").unwrap();
    item.merge_candidates[0].merge_state.status =
        fluent::work_model::MergeCandidateMergeStatus::Executing;
    fx.store.write_work_item(&item).unwrap();
    git_run(&fx.root, &["merge", "--ff-only", &fx.candidate]);
    assert!(land(&fx).is_err());
    assert_eq!(head(&fx.root), fx.candidate);

    let fx = fixture_mode(LearnerMode::Capture);
    let before = head(&fx.root);
    interrupt(&fx, "BEFORE");
    let item = fx.store.read_work_item("work-1").unwrap();
    let artifact = &item.merge_candidates[0]
        .merge_state
        .landing_intent
        .as_ref()
        .unwrap()
        .check_artifacts[0];
    let path = fx.root.join(&artifact.path);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"\nchanged evidence\n");
    fs::write(&path, bytes).unwrap();
    assert!(land(&fx).is_err());
    assert_eq!(head(&fx.root), before);
}

#[test]
fn landing_recovery_preserves_cancellation_boundary() {
    let fx = fixture();
    let before = head(&fx.root);
    interrupt(&fx, "BEFORE");
    let mut item = fx.store.read_work_item("work-1").unwrap();
    item.attempts[0].cancellation = Some(AttemptCancellation {
        requested_at: fluent::work_model::now_iso8601(),
        settled_at: None,
        resumed_at: None,
    });
    item.attempts[0].status = AttemptStatus::NeedsUser;
    item.attempts[0].pause_kind = Some(PauseKind::Interrupted);
    fx.store.write_work_item(&item).unwrap();
    assert!(land(&fx).is_err());
    assert_eq!(head(&fx.root), before);
    assert!(fx.store.read_work_item("work-1").unwrap().attempts[0].cancellation_requested());

    let fx = fixture();
    interrupt(&fx, "AFTER");
    let mut item = fx.store.read_work_item("work-1").unwrap();
    item.attempts[0].cancellation = Some(AttemptCancellation {
        requested_at: fluent::work_model::now_iso8601(),
        settled_at: None,
        resumed_at: None,
    });
    item.attempts[0].status = AttemptStatus::NeedsUser;
    item.attempts[0].pause_kind = Some(PauseKind::Interrupted);
    fx.store.write_work_item(&item).unwrap();
    assert_eq!(land(&fx).unwrap(), fx.candidate);
    let after = fx.store.read_work_item("work-1").unwrap();
    assert!(after.attempts[0].cancellation_requested());
    assert_eq!(
        after.merge_candidates[0]
            .merge_state
            .merged_commit
            .as_deref(),
        Some(fx.candidate.as_str())
    );
}

#[test]
fn landing_recovery_replays_partial_work_publication() {
    for stage in [
        "after-attempt-record",
        "after-task-record",
        "before-root-publication",
    ] {
        let fx = fixture();
        interrupt(&fx, "AFTER");
        let mut item = fx.store.read_work_item("work-1").unwrap();
        item.attempts[0].cancellation = Some(AttemptCancellation {
            requested_at: fluent::work_model::now_iso8601(),
            settled_at: None,
            resumed_at: None,
        });
        item.attempts[0].status = AttemptStatus::NeedsUser;
        item.attempts[0].pause_kind = Some(PauseKind::Interrupted);
        fx.store.write_work_item(&item).unwrap();
        unsafe {
            std::env::set_var(
                "FLUENT_TEST_WORK_MODEL_INTERRUPT",
                format!("{stage}:work-1"),
            )
        };
        let first = land(&fx);
        unsafe { std::env::remove_var("FLUENT_TEST_WORK_MODEL_INTERRUPT") };
        assert!(first.is_err(), "{stage} must interrupt publication");
        let transaction = fx.root.join(".fluent/work/transactions/work-1.json");
        let root = fx.root.join(".fluent/work/items/work-1.json");
        let pending_bytes = fs::read(&transaction).unwrap();
        let root_bytes = fs::read(&root).unwrap();
        let _ = fx.store.read_work_item_snapshot("work-1");
        assert_eq!(fs::read(&transaction).unwrap(), pending_bytes);
        assert_eq!(fs::read(&root).unwrap(), root_bytes);
        let target = head(&fx.root);
        assert_eq!(target, fx.candidate);
        assert_eq!(land(&fx).unwrap(), fx.candidate);
        assert_eq!(head(&fx.root), target);
        let after = fx.store.read_work_item("work-1").unwrap();
        assert!(after.attempts[0].cancellation_requested());
        assert_eq!(
            after.merge_candidates[0]
                .merge_state
                .merged_commit
                .as_deref(),
            Some(fx.candidate.as_str())
        );
    }
}

#[test]
fn landing_recovery_preserves_both_learner_modes() {
    for mode in [LearnerMode::NoExpertise, LearnerMode::Capture] {
        let fx = fixture_mode(mode);
        interrupt(&fx, "AFTER");
        let before = fx.store.read_work_item("work-1").unwrap();
        let intent = before.merge_candidates[0]
            .merge_state
            .landing_intent
            .clone()
            .unwrap();
        assert_eq!(intent.learner_mode, mode);
        assert_eq!(intent.verified_commit, fx.candidate);
        assert_eq!(land(&fx).unwrap(), fx.candidate);
        let after = fx.store.read_work_item("work-1").unwrap();
        assert_eq!(after.merge_candidates[0].candidate_commit, fx.candidate);
        assert_eq!(
            after.merge_candidates[0].merge_state.landing_intent,
            Some(intent)
        );
        assert!(
            after.merge_candidates[0]
                .merge_state
                .follow_up_failure
                .is_none()
        );
        assert_eq!(
            after.attempts[0].tasks.len(),
            before.attempts[0].tasks.len()
        );
        assert!(
            !fx.root
                .parent()
                .unwrap()
                .join("work-6-work-1-attempt-1")
                .exists()
        );
        assert_eq!(land(&fx).unwrap(), fx.candidate);
    }
}

#[test]
fn landing_recovery_old_reader_rejects_new_intent() {
    let old_bin = PathBuf::from(
        std::env::var("FLUENT_LANDING_RECOVERY_OLD_BIN")
            .expect("prepare the pinned predecessor before this test"),
    );
    let identity: serde_json::Value = serde_json::from_slice(
        &fs::read(
            old_bin
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("identity.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        identity["source_commit"],
        "dfa960cd3bb7ba2087ab8562c728755e75d37f04"
    );
    assert_eq!(
        identity["binary_sha256"],
        format!("{:x}", Sha256::digest(fs::read(&old_bin).unwrap()))
    );
    assert_eq!(
        identity["binary_sha256"],
        "909fd3f12d6ff6ed1cc26b31c55c320022ab8725cc54fbaf5dac8f306ae18cd3"
    );
    let fx = fixture();
    interrupt(&fx, "BEFORE");
    let root = fx.root.join(".fluent/work/items/work-1.json");
    let child = fx
        .root
        .join(".fluent/work/merge-candidates/work-1/attempt-1-merge-candidate.json");
    let before_root = fs::read(&root).unwrap();
    let before_child = fs::read(&child).unwrap();
    let before_tip = head(&fx.root);
    let output = Command::new(&old_bin)
        .current_dir(&fx.root)
        .args(["work-item", "upgrade", "work-1"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "older writer accepted new landing authority"
    );
    assert_eq!(fs::read(&root).unwrap(), before_root);
    assert_eq!(fs::read(&child).unwrap(), before_child);
    assert_eq!(head(&fx.root), before_tip);

    // Restore only the predecessor's record and writer envelope versions.
    // The retained child authority must still reject the old reader.
    let mut root_value: serde_json::Value = serde_json::from_slice(&before_root).unwrap();
    root_value["schema_version"] = 11.into();
    root_value["model_writer_version"]["schema_version"] = 9.into();
    fs::write(&root, serde_json::to_vec_pretty(&root_value).unwrap()).unwrap();
    let attempts = fx.root.join(".fluent/work/attempts/work-1");
    for entry in fs::read_dir(&attempts).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["schema_version"] = 11.into();
            fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
    }
    let inner_root = fs::read(&root).unwrap();
    let inner_child = fs::read(&child).unwrap();
    let output = Command::new(&old_bin)
        .current_dir(&fx.root)
        .args(["work-item", "upgrade", "work-1"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "older writer accepted the landing child"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("landing-pending"),
        "the child status must reject the old reader: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&root).unwrap(), inner_root);
    assert_eq!(fs::read(&child).unwrap(), inner_child);
}
