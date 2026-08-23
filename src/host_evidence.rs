//! Immutable, host-produced evidence attached to a reviewed Attempt.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::content::ContentResolver;
use crate::review::{self, Verdict};
use crate::work_model::{
    self, ArtifactRef, EvidenceAttachment, EvidenceRecovery, EvidenceRecoveryState,
    EvidenceReviewContext, EvidenceReviewTarget, Task, TaskKind, TaskStatus, WorkModelError,
    WorkModelStore,
};

const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;
const MAX_FOCUSED_COMMAND_BYTES: usize = 4096;
const MAX_FOCUSED_OUTPUT_BYTES: u64 = 384 * 1024;
const MAX_FOCUSED_TIMEOUT_SECONDS: u64 = 3600;

/// Version-one operator evidence document. Its serialized bytes, not a
/// re-rendering, are the audited artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDocument {
    pub schema_version: u32,
    pub producer: String,
    pub check: String,
    pub working_directory: String,
    pub result: EvidenceResult,
    pub run_at: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceResult {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundRecheckArtifact {
    path: String,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedCommandRecheckEvidenceBinding {
    schema_version: u32,
    candidate_commit: String,
    recheck_task_id: String,
    source_results: BoundRecheckArtifact,
    recheck_artifacts: Vec<BoundRecheckArtifact>,
    command_index: usize,
    command: String,
    working_directory: String,
}

#[derive(Debug, Clone)]
struct ValidatedRecheckFrontier {
    prior_tests_review: Task,
    binding: FailedCommandRecheckEvidenceBinding,
    binding_path: String,
    binding_bytes: Vec<u8>,
    correction_artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedEvidenceRun {
    pub recovery: EvidenceRecovery,
    pub result: EvidenceResult,
    pub command: String,
    pub working_directory: String,
}

#[allow(clippy::too_many_arguments)]
pub fn run_focused(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    review_artifact: &str,
    command_override: Option<&str>,
    working_directory_override: Option<&str>,
    timeout_seconds: u64,
    resolver: &ContentResolver,
) -> Result<FocusedEvidenceRun> {
    anyhow::ensure!(
        (1..=MAX_FOCUSED_TIMEOUT_SECONDS).contains(&timeout_seconds),
        "focused evidence timeout must be between 1 and {MAX_FOCUSED_TIMEOUT_SECONDS} seconds"
    );
    let item = store
        .read_work_item(work_item_id)
        .with_context(|| format!("read Work Item {work_item_id}"))?;
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .with_context(|| format!("Attempt {attempt_id} not found"))?;
    let candidate_commit = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.clone())
        .context("focused evidence requires a completed Writer candidate")?;
    let selected = validate_evidence_frontier(
        project_root,
        &item,
        attempt_id,
        &candidate_commit,
        &[review_artifact.to_string()],
    )?;
    let review_task = selected
        .first()
        .context("focused evidence requires one failed review")?;
    anyhow::ensure!(
        review_task.role == "tests",
        "focused evidence can target only a failed Tests Review"
    );
    let review_path = project_root.join(review_artifact);
    let review_content = fs::read_to_string(&review_path)
        .with_context(|| format!("read failed review {}", review_path.display()))?;
    let declared = review::extract_focused_evidence_request(&review_content);
    let command = resolve_requested_value(
        "command",
        declared.as_ref().map(|request| request.command.as_str()),
        command_override,
    )?;
    validate_focused_command(&command)?;
    let working_directory = resolve_requested_value(
        "working directory",
        declared
            .as_ref()
            .map(|request| request.working_directory.as_str()),
        working_directory_override,
    )?;

    let candidate_workspace = review_task
        .review_context
        .as_ref()
        .map(|context| PathBuf::from(&context.candidate_workspace_path))
        .context("failed Tests Review has no candidate workspace")?;
    let candidate_workspace = if candidate_workspace.is_absolute() {
        candidate_workspace
    } else {
        project_root.join(candidate_workspace)
    };
    let candidate_workspace = canonical_candidate_workspace(&candidate_workspace)?;
    let working_path = resolve_working_directory(&candidate_workspace, &working_directory)?;

    // Prevent Fluent landing from changing the candidate while this command
    // reads it. The later attachment reacquires the same boundary and validates
    // the complete frontier again before publishing Work state.
    let land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire land lock before focused evidence run")?;
    anyhow::ensure!(
        candidate_workspace_is_clean_at(
            project_root,
            attempt
                .tasks
                .iter()
                .rev()
                .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
                .and_then(|task| task.output.as_ref())
                .context("focused evidence lost its Writer output")?,
            &candidate_commit,
        ),
        "focused evidence candidate changed before execution"
    );
    let run_dir = allocate_focused_run_dir(project_root, work_item_id, attempt_id)?;
    let execution = execute_focused_command(
        &candidate_workspace,
        &working_path,
        &run_dir,
        &command,
        Duration::from_secs(timeout_seconds),
        resolver,
    )?;
    anyhow::ensure!(
        candidate_workspace_is_clean_at(
            project_root,
            attempt
                .tasks
                .iter()
                .rev()
                .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
                .and_then(|task| task.output.as_ref())
                .context("focused evidence lost its Writer output")?,
            &candidate_commit,
        ),
        "focused evidence candidate changed during execution"
    );
    let result = if execution.exit_code == 0 {
        EvidenceResult::Pass
    } else {
        EvidenceResult::Fail
    };
    let document = EvidenceDocument {
        schema_version: 1,
        producer: "fluent focused evidence runner".to_string(),
        check: command.clone(),
        working_directory: working_path.display().to_string(),
        result: result.clone(),
        run_at: chrono::Utc::now().to_rfc3339(),
        output: execution.output,
    };
    let document_bytes = serde_json::to_vec_pretty(&document)?;
    anyhow::ensure!(
        document_bytes.len() <= MAX_EVIDENCE_BYTES,
        "focused evidence document exceeds {MAX_EVIDENCE_BYTES} bytes"
    );
    let document_path = run_dir.join("evidence.json");
    write_exclusive(&document_path, &document_bytes)?;
    drop(land_lock);

    let recovery = attach(
        project_root,
        store,
        work_item_id,
        attempt_id,
        &candidate_commit,
        &document_path,
        &[review_artifact.to_string()],
        None,
    )?;
    Ok(FocusedEvidenceRun {
        recovery,
        result,
        command,
        working_directory,
    })
}

fn resolve_requested_value(
    name: &str,
    declared: Option<&str>,
    supplied: Option<&str>,
) -> Result<String> {
    match (declared, supplied) {
        (Some(declared), Some(supplied)) if declared != supplied => {
            bail!("supplied focused evidence {name} does not match the failed review")
        }
        (Some(declared), _) => Ok(declared.to_string()),
        (None, Some(supplied)) if !supplied.trim().is_empty() => Ok(supplied.to_string()),
        _ => bail!(
            "failed review has no valid Evidence-Command and Evidence-Working-Directory; supply the missing focused evidence {name} explicitly"
        ),
    }
}

fn validate_focused_command(command: &str) -> Result<()> {
    anyhow::ensure!(
        !command.trim().is_empty() && command.len() <= MAX_FOCUSED_COMMAND_BYTES,
        "focused evidence command must be between 1 and {MAX_FOCUSED_COMMAND_BYTES} bytes"
    );
    anyhow::ensure!(
        !command.contains(['\n', '\r', '\0']),
        "focused evidence command must occupy one line"
    );
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Unquoted,
        Single,
        Double,
    }
    let mut quote = Quote::Unquoted;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((_, character)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Quote::Single {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (Quote::Unquoted, '\'') => quote = Quote::Single,
            (Quote::Unquoted, '"') => quote = Quote::Double,
            (Quote::Single, '\'') => quote = Quote::Unquoted,
            (Quote::Double, '"') => quote = Quote::Unquoted,
            (Quote::Unquoted, ';' | '&' | '|' | '(' | ')' | '>' | '<') => {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token {character:?} is not allowed"
                )
            }
            (Quote::Unquoted | Quote::Double, '`') => {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token \"`\" is not allowed"
                )
            }
            (Quote::Unquoted | Quote::Double, '$')
                if chars.peek().is_some_and(|(_, next)| *next == '(') =>
            {
                bail!(
                    "focused evidence command must name one direct harness invocation; shell control token \"$(\" is not allowed"
                )
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        quote == Quote::Unquoted && !escaped,
        "focused evidence command contains an unterminated quote or escape"
    );
    Ok(())
}

fn canonical_candidate_workspace(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect candidate workspace {}", path.display()))?;
    anyhow::ensure!(metadata.is_dir(), "candidate workspace must be a directory");
    path.canonicalize()
        .with_context(|| format!("canonicalize candidate workspace {}", path.display()))
}

fn resolve_working_directory(candidate: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    let canonical_spelling = relative == Path::new(".")
        || relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    anyhow::ensure!(
        !relative.is_absolute() && canonical_spelling,
        "focused evidence working directory must be a canonical relative candidate path"
    );
    let joined = candidate.join(relative);
    let resolved = joined.canonicalize().with_context(|| {
        format!(
            "resolve focused evidence working directory {}",
            joined.display()
        )
    })?;
    anyhow::ensure!(
        resolved.starts_with(candidate) && resolved.is_dir(),
        "focused evidence working directory must stay inside the candidate"
    );
    Ok(resolved)
}

fn allocate_focused_run_dir(project_root: &Path, work: &str, attempt: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        [work, attempt].iter().all(|identity| {
            let path = Path::new(identity);
            !identity.is_empty()
                && !path.is_absolute()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                && path.components().count() == 1
        }),
        "focused evidence identities must be single managed path components"
    );
    let relative = PathBuf::from(".fluent/work/artifacts")
        .join(work)
        .join(attempt)
        .join("focused-evidence-runs");
    let parent = create_managed_directory_chain(project_root, &relative)?;
    for index in 1..=10_000_u32 {
        let path = parent.join(format!("run-{index}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create focused evidence run {}", path.display()));
            }
        }
    }
    bail!("focused evidence run index exhausted")
}

