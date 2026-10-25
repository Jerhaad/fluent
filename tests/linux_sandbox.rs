//! Prove the Landlock launcher confines the command it runs.
//!
//! Rendering the right policy and enforcing it are separate failures, and the
//! unit tests only cover the first. These run the shipped `sandbox-run`
//! launcher against a real kernel.
//!
//! Landlock is commonly compiled in but left out of the boot `lsm=` list, and
//! on such a host the launcher refuses to start rather than running a coder
//! unconfined. Every assertion below therefore names which of the two outcomes
//! it expects: a forbidden action that failed because the launcher never
//! started proves nothing about confinement, and asserting only "it failed"
//! would pass on a host with no sandbox at all.
//!
//! SPDX-License-Identifier: MIT
//! Added by the Linux fork; see NOTICE.md.

#![cfg(target_os = "linux")]

use fluent::coder::CoderKind;
use fluent::linux_sandbox;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Emitted when the kernel enforces nothing; the launcher exits on it.
const NOT_ENFORCED: &str = "enforced no part";

fn kernel_enforces_landlock() -> bool {
    linux_sandbox::is_available()
}

struct Fixture {
    _home: TempDir,
    home: PathBuf,
    policy: tempfile::NamedTempFile,
}

/// Build the fixture under the real home rather than the system temp tree.
///
/// The rendered policy grants `/tmp` read-write, so a fixture home there is not
/// outside the sandbox at all and the escape assertions below would be checking
/// nothing.
fn fixture_root() -> TempDir {
    match std::env::var_os("HOME") {
        Some(home) => TempDir::new_in(home).expect("creating a fixture under HOME"),
        None => TempDir::new().unwrap(),
    }
}

fn fixture(grant_shared_temp: bool) -> Fixture {
    let home = fixture_root();
    let home_path = home.path().to_path_buf();

    std::fs::create_dir(home_path.join(".ssh")).unwrap();
    std::fs::write(home_path.join(".ssh/id_rsa"), "PRIVATE KEY").unwrap();
    std::fs::create_dir(home_path.join("project")).unwrap();
    std::fs::write(home_path.join("project/tracked.txt"), "candidate").unwrap();

    let writable = vec![home_path.join("project")];
    let policy = linux_sandbox::render(&linux_sandbox::PolicyRequest {
        home: &home_path,
        writable_roots: &writable,
        readable_roots: &[],
        denied_write_roots: &[],
        coder_kind: Some(CoderKind::Claude),
        codex_home: None,
        grant_shared_temp,
        denied_write_paths: &[],
        denied_read_paths: &[],
        reallowed_write_paths: &[],
    })
    .unwrap();

    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), linux_sandbox::serialize(&policy).unwrap()).unwrap();

    Fixture {
        _home: home,
        home: home_path,
        policy: file,
    }
}

/// Run `script` under the launcher and report whether it succeeded.
fn run_confined(policy: &Path, script: &str) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_fluent"))
        .args(["sandbox-run", "--policy"])
        .arg(policy)
        .args(["--", "/bin/sh", "-c", script])
        .output()
        .expect("spawning the sandbox launcher");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stderr)
}

/// Assert that `script` was stopped by the kernel rather than by a launcher
/// that never confined anything.
fn assert_denied(policy: &Path, script: &str, what: &str) {
    let (ok, stderr) = run_confined(policy, script);
    assert!(!ok, "{what}");
    if kernel_enforces_landlock() {
        assert!(
            !stderr.contains(NOT_ENFORCED),
            "{what} — but the launcher refused to start, so nothing was confined: {stderr}"
        );
    }
}

#[test]
fn a_confined_command_reads_and_writes_inside_its_root() {
    let fixture = fixture(true);
    let project = fixture.home.join("project");
    let script = format!(
        "cat {0}/tracked.txt && echo written > {0}/new.txt",
        project.display()
    );

    let (ok, stderr) = run_confined(fixture.policy.path(), &script);

    if kernel_enforces_landlock() {
        assert!(ok, "the sandbox blocked its own writable root: {stderr}");
        assert_eq!(
            std::fs::read_to_string(project.join("new.txt")).unwrap(),
            "written\n"
        );
    } else {
        assert!(!ok, "a kernel with no Landlock ran the command anyway");
        assert!(stderr.contains(NOT_ENFORCED), "{stderr}");
    }
}