fn create_managed_directory_chain(project_root: &Path, relative: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "managed evidence directory must use normal relative components"
    );
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            unreachable!("validated normal component");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("managed evidence directory must not traverse a symlink")
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("managed evidence path component must be a directory")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)
                .with_context(|| {
                    format!("create managed evidence directory {}", current.display())
                })?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

struct FocusedExecution {
    exit_code: i32,
    output: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RustupEnvironment {
    home: Option<PathBuf>,
    toolchain: Option<std::ffi::OsString>,
}

fn host_rustup_environment(source_home: &Path) -> RustupEnvironment {
    let configured_home = std::env::var_os("RUSTUP_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = configured_home
        .unwrap_or_else(|| source_home.join(".rustup"))
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir());
    let toolchain = std::env::var_os("RUSTUP_TOOLCHAIN").filter(|value| !value.is_empty());
    RustupEnvironment { home, toolchain }
}

fn execute_focused_command(
    candidate_workspace: &Path,
    working_directory: &Path,
    run_dir: &Path,
    command: &str,
    timeout: Duration,
    resolver: &ContentResolver,
) -> Result<FocusedExecution> {
    let scratch = run_dir.join("scratch");
    let home = scratch.join("home");
    let temporary = scratch.join("tmp");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&temporary)?;
    let cargo_home = scratch.join("cargo-home");
    crate::writer_cache::prepare_private_cargo_dependency_cache(
        &cargo_home,
        crate::writer_cache::canonical_cargo_home_from_env().as_deref(),
    )?;
    let source_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("focused evidence requires the host HOME path for toolchain confinement")?;
    let rustup = host_rustup_environment(&source_home);
    let denied_agent_state = [".claude", ".codex", ".agents"]
        .into_iter()
        .map(|path| source_home.join(path))
        .collect::<Vec<_>>();
    let mut denied_writes = denied_agent_state.clone();
    denied_writes.extend([
        candidate_workspace.to_path_buf(),
        source_home.join(".local/state"),
        source_home.join(".claude.json"),
        source_home.join(".claude.json.lock"),
    ]);
    let mut readable_roots = vec![candidate_workspace.to_path_buf()];
    if let Some(rustup_home) = &rustup.home {
        denied_writes.push(rustup_home.clone());
        readable_roots.push(rustup_home.clone());
    }
    let profile = crate::os::render_profile_common_only_with_denied_access(
        resolver,
        &source_home.to_string_lossy(),
        &[run_dir.to_path_buf()],
        &readable_roots,
        &denied_agent_state,
        &denied_writes,
    )?;
    crate::os::preflight_profile(&profile)?;

    let stdout_path = run_dir.join("stdout.log");
    let stderr_path = run_dir.join("stderr.log");
    let stdout = create_exclusive_file(&stdout_path)?;
    let stderr = create_exclusive_file(&stderr_path)?;
    let mut process = crate::os::command_under_profile(&profile, Path::new("/bin/sh"));
    process
        .arg("-c")
        .arg(command)
        .current_dir(working_directory)
        .env_clear()
        .env("HOME", &home)
        // Foundation-backed tools can resolve the account home without HOME.
        // Keep their user caches in the same private tree as shell-native tools.
        .env("CFFIXED_USER_HOME", &home)
        .env("TMPDIR", &temporary)
        .env("XDG_CACHE_HOME", scratch.join("xdg-cache"))
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TARGET_DIR", scratch.join("cargo-target"))
        .env("CLANG_MODULE_CACHE_PATH", scratch.join("clang-modules"))
        .env("SWIFT_MODULE_CACHE_PATH", scratch.join("swift-modules"))
        .env(
            "SWIFTPM_MODULECACHE_OVERRIDE",
            scratch.join("swiftpm-modules"),
        )
        .env("FLUENT_EVIDENCE_SCRATCH", &scratch)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(rustup_home) = &rustup.home {
        process.env("RUSTUP_HOME", rustup_home);
    }
    if let Some(toolchain) = &rustup.toolchain {
        process.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    for name in ["PATH", "DEVELOPER_DIR", "SDKROOT", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let child = process
        .spawn()
        .with_context(|| format!("spawn focused evidence command: {command}"))?;
    let (natural_exit_code, timed_out, disposition) =
        crate::coder::wait_owned_process_tree(child, timeout)
            .map_err(|error| anyhow::anyhow!(error))?;
    anyhow::ensure!(
        disposition.is_verified_swept(),
        "focused evidence process cleanup was not confirmed: {disposition:?}"
    );
    let exit_code = if timed_out { 124 } else { natural_exit_code };
    let stdout = read_bounded_file(&stdout_path)?;
    let stderr = read_bounded_file(&stderr_path)?;
    let output = format!(
        "exit_code: {exit_code}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    Ok(FocusedExecution { exit_code, output })
}

fn create_exclusive_file(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create host evidence file {}", path.display()))
}

fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_exclusive_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_FOCUSED_OUTPUT_BYTES {
        file.seek(SeekFrom::Start(length - MAX_FOCUSED_OUTPUT_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FOCUSED_OUTPUT_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn attach(
    project_root: &Path,
    store: &WorkModelStore,
    work_item_id: &str,
    attempt_id: &str,
    candidate_commit: &str,
    evidence_file: &Path,
    review_artifacts: &[String],
    recheck_artifact: Option<&str>,
) -> Result<EvidenceRecovery> {
    anyhow::ensure!(
        (recheck_artifact.is_some() && review_artifacts.is_empty())
            || (recheck_artifact.is_none() && !review_artifacts.is_empty()),
        "name either failed review artifacts or one failed-command recheck artifact"
    );
    if fs::symlink_metadata(evidence_file)
        .with_context(|| format!("inspect evidence file {}", evidence_file.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("evidence file must not be a symlink");
    }
    let bytes = fs::read(evidence_file)
        .with_context(|| format!("read evidence file {}", evidence_file.display()))?;
    let document = parse_document(&bytes)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let snapshot_path = snapshot_path(work_item_id, attempt_id, &digest)?;

    // Share the land boundary with Attempt execution and landing.  Acquire it
    // before the model lock so the frontier is revalidated only after no
    // concurrent operation can publish a candidate transition.
    let _land_lock = crate::land_lock::acquire(&crate::land_lock::lock_path(project_root))
        .context("acquire land lock before attaching host evidence")?;

    let recovery = store.mutate_work_item(work_item_id, |item| {
        let existing = item
            .attempts
            .iter()
            .find(|a| a.id == attempt_id)
            .ok_or_else(|| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        if let Some(recheck_artifact) = recheck_artifact
            && let Some(prior) = existing_recheck_recovery(
                project_root,
                existing,
                candidate_commit,
                &digest,
                &snapshot_path,
                recheck_artifact,
            )
        {
            return Ok(prior.clone());
        }
        let recheck_frontier = recheck_artifact
            .map(|artifact| {
                validate_recheck_evidence_frontier(
                    project_root,
                    item,
                    attempt_id,
                    candidate_commit,
                    artifact,
                    &document,
                )
            })
            .transpose()?;
        let prior_reviews = match &recheck_frontier {
            Some(frontier) => vec![frontier.prior_tests_review.clone()],
            None => validate_evidence_frontier(
                project_root,
                item,
                attempt_id,
                candidate_commit,
                review_artifacts,
            )?,
        };
        let target_artifacts = match &recheck_frontier {
            Some(frontier) => vec![frontier.binding_path.clone()],
            None => review_artifacts.to_vec(),
        };
        if let Some(prior) = existing.evidence_recoveries.iter().find(|prior| {
            prior.candidate_commit == candidate_commit
                && prior.attachment.digest == digest
                && prior.attachment.snapshot_path == snapshot_path
                && prior.targets.len() == prior_reviews.len()
                && prior.targets.iter().all(|target| {
                    prior_reviews
                        .iter()
                        .any(|review| review.role == target.role)
                        && target_artifacts
                            .iter()
                            .any(|path| path == &target.prior_review_artifact)
                })
        }) {
            return Ok(prior.clone());
        }
        let attachment_was_used = existing.evidence_recoveries.iter().any(|prior| {
            prior.candidate_commit == candidate_commit
                && prior.attachment.digest == digest
                && prior.attachment.snapshot_path == snapshot_path
        });
        let recovery_token = if attachment_was_used {
            let frontier = target_artifacts.join("\0");
            let frontier_digest = format!("{:x}", Sha256::digest(frontier.as_bytes()));
            format!("{}-{}", &digest[7..19], &frontier_digest[..12])
        } else {
            digest[7..19].to_string()
        };
        // Publish only after the lock-held frontier check. A rejected request
        // therefore cannot leave a host-owned artifact behind.
        publish_snapshot(project_root, &snapshot_path, &bytes).map_err(|_| {
            WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            }
        })?;
        if let Some(frontier) = &recheck_frontier {
            publish_snapshot(
                project_root,
                &frontier.binding_path,
                &frontier.binding_bytes,
            )
            .map_err(|_| WorkModelError::AttemptNotFound {
                id: attempt_id.to_string(),
            })?;
        }
        let attempt = item
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt_id)
            .expect("validated Attempt must remain present");
        let roles = prior_reviews
            .iter()
            .map(|task| task.role.clone())
            .collect::<Vec<_>>();
        let recovery = attempt.attach_evidence_recovery(EvidenceRecovery {
            id: format!("host-evidence-{recovery_token}"),
            candidate_commit: candidate_commit.to_string(),
            attachment: EvidenceAttachment {
                snapshot_path: snapshot_path.clone(),
                digest: digest.clone(),
            },
            targets: roles
                .iter()
                .map(|role| EvidenceReviewTarget {
                    role: role.clone(),
                    prior_review_artifact: if recheck_frontier.is_some() {
                        target_artifacts.first().cloned().unwrap_or_default()
                    } else {
                        review_artifacts
                            .iter()
                            .find(|path| {
                                prior_reviews.iter().any(|task| {
                                    task.role == *role
                                        && review_artifact_path(task).as_deref()
                                            == Some(path.as_str())
                                })
                            })
                            .cloned()
                            .unwrap_or_default()
                    },
                    review_task_id: None,
                })
                .collect(),
            state: EvidenceRecoveryState::Reviewing,
            created_at: work_model::now_iso8601(),
        })?;
        for role in roles {
            let prior = prior_reviews
                .iter()
                .find(|task| task.role == role)
                .cloned()
                .unwrap();
            let task_id = format!("{attempt_id}-evidence-{recovery_token}-{role}");
            if !attempt.tasks.iter().any(|task| task.id == task_id) {
                let mut inputs = prior.input_artifacts.clone();
                if let Some(frontier) = &recheck_frontier {
                    inputs.push(ArtifactRef {
                        producer_id: frontier.binding.recheck_task_id.clone(),
                        path: frontier.binding_path.clone(),
                    });
                    inputs.extend(frontier.correction_artifacts.clone());
                }
                inputs.push(ArtifactRef {
                    producer_id: "host-evidence".to_string(),
                    path: snapshot_path.clone(),
                });
                inputs.push(ArtifactRef {
                    producer_id: prior.id.clone(),
                    path: review_artifact_path(&prior).unwrap(),
                });
                let prior_review_artifact = recheck_frontier
                    .as_ref()
                    .map(|frontier| frontier.binding_path.clone())
                    .unwrap_or_else(|| review_artifact_path(&prior).unwrap());
                attempt.tasks.push(Task {
                    id: task_id.clone(),
                    kind: TaskKind::Review,
                    status: TaskStatus::Planned,
                    role: role.clone(),
                    instructions: None,
                    work_item_id: work_item_id.to_string(),
                    attempt_id: Some(attempt_id.to_string()),
                    workspace_access: prior.workspace_access,
                    artifact_area: Some(work_model::TaskArtifactArea {
                        path: work_model::work_artifact_path(work_item_id, attempt_id, &task_id),
                    }),
                    review_context: prior.review_context.clone(),
                    evidence_review_context: Some(EvidenceReviewContext {
                        recovery_id: recovery.id.clone(),
                        candidate_commit: candidate_commit.to_string(),
                        attachment: recovery.attachment.clone(),
                        prior_review_artifact,
                    }),
                    input_artifacts: inputs,
                    depends_on: None,
                    output: None,
                    created_at: Some(work_model::now_iso8601()),
                    started_at: None,
                    completed_at: None,
                });
            }
            if let Some(target) = attempt
                .evidence_recoveries
                .iter_mut()
                .find(|entry| entry.id == recovery.id)
                .and_then(|entry| entry.targets.iter_mut().find(|target| target.role == role))
            {
                target.review_task_id = Some(task_id);
            }
        }
        attempt.status = work_model::AttemptStatus::Reviewing;
        attempt.review_state = Some(work_model::AttemptReviewState::NotReviewed);
        Ok(recovery)
    })?;
    let _ = document; // validation intentionally happens before publication.
    Ok(recovery)
}

fn existing_recheck_recovery<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
    evidence_digest: &str,
    evidence_snapshot_path: &str,
    recheck_artifact: &str,
) -> Option<&'a EvidenceRecovery> {
    attempt.evidence_recoveries.iter().find_map(|recovery| {
        if recovery.candidate_commit != candidate_commit
            || recovery.attachment.digest != evidence_digest
            || recovery.attachment.snapshot_path != evidence_snapshot_path
            || recovery.targets.len() != 1
        {
            return None;
        }
        let binding_path = &recovery.targets[0].prior_review_artifact;
        let binding_bytes = fs::read(project_root.join(binding_path)).ok()?;
        let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
        if snapshot_path(&attempt.work_item_id, &attempt.id, &binding_digest)
            .ok()?
            .as_str()
            != binding_path
            || format!(
                "sha256:{:x}",
                Sha256::digest(fs::read(project_root.join(evidence_snapshot_path)).ok()?)
            ) != evidence_digest
        {
            return None;
        }
        let binding: FailedCommandRecheckEvidenceBinding =
            serde_json::from_slice(&binding_bytes).ok()?;
        let recheck = attempt
            .tasks
            .iter()
            .find(|task| task.id == binding.recheck_task_id)?;
        let expected = recheck
            .artifact_area
            .as_ref()
            .map(|area| format!("{}/failed-command-recheck.json", area.path));
        (binding.candidate_commit == candidate_commit
            && expected.as_deref() == Some(recheck_artifact))
        .then_some(recovery)
    })
}

fn validate_evidence_frontier(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate_commit: &str,
    review_artifacts: &[String],
) -> Result<Vec<Task>, WorkModelError> {
    let rejected = || WorkModelError::AttemptNotFound {
        id: attempt_id.to_string(),
    };
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(rejected)?;
    if review_artifacts.is_empty()
        || review_artifacts.len()
            != review_artifacts
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        || attempt
            .tasks
            .iter()
            .any(|task| task.status == TaskStatus::Executing)
        || item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id && candidate.candidate_commit == candidate_commit
        })
    {
        return Err(rejected());
    }
    let completed_writer = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|(index, task)| task.output.as_ref().map(|output| (index, output)))
        .ok_or_else(rejected)?;
    if completed_writer.1.commit != candidate_commit
        || !candidate_workspace_is_clean_at(project_root, completed_writer.1, candidate_commit)
    {
        return Err(rejected());
    }
    let is_legacy = attempt.is_evidence_only_recoverable(candidate_commit, review_artifacts);
    let is_failed_review_pause = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::RoundCap | work_model::PauseKind::Uncertain)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let is_paused_evidence_recovery = attempt.status == work_model::AttemptStatus::NeedsUser
        && attempt.pause_kind == Some(work_model::PauseKind::Uncertain)
        && attempt.evidence_recoveries.last().is_some_and(|recovery| {
            recovery.state == EvidenceRecoveryState::NeedsEvidence
                && recovery.candidate_commit == candidate_commit
                && attempt.tasks.iter().any(|task| {
                    task.evidence_review_context
                        .as_ref()
                        .is_some_and(|context| {
                            context.recovery_id == recovery.id
                                && recovery
                                    .targets
                                    .iter()
                                    .any(|target| target.role == task.role)
                        })
                })
        });
    if !(attempt.status == work_model::AttemptStatus::Reviewing
        || is_failed_review_pause
        || is_legacy
        || is_paused_evidence_recovery)
    {
        return Err(rejected());
    }
    let mut selected = Vec::new();
    for path in review_artifacts {
        let (index, task) = attempt
            .tasks
            .iter()
            .enumerate()
            .find(|(_, task)| review_artifact_path(task).as_deref() == Some(path.as_str()))
            .ok_or_else(rejected)?;
        if task.kind != TaskKind::Review
            || task.status != TaskStatus::Complete
            || task
                .review_context
                .as_ref()
                .map(|context| context.candidate_commit.as_str())
                != Some(candidate_commit)
            || !matches!(read_verdict(project_root, task), Some(Verdict::Fail))
            || index <= completed_writer.0
            || attempt.tasks.iter().skip(index + 1).any(|later| {
                later.kind == TaskKind::Review
                    && later.role == task.role
                    && later.status == TaskStatus::Complete
            })
            || selected.iter().any(|prior: &Task| prior.role == task.role)
            || (is_paused_evidence_recovery
                && !task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        attempt.evidence_recoveries.last().is_some_and(|recovery| {
                            context.recovery_id == recovery.id
                                && recovery.state == EvidenceRecoveryState::NeedsEvidence
                        })
                    }))
        {
            return Err(rejected());
        }
        selected.push(task.clone());
    }
    Ok(selected)
}