#[test]
fn a_confined_command_cannot_read_a_withheld_home_secret() {
    let fixture = fixture(true);

    assert_denied(
        fixture.policy.path(),
        &format!("cat {}/.ssh/id_rsa", fixture.home.display()),
        "the private key was readable inside the sandbox",
    );
}

#[test]
fn a_confined_command_cannot_write_outside_its_root() {
    let fixture = fixture(true);

    assert_denied(
        fixture.policy.path(),
        &format!("echo escaped > {}/escaped.txt", fixture.home.display()),
        "a write landed outside every writable root",
    );
    assert!(!fixture.home.join("escaped.txt").exists());
}

#[test]
fn a_handoff_policy_closes_the_shared_temp_escape_hatch() {
    let fixture = fixture(false);

    assert_denied(
        fixture.policy.path(),
        "echo escaped > /tmp/fluent-handoff-escape.txt",
        "a handoff-only policy left /tmp writable",
    );
}

#[test]
fn the_launcher_refuses_a_policy_it_cannot_parse() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "{ not a policy }").unwrap();

    let (ok, stderr) = run_confined(file.path(), "true");

    assert!(!ok, "an unparsable policy ran the command anyway");
    assert!(stderr.contains("Landlock policy"), "{stderr}");
}

#[test]
fn linux_writer_confinement_matches_seatbelt_probe() {
    // The fixture lives under the shared temp tree, so the temporary-directory
    // grant encloses the source checkout. Upstream's Seatbelt profile denies
    // the source entries after that broad grant, and the Linux profile has to
    // carve them out of it instead.
    let root = tempfile::TempDir::new().unwrap();
    let physical_source = root.path().join("physical-source");
    let source_link = root.path().join("source");
    let candidate = root.path().join("candidate");
    let artifact = source_link.join(".fluent/work/artifacts/work/attempt/task");
    let cache = source_link.join(".fluent/work/cache/writers/work/attempt");
    let sibling_artifact = source_link.join(".fluent/work/artifacts/work/attempt/other-task");
    let gate_artifact = source_link.join(".fluent/work/artifacts/work/attempt/candidate-gates");
    let sibling_cache = source_link.join(".fluent/work/cache/writers/work/other-attempt");
    let work_model = source_link.join(".fluent/work/items/work.json");

    std::fs::create_dir_all(&physical_source).unwrap();
    std::os::unix::fs::symlink(&physical_source, &source_link).unwrap();
    std::fs::create_dir_all(&candidate).unwrap();
    std::fs::create_dir_all(physical_source.join("src")).unwrap();
    std::fs::write(physical_source.join("src/lib.rs"), "source").unwrap();
    std::fs::create_dir_all(physical_source.join(".git")).unwrap();
    std::fs::create_dir_all(physical_source.join(".fluent/work/artifacts/work/attempt/task"))
        .unwrap();
    std::fs::create_dir_all(physical_source.join(".fluent/work/artifacts/work/attempt/other-task"))
        .unwrap();
    std::fs::create_dir_all(
        physical_source.join(".fluent/work/artifacts/work/attempt/candidate-gates"),
    )
    .unwrap();
    std::fs::create_dir_all(physical_source.join(".fluent/work/cache/writers/work/attempt"))
        .unwrap();
    std::fs::create_dir_all(physical_source.join(".fluent/work/cache/writers/work/other-attempt"))
        .unwrap();
    std::fs::create_dir_all(physical_source.join(".fluent/work/items")).unwrap();
    std::fs::write(
        physical_source.join(".fluent/work/items/work.json"),
        "host-owned",
    )
    .unwrap();

    // A symlink outside the shared temp tree points at the same source. A
    // write through it must be refused like the direct one: the denial
    // follows the tree, not the spelling of the path.
    let outside = fixture_root();
    let outside_link = outside.path().join("source-alias");
    std::os::unix::fs::symlink(&physical_source, &outside_link).unwrap();

    let confinement =
        fluent::os::writer_confinement(&source_link, &candidate, &artifact, &cache).unwrap();
    // The roots the executor grants the Writer: the candidate worktree, the
    // Git directory, the Task artifact, and the Attempt's writer cache.
    let writable = vec![
        candidate.clone(),
        physical_source.join(".git"),
        artifact.clone(),
        cache.clone(),
    ];
    let profile = fluent::os::render_writer_profile_for_landlock(
        &fluent::content::ContentResolver::new(None),
        &fluent::os::WriterProfileKind::AutonomousClaude,
        root.path().to_str().unwrap(),
        &writable,
        &[],
        &confinement,
    )
    .unwrap();
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(profile.path, policy_file.path()).unwrap();

    let probes: [(u8, &Path, bool); 10] = [
        (10, &physical_source.join("src/lib.rs"), false),
        (11, &candidate.join("change.txt"), true),
        (12, &physical_source.join(".git/test"), true),
        (13, &artifact.join("test"), true),
        (14, &work_model, false),
        (15, &sibling_artifact.join("test"), false),
        (16, &gate_artifact.join("test"), false),
        (17, &cache.join("test"), true),
        (18, &sibling_cache.join("test"), false),
        (19, &outside_link.join("src/lib.rs"), false),
    ];

    if kernel_enforces_landlock() {
        for (row, path, allowed) in probes {
            let script = format!("printf written > {}", path.display());
            let (ok, stderr) = run_confined(policy_file.path(), &script);
            if allowed {
                assert!(ok, "row {row} should be allowed: {stderr}");
                assert!(path.exists(), "row {row} did not create its file");
            } else {
                assert!(!ok, "row {row} should be denied");
                assert!(
                    !stderr.contains(NOT_ENFORCED),
                    "row {row} failed because the launcher never confined anything: {stderr}"
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(physical_source.join("src/lib.rs")).unwrap(),
            "source",
            "row 10 changed the source file"
        );
        assert_eq!(
            std::fs::read_to_string(&work_model).unwrap(),
            "host-owned",
            "row 14 changed the Work-model record"
        );
        let canonical = std::fs::canonicalize(&physical_source).unwrap();
        assert_denied(
            policy_file.path(),
            &format!("printf written > {}/src/lib.rs", canonical.display()),
            "the canonical source path was writable",
        );
    } else {
        let (ok, stderr) = run_confined(policy_file.path(), "true");
        assert!(!ok, "a kernel with no Landlock ran the Writer anyway");
        assert!(stderr.contains(NOT_ENFORCED), "{stderr}");
    }
}

#[test]
fn autonomous_claude_profile_denies_operator_project_state_on_linux() {
    let root = fixture_root();
    let home = root.path().join("home");
    let projects = home.join(".claude/projects");
    let worker_home = root.path().join("worker-home");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::create_dir_all(worker_home.join(".claude/projects")).unwrap();
    // A sibling inside the coder's own configuration tree stays writable, so
    // the refusal below names the project tree and not the whole grant.
    let settings = home.join(".claude/settings.json");
    std::fs::write(&settings, "{}").unwrap();
    std::fs::write(projects.join("session.jsonl"), "operator").unwrap();

    let profile = fluent::os::render_profile_for_access_for_autonomous_claude(
        &fluent::content::ContentResolver::new(None),
        &home.to_string_lossy(),
        std::slice::from_ref(&worker_home),
        &[],
    )
    .unwrap();
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(profile.path, policy_file.path()).unwrap();

    if kernel_enforces_landlock() {
        assert_denied(
            policy_file.path(),
            &format!("printf written > {}", projects.join("new.jsonl").display()),
            "a write landed in the operator's project tree",
        );
        assert_denied(
            policy_file.path(),
            &format!("cat {}", projects.join("session.jsonl").display()),
            "the operator's project tree was readable",
        );
        let (ok, stderr) = run_confined(
            policy_file.path(),
            &format!("printf written > {}", settings.display()),
        );
        assert!(
            ok,
            "a write to a sibling of the project tree was refused: {stderr}"
        );
    } else {
        let (ok, stderr) = run_confined(policy_file.path(), "true");
        assert!(!ok, "a kernel with no Landlock ran the coder anyway");
        assert!(stderr.contains(NOT_ENFORCED), "{stderr}");
    }
}

#[test]
fn common_only_with_denied_access_withholds_paths_on_linux() {
    let root = fixture_root();
    let scratch = root.path().join("scratch");
    let readable = root.path().join("readable");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&readable).unwrap();
    // A denied path inside the writable root, beside a path the root grants.
    // The sibling is what proves the carve-out is narrow.
    std::fs::create_dir_all(scratch.join("denied")).unwrap();
    let denied = scratch.join("denied");
    // A denied path inside the shared temp tree: the temporary directory grant
    // encloses it, so the carve-out has to reach that grant too.
    let temp = tempfile::TempDir::new().unwrap();
    let temp_denied = temp.path().join("denied");
    std::fs::create_dir_all(&temp_denied).unwrap();

    let profile = fluent::os::render_profile_common_only_with_denied_access(
        &fluent::content::ContentResolver::new(None),
        &root.path().to_string_lossy(),
        std::slice::from_ref(&scratch),
        std::slice::from_ref(&readable),
        std::slice::from_ref(&temp_denied),
        &[denied.clone(), temp_denied.clone()],
    )
    .unwrap();
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(profile.path, policy_file.path()).unwrap();

    if kernel_enforces_landlock() {
        assert_denied(
            policy_file.path(),
            &format!("printf written > {}", denied.join("new.txt").display()),
            "a write landed in a denied path",
        );
        assert_denied(
            policy_file.path(),
            &format!("printf written > {}", temp_denied.join("new.txt").display()),
            "a write landed in a denied path under the shared temp tree",
        );
        let (ok, stderr) = run_confined(
            policy_file.path(),
            &format!("printf written > {}", scratch.join("allowed.txt").display()),
        );
        assert!(ok, "a write to a granted sibling was refused: {stderr}");
    } else {
        let (ok, stderr) = run_confined(policy_file.path(), "true");
        assert!(!ok, "a kernel with no Landlock ran the command anyway");
        assert!(stderr.contains(NOT_ENFORCED), "{stderr}");
    }
}

#[test]
fn scoped_codex_provider_with_tools_withholds_installed_writes_on_linux() {
    let root = fixture_root();
    let private = root.path().join("private");
    let installed = root.path().join("installed");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();

    let profile = fluent::os::render_profile_for_scoped_codex_provider_with_tools(
        &fluent::content::ContentResolver::new(None),
        &private,
        std::slice::from_ref(&workspace),
        &[],
        &private,
        std::slice::from_ref(&installed),
    )
    .unwrap();
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(profile.path, policy_file.path()).unwrap();

    if kernel_enforces_landlock() {
        assert_denied(
            policy_file.path(),
            &format!("printf written > {}", installed.join("tool").display()),
            "the installed tool tree was writable",
        );
        let (ok, stderr) = run_confined(
            policy_file.path(),
            &format!(
                "printf written > {}",
                workspace.join("change.txt").display()
            ),
        );
        assert!(ok, "a write to a granted sibling was refused: {stderr}");
    } else {
        let (ok, stderr) = run_confined(policy_file.path(), "true");
        assert!(!ok, "a kernel with no Landlock ran the provider anyway");
        assert!(stderr.contains(NOT_ENFORCED), "{stderr}");
    }
}

#[test]
fn a_kernel_without_landlock_is_reported_rather_than_run_unconfined() {
    if kernel_enforces_landlock() {
        return;
    }
    let fixture = fixture(true);

    let (ok, stderr) = run_confined(fixture.policy.path(), "true");

    assert!(!ok);
    assert!(
        stderr.contains(NOT_ENFORCED) && stderr.contains("5.13"),
        "the refusal must name the requirement: {stderr}"
    );
}