/// Resolve the failed-command recheck that currently needs host evidence.
///
/// The canonical shape ends with the completed recheck. Fluent 0.2.0 could also
/// schedule one corrective Writer while capacity remained, give it the recheck
/// artifacts, accept a no-change declaration, and then terminalize the Attempt
/// after rejecting that same recheck as a source. Admit only that exact historical
/// shape so the public evidence command can repair it without rewriting history.
fn recoverable_failed_recheck<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate_commit: &str,
) -> Option<&'a Task> {
    if let Some(recheck) = attempt.tasks.last().filter(|task| {
        task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && task.is_failed_command_recheck()
            && task
                .review_context
                .as_ref()
                .is_some_and(|context| context.candidate_commit == candidate_commit)
    }) {
        return Some(recheck);
    }

    let writer = attempt.tasks.last()?;
    let output = writer.output.as_ref()?;
    if writer.kind != TaskKind::Write
        || writer.status != TaskStatus::Complete
        || output.no_change.is_none()
        || output.commit != candidate_commit
    {
        return None;
    }
    let result_sources = writer
        .input_artifacts
        .iter()
        .filter(|artifact| artifact.path.ends_with("/tester-results.json"))
        .collect::<Vec<_>>();
    let [result_source] = result_sources.as_slice() else {
        return None;
    };
    let recheck = attempt
        .tasks
        .iter()
        .rev()
        .skip(1)
        .find(|task| task.id == result_source.producer_id)?;
    if recheck.kind != TaskKind::Tester
        || recheck.status != TaskStatus::Complete
        || !recheck.is_failed_command_recheck()
        || recheck
            .review_context
            .as_ref()
            .is_none_or(|context| context.candidate_commit != candidate_commit)
    {
        return None;
    }
    let expected_results = recheck
        .artifact_area
        .as_ref()
        .map(|area| format!("{}/tester-results.json", area.path));
    if expected_results.as_deref() != Some(result_source.path.as_str()) {
        return None;
    }
    let correction =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .ok()?;
    let expected = correction
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    let actual = writer
        .input_artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    (expected.len() == correction.len()
        && actual.len() == writer.input_artifacts.len()
        && expected == actual)
        .then_some(recheck)
}

fn validate_recheck_evidence_frontier(
    project_root: &Path,
    item: &work_model::WorkItem,
    attempt_id: &str,
    candidate_commit: &str,
    recheck_artifact: &str,
    document: &EvidenceDocument,
) -> Result<ValidatedRecheckFrontier, WorkModelError> {
    let rejected = || WorkModelError::AttemptNotFound {
        id: attempt_id.to_string(),
    };
    let attempt = item
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(rejected)?;
    let paused_for_evidence = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::HostEvidence | work_model::PauseKind::RoundCap)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let legacy_terminal = attempt.status == work_model::AttemptStatus::Complete
        && attempt.pause_kind.is_none()
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    if !(paused_for_evidence || legacy_terminal)
        || attempt.has_round_cap_task_blocker()
        || item.merge_candidates.iter().any(|candidate| {
            candidate.attempt_id == attempt_id && candidate.candidate_commit == candidate_commit
        })
        || document.result != EvidenceResult::Pass
        || document.working_directory != "."
    {
        return Err(rejected());
    }
    let (_writer_index, writer_output) = attempt
        .tasks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, task)| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|(index, task)| task.output.as_ref().map(|output| (index, output)))
        .ok_or_else(rejected)?;
    if writer_output.commit != candidate_commit
        || !candidate_workspace_is_clean_at(project_root, writer_output, candidate_commit)
    {
        return Err(rejected());
    }
    let recheck =
        recoverable_failed_recheck(project_root, attempt, candidate_commit).ok_or_else(rejected)?;
    let area = recheck.artifact_area.as_ref().ok_or_else(rejected)?;
    let expected_manifest = format!("{}/failed-command-recheck.json", area.path);
    if recheck_artifact != expected_manifest {
        return Err(rejected());
    }
    let correction_artifacts =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .map_err(|_| rejected())?;
    let manifest_path = project_root.join(&expected_manifest);
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
    let results_artifact = correction_artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("/tester-results.json"))
        .ok_or_else(rejected)?;
    let results_path = project_root.join(&results_artifact.path);
    let results: crate::tester::TesterResults =
        serde_json::from_slice(&fs::read(&results_path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
    let [command_index] = manifest.failed_command_indexes.as_slice() else {
        return Err(rejected());
    };
    let [recheck_command] = results.commands.as_slice() else {
        return Err(rejected());
    };
    if results.error.is_some()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &results_path,
            None,
            0,
            &recheck_command.command,
        ) != 0
        || recheck_command.exit_code == 0
        || recheck_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || document.check != recheck_command.command
    {
        return Err(rejected());
    }
    let source = recheck.input_artifacts.first().ok_or_else(rejected)?;
    if source.path != manifest.source_results {
        return Err(rejected());
    }
    let source_path = project_root.join(&source.path);
    let source_bytes = fs::read(&source_path).map_err(|_| rejected())?;
    let source_results: crate::tester::TesterResults =
        serde_json::from_slice(&source_bytes).map_err(|_| rejected())?;
    let source_command = source_results
        .commands
        .get(*command_index)
        .ok_or_else(rejected)?;
    if source_results.candidate_commit.as_deref() != Some(candidate_commit)
        || source_results.error.is_some()
        || source_command.exit_code == 0
        || source_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_command.command != recheck_command.command
        || source_command.test_harness != recheck_command.test_harness
        || source_results.summary.fail
            != source_results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            *command_index,
            &source_command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
    {
        return Err(rejected());
    }
    let prior_tests_review = latest_passing_tests_review(project_root, attempt, candidate_commit)
        .cloned()
        .ok_or_else(rejected)?;
    let bound_artifact = |artifact: &ArtifactRef| -> Result<BoundRecheckArtifact, WorkModelError> {
        let bytes = fs::read(project_root.join(&artifact.path)).map_err(|_| rejected())?;
        Ok(BoundRecheckArtifact {
            path: artifact.path.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
        })
    };
    let binding = FailedCommandRecheckEvidenceBinding {
        schema_version: 1,
        candidate_commit: candidate_commit.to_string(),
        recheck_task_id: recheck.id.clone(),
        source_results: BoundRecheckArtifact {
            path: source.path.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(&source_bytes)),
        },
        recheck_artifacts: correction_artifacts
            .iter()
            .map(bound_artifact)
            .collect::<Result<Vec<_>, _>>()?,
        command_index: *command_index,
        command: recheck_command.command.clone(),
        working_directory: ".".to_string(),
    };
    let binding_bytes = serde_json::to_vec_pretty(&binding).map_err(|_| rejected())?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    let binding_path =
        snapshot_path(item.id.as_str(), attempt_id, &binding_digest).map_err(|_| rejected())?;
    Ok(ValidatedRecheckFrontier {
        prior_tests_review,
        binding,
        binding_path,
        binding_bytes,
        correction_artifacts,
    })
}

pub(crate) fn failed_recheck_evidence_recovery_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<String> {
    let paused_for_evidence = attempt.status == work_model::AttemptStatus::NeedsUser
        && matches!(
            attempt.pause_kind,
            Some(work_model::PauseKind::HostEvidence | work_model::PauseKind::RoundCap)
        )
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    let legacy_terminal = attempt.status == work_model::AttemptStatus::Complete
        && attempt.pause_kind.is_none()
        && attempt.review_state == Some(work_model::AttemptReviewState::Failed);
    if !(paused_for_evidence || legacy_terminal) {
        return None;
    }
    pending_failed_recheck_evidence_recovery_action(project_root, attempt)
}

pub(crate) fn pending_failed_recheck_evidence_recovery_action(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<String> {
    let writer_output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)?
        .output
        .as_ref()?;
    let candidate = writer_output.commit.as_str();
    if !candidate_workspace_is_clean_at(project_root, writer_output, candidate)
        || latest_passing_tests_review(project_root, attempt, candidate).is_none()
    {
        return None;
    }
    let recheck = recoverable_failed_recheck(project_root, attempt, candidate)?;
    let area = recheck.artifact_area.as_ref()?;
    let manifest_path = format!("{}/failed-command-recheck.json", area.path);
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(project_root.join(&manifest_path)).ok()?).ok()?;
    let results: crate::tester::TesterResults = serde_json::from_slice(
        &fs::read(project_root.join(&area.path).join("tester-results.json")).ok()?,
    )
    .ok()?;
    let [command_index] = manifest.failed_command_indexes.as_slice() else {
        return None;
    };
    let [recheck_command] = results.commands.as_slice() else {
        return None;
    };
    let source = recheck.input_artifacts.first()?;
    let source_path = project_root.join(&source.path);
    let source_results: crate::tester::TesterResults =
        serde_json::from_slice(&fs::read(&source_path).ok()?).ok()?;
    let source_command = source_results.commands.get(*command_index)?;
    if manifest.candidate_commit != candidate
        || manifest.source_results != source.path
        || results.candidate_commit.as_deref() != Some(candidate)
        || results.error.is_some()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &project_root.join(&area.path).join("tester-results.json"),
            None,
            0,
            &recheck_command.command,
        ) != 0
        || recheck_command.exit_code == 0
        || recheck_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_results.candidate_commit.as_deref() != Some(candidate)
        || source_results.error.is_some()
        || source_command.exit_code == 0
        || source_command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || source_command.command != recheck_command.command
        || source_command.test_harness != recheck_command.test_harness
        || source_results.summary.fail
            != source_results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            *command_index,
            &source_command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
        || crate::tester_evidence::failed_command_recheck_correction_artifacts(
            project_root,
            recheck,
        )
        .is_err()
    {
        return None;
    }
    Some(format!(
        "fluent attempt evidence attach {} {} --candidate {} --evidence-file <path> --recheck-artifact {}",
        attempt.work_item_id, attempt.id, candidate, manifest_path
    ))
}

pub(crate) fn completed_recheck_evidence_source(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Option<PathBuf> {
    let recovery = attempt.evidence_recoveries.iter().rev().find(|recovery| {
        recovery.state == EvidenceRecoveryState::Complete
            && recovery.targets.len() == 1
            && recovery.targets[0].role == "tests"
    })?;
    let target = &recovery.targets[0];
    let binding_bytes = fs::read(project_root.join(&target.prior_review_artifact)).ok()?;
    let binding: FailedCommandRecheckEvidenceBinding =
        serde_json::from_slice(&binding_bytes).ok()?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    if binding.schema_version != 1
        || binding.candidate_commit != recovery.candidate_commit
        || snapshot_path(
            attempt.work_item_id.as_str(),
            attempt.id.as_str(),
            &binding_digest,
        )
        .ok()
        .as_deref()
            != Some(target.prior_review_artifact.as_str())
        || attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref())
            .map(|output| output.commit.as_str())
            != Some(recovery.candidate_commit.as_str())
    {
        return None;
    }
    let evidence_bytes = fs::read(project_root.join(&recovery.attachment.snapshot_path)).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest {
        return None;
    }
    let evidence = parse_document(&evidence_bytes).ok()?;
    if evidence.result != EvidenceResult::Pass
        || evidence.check != binding.command
        || evidence.working_directory != binding.working_directory
    {
        return None;
    }
    let review_task = target.review_task_id.as_ref().and_then(|id| {
        attempt.tasks.iter().find(|task| {
            task.id == *id
                && task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task
                    .evidence_review_context
                    .as_ref()
                    .is_some_and(|context| {
                        context.recovery_id == recovery.id
                            && context.candidate_commit == recovery.candidate_commit
                            && context.prior_review_artifact == target.prior_review_artifact
                    })
        })
    })?;
    if read_verdict(project_root, review_task) != Some(Verdict::Pass) {
        return None;
    }
    let recheck = attempt.tasks.iter().find(|task| {
        task.id == binding.recheck_task_id
            && task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && task.is_failed_command_recheck()
    })?;
    let current_artifacts =
        crate::tester_evidence::failed_command_recheck_correction_artifacts(project_root, recheck)
            .ok()?;
    if current_artifacts.len() != binding.recheck_artifacts.len()
        || !current_artifacts
            .iter()
            .zip(&binding.recheck_artifacts)
            .all(|(current, bound)| {
                current.path == bound.path
                    && fs::read(project_root.join(&current.path)).is_ok_and(|bytes| {
                        format!("sha256:{:x}", Sha256::digest(&bytes)) == bound.digest
                    })
            })
    {
        return None;
    }
    let source_path = project_root.join(&binding.source_results.path);
    let source_bytes = fs::read(&source_path).ok()?;
    if format!("sha256:{:x}", Sha256::digest(&source_bytes)) != binding.source_results.digest {
        return None;
    }
    let source: crate::tester::TesterResults = serde_json::from_slice(&source_bytes).ok()?;
    let command = source.commands.get(binding.command_index)?;
    if source.candidate_commit.as_deref() != Some(recovery.candidate_commit.as_str())
        || source.error.is_some()
        || command.exit_code == 0
        || command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || command.command != binding.command
        || source.summary.fail
            != source
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || crate::tester_evidence::introduced_tester_failures_except_command(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
            binding.command_index,
            &command.command,
        ) != 0
        || crate::tester_evidence::unexplained_failed_tester_commands(
            &source_path,
            crate::tester_evidence::baseline_tester_results_path(project_root, attempt).as_deref(),
        ) != 1
    {
        return None;
    }
    Some(source_path)
}

fn candidate_workspace_is_clean_at(
    project_root: &Path,
    output: &crate::work_model::TaskOutput,
    candidate: &str,
) -> bool {
    let workspace = Path::new(&output.workspace_path);
    let workspace = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        project_root.join(workspace)
    };
    crate::git::run_stdout(
        &workspace,
        &["rev-parse", "HEAD"],
        "inspect evidence candidate",
    )
    .is_ok_and(|head| head == candidate)
        && crate::git::run_stdout(
            &workspace,
            &["status", "--porcelain", "--untracked-files=all"],
            "inspect evidence candidate cleanliness",
        )
        .is_ok_and(|status| status.is_empty())
}

fn parse_document(bytes: &[u8]) -> Result<EvidenceDocument> {
    if bytes.is_empty() || bytes.len() > MAX_EVIDENCE_BYTES {
        bail!("evidence document must be between 1 and {MAX_EVIDENCE_BYTES} bytes");
    }
    let document: EvidenceDocument = serde_json::from_slice(bytes)
        .context("evidence document must be valid schema-version-1 JSON")?;
    if document.schema_version != 1
        || document.producer.trim().is_empty()
        || document.check.trim().is_empty()
        || document.working_directory.trim().is_empty()
    {
        bail!("evidence document has an unsupported version or empty required field");
    }
    DateTime::parse_from_rfc3339(&document.run_at).context("evidence run_at must be RFC3339")?;
    Ok(document)
}

fn snapshot_path(work: &str, attempt: &str, digest: &str) -> Result<String> {
    let hex = digest
        .strip_prefix("sha256:")
        .context("invalid evidence digest")?;
    Ok(format!(
        ".fluent/work/artifacts/{work}/{attempt}/host-evidence/{hex}.json"
    ))
}
fn review_artifact_path(task: &Task) -> Option<String> {
    task.artifact_area
        .as_ref()
        .map(|area| format!("{}/review.md", area.path))
}
fn read_verdict(project_root: &Path, task: &Task) -> Option<Verdict> {
    fs::read_to_string(project_root.join(review_artifact_path(task)?))
        .ok()
        .map(|text| review::extract_verdict(&text))
}

fn latest_passing_tests_review<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    candidate: &str,
) -> Option<&'a Task> {
    attempt
        .tasks
        .iter()
        .rev()
        .find(|task| {
            task.kind == TaskKind::Review
                && task.role == "tests"
                && task.status == TaskStatus::Complete
                && task
                    .review_context
                    .as_ref()
                    .map(|context| context.candidate_commit.as_str())
                    == Some(candidate)
        })
        .filter(|task| read_verdict(project_root, task) == Some(Verdict::Pass))
}

fn publish_snapshot(project_root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let relative_path = Path::new(relative);
    let managed_root = Path::new(".fluent/work/artifacts");
    if relative_path.is_absolute() || !relative_path.starts_with(managed_root) {
        bail!("evidence snapshot path must stay under the managed artifact root");
    }
    let mut current = project_root.to_path_buf();
    for component in relative_path.components() {
        let Component::Normal(part) = component else {
            bail!("evidence snapshot path must use normal managed components");
        };
        current.push(part);
        if current == project_root.join(relative_path) {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("evidence snapshot path must not traverse a symlink")
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("evidence snapshot parent must be a directory")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("create managed evidence directory {}", current.display())
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let path = project_root.join(relative_path);
    match tempfile::NamedTempFile::new_in(path.parent().expect("snapshot has a parent")) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                        bail!("evidence snapshot path must not be a symlink");
                    }
                    if fs::read(&path)? != bytes {
                        bail!("evidence snapshot digest collision at {}", path.display());
                    }
                }
                Err(error) => return Err(error.error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    // The Work-model write that follows may survive a power loss, so make the
    // snapshot directory entry durable before that model can reference it.
    fs::File::open(path.parent().expect("snapshot has a parent"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_document_requires_versioned_complete_host_claim() {
        let accepted = br#"{"schema_version":1,"producer":"host","check":"fluent tester check","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"ok"}"#;
        assert_eq!(
            parse_document(accepted).unwrap().result,
            EvidenceResult::Pass
        );

        let missing_check = br#"{"schema_version":1,"producer":"host","check":"","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"ok"}"#;
        assert!(parse_document(missing_check).is_err());
    }

    #[test]
    fn focused_request_requires_missing_values_and_rejects_disagreement() {
        assert!(resolve_requested_value("command", None, None).is_err());
        assert!(
            resolve_requested_value(
                "command",
                Some("swift test --filter one"),
                Some("swift test --filter two")
            )
            .is_err()
        );
        assert_eq!(
            resolve_requested_value(
                "command",
                Some("swift test --filter one"),
                Some("swift test --filter one")
            )
            .unwrap(),
            "swift test --filter one"
        );
    }

    #[test]
    fn focused_command_allows_quoted_harness_expressions() {
        validate_focused_command(
            "cargo nextest run -E 'test(failed_split_write_retries_from_durable_transaction)'",
        )
        .unwrap();
        validate_focused_command("printf '(literal)'").unwrap();
    }

    #[test]
    fn focused_command_rejects_active_shell_control() {
        for command in [
            "printf one && printf two",
            "printf $(whoami)",
            "printf \"$(whoami)\"",
            "(printf one)",
            "printf one > output",
            "printf 'unterminated",
        ] {
            assert!(
                validate_focused_command(command).is_err(),
                "must reject {command:?}"
            );
        }
    }

    #[test]
    fn evidence_snapshot_is_exact_and_idempotent() {
        let root = tempfile::TempDir::new().unwrap();
        let bytes = b"exact host output\n";
        publish_snapshot(
            root.path(),
            ".fluent/work/artifacts/work/a/host-evidence/digest.json",
            bytes,
        )
        .unwrap();
        publish_snapshot(
            root.path(),
            ".fluent/work/artifacts/work/a/host-evidence/digest.json",
            bytes,
        )
        .unwrap();
        assert_eq!(
            fs::read(
                root.path()
                    .join(".fluent/work/artifacts/work/a/host-evidence/digest.json")
            )
            .unwrap(),
            bytes
        );
        assert!(
            publish_snapshot(
                root.path(),
                ".fluent/work/artifacts/work/a/host-evidence/digest.json",
                b"different"
            )
            .is_err()
        );
    }

    #[test]
    fn stale_evidence_frontier_is_rejected_without_mutation() {
        let root = tempfile::TempDir::new().unwrap();
        let mut item = work_model::WorkItem::planned("work-1", "Attach host evidence");
        item.add_initial_attempt("attempt-1").unwrap();
        let writer = &mut item.attempts[0].tasks[0];
        writer.status = TaskStatus::Complete;
        writer.output = Some(work_model::TaskOutput {
            workspace_id: "candidate".to_string(),
            workspace_path: root.path().display().to_string(),
            source_branch: "main".to_string(),
            base_commit: None,
            commit: "old-candidate".to_string(),
            no_change: None,
            learner_canonicalization: None,
        });

        let before = item.clone();
        assert!(
            validate_evidence_frontier(
                root.path(),
                &item,
                "attempt-1",
                "new-candidate",
                &["review.md".to_string()],
            )
            .is_err()
        );
        assert_eq!(item, before);
    }

    #[cfg(unix)]
    #[test]
    fn evidence_snapshot_rejects_symlinked_managed_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(root.path().join(".fluent/work")).unwrap();
        symlink(outside.path(), root.path().join(".fluent/work/artifacts")).unwrap();

        assert!(
            publish_snapshot(
                root.path(),
                ".fluent/work/artifacts/work/a/host-evidence/digest.json",
                b"exact host output\n",
            )
            .is_err()
        );
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
