use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::checkout_identity;
use super::{
    Checkout, CollaborationStore, ConfiguredProject, FileCollaborationStore, FluentThread,
    RunnerConfig, ThreadState, write_config,
};
use crate::setup::{ConfiguredSetup, InitSetupInputs};

/// One runner-owner-approved directory that may contain project checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDiscoveryRoot {
    pub label: String,
    pub path: PathBuf,
    pub maximum_depth: usize,
    pub allowed_participants: BTreeSet<String>,
    pub project_approvers: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlackProjectSetupState {
    Selecting {
        goal: Option<String>,
    },
    AwaitingSetupApproval {
        proposal: SlackSetupProposal,
        goal: Option<String>,
    },
    AwaitingCommitApproval {
        selection_id: String,
        review: InitializationReview,
        goal: Option<String>,
    },
}

const SETUP_STATE_PREFIX: &str = "project-setup-v1:";

pub fn encode_project_setup_state(state: &SlackProjectSetupState) -> Result<String> {
    Ok(format!(
        "{SETUP_STATE_PREFIX}{}",
        serde_json::to_string(state)?
    ))
}

pub fn decode_project_setup_state(value: Option<&str>) -> Result<Option<SlackProjectSetupState>> {
    value
        .and_then(|value| value.strip_prefix(SETUP_STATE_PREFIX))
        .map(serde_json::from_str)
        .transpose()
        .map_err(Into::into)
}

pub fn project_discovery_roots_path(home: &Path) -> PathBuf {
    home.join(".config/fluent/slack/project-roots.json")
}

pub fn read_project_discovery_roots(home: &Path) -> Result<Vec<ProjectDiscoveryRoot>> {
    let path = project_discovery_roots_path(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let roots: Vec<ProjectDiscoveryRoot> =
        serde_json::from_slice(&fs::read(path)?).context("read Slack project discovery roots")?;
    let mut labels = BTreeSet::new();
    for root in &roots {
        if root.label.trim().is_empty()
            || root.maximum_depth > 8
            || root.allowed_participants.is_empty()
            || !root.project_approvers.is_subset(&root.allowed_participants)
        {
            anyhow::bail!("Slack project discovery root policy is invalid")
        }
        if !labels.insert(root.label.clone()) {
            anyhow::bail!(
                "Slack project discovery root policy has duplicate project label {:?}",
                root.label
            )
        }
    }
    Ok(roots)
}

/// A path-free checkout choice safe to render in Slack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCheckoutOffer {
    pub selection_id: String,
    pub project_label: String,
    pub checkout_label: String,
}

#[derive(Debug, Clone)]
pub(super) struct DiscoveredCheckout {
    offer: ProjectCheckoutOffer,
    pub(super) path: PathBuf,
    identity: String,
    approvers: BTreeSet<String>,
    allowed_participants: BTreeSet<String>,
}

/// A point-in-time, owner-bounded discovery result. Public offers contain no
/// host paths; later mutations resolve through the private canonical entries.
#[derive(Debug, Clone, Default)]
pub struct ProjectDiscoveryIndex {
    entries: BTreeMap<String, DiscoveredCheckout>,
}

impl ProjectDiscoveryIndex {
    pub fn offers(&self) -> Vec<ProjectCheckoutOffer> {
        self.entries
            .values()
            .map(|entry| entry.offer.clone())
            .collect()
    }

    pub(super) fn entry(&self, selection_id: &str) -> Option<&DiscoveredCheckout> {
        self.entries.get(selection_id)
    }
}

/// Complete configured-init choices collected before checkout mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackInitChoices {
    pub coder_profile: String,
    pub follow_up_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_coder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_coder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_effort: Option<String>,
}

impl SlackInitChoices {
    fn configured_setup(&self) -> Result<ConfiguredSetup> {
        ConfiguredSetup::from_inputs(InitSetupInputs {
            coder_profile: Some(self.coder_profile.clone()),
            follow_up_mode: Some(self.follow_up_mode.clone()),
            write_coder: self.write_coder.clone(),
            write_model: self.write_model.clone(),
            write_effort: self.write_effort.clone(),
            review_coder: self.review_coder.clone(),
            review_model: self.review_model.clone(),
            review_effort: self.review_effort.clone(),
        })?
        .context("Slack project setup requires configured initialization choices")
    }
}

/// Exact path-free setup proposal presented for Slack approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackSetupProposal {
    pub selection_id: String,
    pub project_label: String,
    pub checkout_label: String,
    pub choices: SlackInitChoices,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredInitReport {
    pub already_initialized: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackSetupOutcome {
    AlreadyInitialized { configuration: String },
    Initialized(InitializationReview),
    PreflightFailed { boundary: String, recovery: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitGeneratedChange {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub before_mode: Option<String>,
    pub after_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotEntry {
    bytes: Vec<u8>,
    mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CheckoutSnapshot {
    entries: BTreeMap<PathBuf, SnapshotEntry>,
    directories: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationReview {
    pub summary: String,
    pub expected_head: String,
    pub expected_index_sha256: String,
    pub preexisting_status: String,
    pub preexisting_paths: Vec<String>,
    pub generated_changes: Vec<InitGeneratedChange>,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationCommit {
    pub commit: String,
    pub slack_approver: String,
    pub reviewed_revision: String,
}

/// Existing configured-initialization boundary used by Slack setup. Its
/// implementation owns repository, durable-path, host, and provider preflights.
pub trait ConfiguredInitBoundary {
    fn initialize(
        &mut self,
        checkout: &Path,
        choices: &SlackInitChoices,
    ) -> Result<ConfiguredInitReport>;
}

/// Invoke the same configured `fluent init` route used by local setup.
pub struct ExistingConfiguredInitBoundary {
    executable: PathBuf,
}

impl ExistingConfiguredInitBoundary {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

impl ConfiguredInitBoundary for ExistingConfiguredInitBoundary {
    fn initialize(
        &mut self,
        checkout: &Path,
        choices: &SlackInitChoices,
    ) -> Result<ConfiguredInitReport> {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(checkout)
            .arg("init")
            .args(["--coder-profile", &choices.coder_profile])
            .args(["--follow-up-mode", &choices.follow_up_mode]);
        for (flag, value) in [
            ("--write-coder", &choices.write_coder),
            ("--write-model", &choices.write_model),
            ("--write-effort", &choices.write_effort),
            ("--review-coder", &choices.review_coder),
            ("--review-model", &choices.review_model),
            ("--review-effort", &choices.review_effort),
        ] {
            if let Some(value) = value {
                command.args([flag, value]);
            }
        }
        let output = command
            .output()
            .context("invoke configured initialization")?;
        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(ConfiguredInitReport {
            already_initialized: false,
            summary: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

pub fn prepare_slack_project_setup(
    index: &ProjectDiscoveryIndex,
    selection_id: &str,
    choices: SlackInitChoices,
) -> Result<SlackSetupProposal> {
    choices.configured_setup()?;
    let entry = index
        .entry(selection_id)
        .context("selected checkout is unavailable")?;
    let mut proposal = SlackSetupProposal {
        selection_id: selection_id.to_owned(),
        project_label: entry.offer.project_label.clone(),
        checkout_label: entry.offer.checkout_label.clone(),
        choices,
        revision: String::new(),
    };
    proposal.revision = setup_revision(&proposal)?;
    Ok(proposal)
}

pub fn approve_slack_project_setup(
    current_index: &ProjectDiscoveryIndex,
    proposal: &SlackSetupProposal,
    reviewed_revision: &str,
    actor: &str,
    boundary: &mut dyn ConfiguredInitBoundary,
) -> Result<SlackSetupOutcome> {
    if reviewed_revision != proposal.revision || setup_revision(proposal)? != proposal.revision {
        anyhow::bail!("project setup approval is stale; review the current proposal")
    }
    proposal.choices.configured_setup()?;
    let entry = current_index
        .entry(&proposal.selection_id)
        .context("selected checkout is no longer available")?;
    if entry.offer.project_label != proposal.project_label
        || entry.offer.checkout_label != proposal.checkout_label
        || !entry.approvers.contains(actor)
    {
        anyhow::bail!("current project authority does not permit setup")
    }
    if checkout_identity(&entry.path)? != entry.identity {
        anyhow::bail!("selected checkout identity changed; select it again")
    }
    let fluent_dir = entry.path.join(".fluent");
    let project_config = fluent_dir.join("config.yaml");
    if project_config.is_file() {
        crate::config::preflight_project_coder_config(&entry.path)
            .context("validate existing Fluent project configuration")?;
        let configuration = fs::read_to_string(project_config)
            .context("read existing Fluent project configuration")?;
        return Ok(SlackSetupOutcome::AlreadyInitialized { configuration });
    }
    let before = snapshot_checkout(&entry.path)?;
    let expected_head = crate::git::run_stdout(
        &entry.path,
        &["rev-parse", "HEAD"],
        "record initialization HEAD",
    )?;
    let expected_index_sha256 = index_sha256(&entry.path)?;
    let preexisting_status = crate::git::run_stdout(
        &entry.path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        "record pre-existing checkout changes",
    )?;
    let preexisting_paths = preexisting_paths(&entry.path)?;
    match boundary.initialize(&entry.path, &proposal.choices) {
        Ok(report) => {
            let after = snapshot_checkout(&entry.path)?;
            let generated_changes = generated_changes(&before, &after)?;
            let mut review = InitializationReview {
                summary: report.summary,
                expected_head,
                expected_index_sha256,
                preexisting_status,
                preexisting_paths,
                generated_changes,
                revision: String::new(),
            };
            review.revision = format!(
                "init-review-v1:{:x}",
                Sha256::digest(serde_json::to_vec(&review)?)
            );
            Ok(SlackSetupOutcome::Initialized(review))
        }
        Err(error) => {
            let diagnostic = error.to_string();
            restore_checkout(&entry.path, &before)?;
            Ok(SlackSetupOutcome::PreflightFailed {
                boundary: classify_init_boundary(&diagnostic).into(),
                recovery: diagnostic,
            })
        }
    }
}

pub fn commit_reviewed_initialization(
    current_index: &ProjectDiscoveryIndex,
    selection_id: &str,
    review: &InitializationReview,
    reviewed_revision: &str,
    actor: &str,
) -> Result<InitializationCommit> {
    let entry = current_index
        .entry(selection_id)
        .context("selected checkout is no longer available")?;
    if !entry.approvers.contains(actor) {
        anyhow::bail!("current project authority does not permit the initialization commit")
    }
    let mut exact_review = review.clone();
    exact_review.revision.clear();
    let actual_revision = format!(
        "init-review-v1:{:x}",
        Sha256::digest(serde_json::to_vec(&exact_review)?)
    );
    if reviewed_revision != review.revision || actual_revision != review.revision {
        anyhow::bail!("initialization commit approval is stale; review the exact changes again")
    }
    if checkout_identity(&entry.path)? != entry.identity {
        anyhow::bail!("selected checkout identity changed; resolve the checkout locally")
    }
    let head = crate::git::run_stdout(
        &entry.path,
        &["rev-parse", "HEAD"],
        "revalidate initialization HEAD",
    )?;
    if head != review.expected_head {
        anyhow::bail!("checkout HEAD changed; resolve the checkout locally")
    }
    if index_sha256(&entry.path)? != review.expected_index_sha256 {
        anyhow::bail!("checkout index changed; resolve the checkout locally")
    }
    let current = snapshot_checkout(&entry.path)?;
    for change in &review.generated_changes {
        let path = PathBuf::from(&change.path);
        if review.preexisting_paths.contains(&change.path) {
            anyhow::bail!(
                "initialization overlaps pre-existing change {}; resolve the checkout locally",
                change.path
            )
        }
        let reviewed_after = change.after.as_ref().map(|bytes| SnapshotEntry {
            bytes: bytes.clone(),
            mode: change.after_mode.clone().unwrap_or_else(|| "100644".into()),
        });
        if current.entries.get(&path).cloned() != reviewed_after {
            anyhow::bail!(
                "reviewed initialization bytes changed at {}; resolve the checkout locally",
                change.path
            )
        }
    }
    if review.generated_changes.is_empty() {
        anyhow::bail!("initialization produced no changes to commit")
    }

    let temporary = tempfile::tempdir().context("create temporary initialization index")?;
    let temporary_index = temporary.path().join("index");
    let environment = ("GIT_INDEX_FILE", temporary_index.as_path());
    crate::git::run_stdout_with_env(
        &entry.path,
        &["read-tree", &review.expected_head],
        environment,
        "seed temporary initialization index",
    )?;
    for change in &review.generated_changes {
        match &change.after {
            Some(bytes) => {
                let blob = crate::git::run_stdout_with_stdin_and_env(
                    &entry.path,
                    &["hash-object", "-w", "--stdin"],
                    bytes,
                    environment,
                    "write reviewed initialization blob",
                )?;
                crate::git::run_stdout_with_env(
                    &entry.path,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        change.after_mode.as_deref().unwrap_or("100644"),
                        &blob,
                        &change.path,
                    ],
                    environment,
                    "add reviewed initialization blob",
                )?;
            }
            None => {
                crate::git::run_stdout_with_env(
                    &entry.path,
                    &["update-index", "--force-remove", "--", &change.path],
                    environment,
                    "remove reviewed initialization path",
                )?;
            }
        }
    }
    let tree = crate::git::run_stdout_with_env(
        &entry.path,
        &["write-tree"],
        environment,
        "write exact initialization tree",
    )?;
    let commit = crate::git::run_stdout(
        &entry.path,
        &[
            "commit-tree",
            &tree,
            "-p",
            &review.expected_head,
            "-m",
            "Initialize Fluent",
        ],
        "create initialization commit",
    )?;
    let reference = crate::git::run_stdout(
        &entry.path,
        &["symbolic-ref", "-q", "HEAD"],
        "resolve initialization branch",
    )?;
    crate::git::run(
        &entry.path,
        &["update-ref", &reference, &commit, &review.expected_head],
        "advance initialization branch with compare-and-swap",
    )?;
    Ok(InitializationCommit {
        commit,
        slack_approver: actor.into(),
        reviewed_revision: review.revision.clone(),
    })
}

fn preexisting_paths(checkout: &Path) -> Result<Vec<String>> {
    let output = crate::git::run_raw(
        checkout,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        anyhow::bail!("git status failed while recording pre-existing paths")
    }
    let mut paths = Vec::new();
    let mut fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let field = String::from_utf8(field.to_vec()).context("Git returned a non-UTF-8 path")?;
        let status = field.get(..2).unwrap_or_default();
        let path = field
            .get(3..)
            .context("Git returned malformed porcelain status")?;
        paths.push(path.to_owned());
        if status.starts_with('R') || status.starts_with('C') {
            let _ = fields.next();
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn snapshot_checkout(checkout: &Path) -> Result<CheckoutSnapshot> {
    fn visit(root: &Path, directory: &Path, snapshot: &mut CheckoutSnapshot) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        for child in fs::read_dir(directory)? {
            let child = child?;
            if directory == root && child.file_name() == ".git" {
                continue;
            }
            let path = child.path();
            let kind = child.file_type()?;
            if kind.is_dir() {
                snapshot
                    .directories
                    .insert(path.strip_prefix(root)?.to_owned());
                visit(root, &path, snapshot)?;
            } else if kind.is_file() {
                let executable = fs::metadata(&path)?.permissions().mode() & 0o111 != 0;
                snapshot.entries.insert(
                    path.strip_prefix(root)?.to_owned(),
                    SnapshotEntry {
                        bytes: fs::read(path)?,
                        mode: if executable { "100755" } else { "100644" }.into(),
                    },
                );
            } else if kind.is_symlink() {
                snapshot.entries.insert(
                    path.strip_prefix(root)?.to_owned(),
                    SnapshotEntry {
                        bytes: fs::read_link(path)?.as_os_str().as_encoded_bytes().to_vec(),
                        mode: "120000".into(),
                    },
                );
            }
        }
        Ok(())
    }
    let mut snapshot = CheckoutSnapshot::default();
    visit(checkout, checkout, &mut snapshot)?;
    Ok(snapshot)
}

fn generated_changes(
    before: &CheckoutSnapshot,
    after: &CheckoutSnapshot,
) -> Result<Vec<InitGeneratedChange>> {
    let paths = before
        .entries
        .keys()
        .chain(after.entries.keys())
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter(|path| before.entries.get(*path) != after.entries.get(*path))
        .map(|path| {
            Ok(InitGeneratedChange {
                path: path
                    .to_str()
                    .context("initialization produced a non-UTF-8 path")?
                    .replace('\\', "/"),
                before: before.entries.get(path).map(|entry| entry.bytes.clone()),
                after: after.entries.get(path).map(|entry| entry.bytes.clone()),
                before_mode: before.entries.get(path).map(|entry| entry.mode.clone()),
                after_mode: after.entries.get(path).map(|entry| entry.mode.clone()),
            })
        })
        .collect()
}

fn restore_checkout(checkout: &Path, expected: &CheckoutSnapshot) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let current = snapshot_checkout(checkout)?;
    for relative in current.entries.keys() {
        if !expected.entries.contains_key(relative) {
            fs::remove_file(checkout.join(relative))?;
        }
    }
    for (relative, entry) in &expected.entries {
        let path = checkout.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let current_entry = current.entries.get(relative);
        if current_entry == Some(entry) {
            continue;
        }
        if path.symlink_metadata().is_ok() {
            fs::remove_file(&path)?;
        }
        if entry.mode == "120000" {
            symlink(std::ffi::OsStr::from_bytes(&entry.bytes), &path)?;
        } else {
            fs::write(&path, &entry.bytes)?;
            let permissions = if entry.mode == "100755" { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(permissions))?;
        }
    }
    let mut extra_directories = current
        .directories
        .difference(&expected.directories)
        .cloned()
        .collect::<Vec<_>>();
    extra_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for relative in extra_directories {
        let path = checkout.join(relative);
        if path
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_none())
        {
            fs::remove_dir(path)?;
        }
    }
    if snapshot_checkout(checkout)? != *expected {
        anyhow::bail!("configured initialization failure could not restore the checkout exactly")
    }
    Ok(())
}

fn index_sha256(checkout: &Path) -> Result<String> {
    let git_dir = crate::git::run_stdout(
        checkout,
        &["rev-parse", "--absolute-git-dir"],
        "locate checkout index",
    )?;
    let bytes = fs::read(PathBuf::from(git_dir).join("index")).unwrap_or_default();
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn classify_init_boundary(diagnostic: &str) -> &'static str {
    let diagnostic = diagnostic.to_ascii_lowercase();
    if diagnostic.contains("repository") || diagnostic.contains("git root") {
        "repository"
    } else if diagnostic.contains("durable") || diagnostic.contains("ignore") {
        "durable-path"
    } else if diagnostic.contains("sandbox") || diagnostic.contains("host") {
        "host"
    } else if diagnostic.contains("provider")
        || diagnostic.contains("codex")
        || diagnostic.contains("claude")
    {
        "provider"
    } else if diagnostic.contains("config") {
        "configuration"
    } else {
        "configured-init"
    }
}

fn setup_revision(proposal: &SlackSetupProposal) -> Result<String> {
    let mut reviewed = proposal.clone();
    reviewed.revision.clear();
    Ok(format!(
        "setup-v1:{:x}",
        Sha256::digest(serde_json::to_vec(&reviewed)?)
    ))
}

/// Discover canonical Git roots without following authority outside an
/// approved directory or walking deeper than the owner-supplied bound.
pub fn discover_project_checkouts(
    roots: &[ProjectDiscoveryRoot],
    participant: &str,
) -> Result<ProjectDiscoveryIndex> {
    let mut index = ProjectDiscoveryIndex::default();
    for root in roots {
        if !root.allowed_participants.contains(participant) {
            continue;
        }
        let canonical_root = root
            .path
            .canonicalize()
            .with_context(|| format!("canonicalize discovery root {}", root.label))?;
        let mut pending = VecDeque::from([(canonical_root.clone(), 0usize)]);
        let mut visited = BTreeSet::new();
        while let Some((candidate, depth)) = pending.pop_front() {
            let canonical = match candidate.canonicalize() {
                Ok(path) if path.starts_with(&canonical_root) => path,
                _ => continue,
            };
            if !visited.insert(canonical.clone()) {
                continue;
            }
            if git_root_is_exact(&canonical) {
                let identity = checkout_identity(&canonical)?;
                let digest = identity.strip_prefix("checkout-v1:").unwrap_or(&identity);
                let selection_id = format!("{}:{}", root.label, &digest[..12.min(digest.len())]);
                let checkout_label = canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("checkout")
                    .to_owned();
                index.entries.insert(
                    selection_id.clone(),
                    DiscoveredCheckout {
                        offer: ProjectCheckoutOffer {
                            selection_id,
                            project_label: root.label.clone(),
                            checkout_label,
                        },
                        path: canonical.clone(),
                        identity,
                        approvers: root.project_approvers.clone(),
                        allowed_participants: root.allowed_participants.clone(),
                    },
                );
            }
            if depth == root.maximum_depth {
                continue;
            }
            let Ok(children) = fs::read_dir(&canonical) else {
                continue;
            };
            for child in children.flatten() {
                if child.file_name() == ".git" {
                    continue;
                }
                if child
                    .file_type()
                    .is_ok_and(|kind| kind.is_dir() || kind.is_symlink())
                {
                    pending.push_back((child.path(), depth + 1));
                }
            }
        }
    }
    Ok(index)
}

/// Publish the committed checkout to the runner registry and promote the
/// original setup thread when it also carried a product goal.
pub fn complete_project_enablement(
    home: &Path,
    config: &mut RunnerConfig,
    current_index: &ProjectDiscoveryIndex,
    selection_id: &str,
    committed: &InitializationCommit,
    thread: &mut FluentThread,
    original_goal: Option<&str>,
) -> Result<()> {
    let entry = current_index
        .entry(selection_id)
        .context("selected checkout is no longer available")?;
    if !entry.approvers.contains(&committed.slack_approver) {
        anyhow::bail!("initialization commit approver no longer has project authority")
    }
    let head = crate::git::run_stdout(
        &entry.path,
        &["rev-parse", "HEAD"],
        "verify committed initialization",
    )?;
    if head != committed.commit || !entry.path.join(".fluent").is_dir() {
        anyhow::bail!("reviewed initialization is not the checkout's current commit")
    }
    let branch = crate::git::run_stdout(
        &entry.path,
        &["branch", "--show-current"],
        "record enabled checkout branch",
    )?;
    let identity = checkout_identity(&entry.path)?;
    let existing = config.projects.get(&entry.offer.project_label);
    if existing.is_some_and(|project| project.checkout.identity != identity) {
        anyhow::bail!(
            "project label {:?} already belongs to another checkout",
            entry.offer.project_label
        )
    }
    let default_provider = if let Some(project) = existing {
        project.default_provider.clone()
    } else {
        let default_provider = if config.enabled_providers.contains("codex") {
            "codex".to_owned()
        } else {
            config
                .enabled_providers
                .iter()
                .next()
                .context("Slack runner has no enabled conversation provider")?
                .clone()
        };
        config.projects.insert(
            entry.offer.project_label.clone(),
            ConfiguredProject {
                label: entry.offer.project_label.clone(),
                checkout: Checkout {
                    label: entry.offer.checkout_label.clone(),
                    path: entry.path.clone(),
                    identity,
                    branch,
                    commit: committed.commit.clone(),
                },
                allowed_participants: entry.allowed_participants.clone(),
                default_provider: default_provider.clone(),
                enabled_providers: BTreeSet::from([default_provider.clone()]),
                project_approvers: entry.approvers.clone(),
                github_remote: None,
            },
        );
        default_provider
    };
    write_config(home, config)?;

    thread.state = ThreadState::Active;
    thread.project_label = Some(entry.offer.project_label.clone());
    thread.provider = Some(default_provider);
    thread.pending_question = None;
    thread
        .project_initialization_approvals
        .push(super::ProjectInitializationApproval {
            commit: committed.commit.clone(),
            reviewed_revision: committed.reviewed_revision.clone(),
            slack_approver: committed.slack_approver.clone(),
        });
    if let Some(goal) = original_goal.filter(|goal| !goal.trim().is_empty()) {
        thread.slice = None;
        thread.linked_thread_intent = Some(format!("slack-{}", thread.id.replace('.', "-")));
        thread.planning_stage = Some("brief".into());
        let action_id = format!("setup-goal:{}", committed.commit);
        thread.conversation_inputs.push(super::ConversationInput {
            event_id: action_id.clone(),
            source_timestamp: thread.id.clone(),
            actor: committed.slack_approver.clone(),
            content: goal.to_owned(),
            corrections: Vec::new(),
            source_deleted: false,
        });
        thread.pending_turn = Some(super::PendingTurn {
            action_id,
            source_timestamp: thread.id.clone(),
            contribution: goal.to_owned(),
            actor: committed.slack_approver.clone(),
            starts_session: true,
            provider_turn_id: None,
            completed_reply: None,
            dispatch_state: super::PendingTurnDispatchState::Runnable,
            retry_at_unix_seconds: None,
            delivery_attempt: 0,
        });
    } else {
        thread.slice = None;
        thread.linked_thread_intent = None;
        thread.planning_stage = None;
        thread.pending_turn = None;
    }
    FileCollaborationStore::new(home).save_thread(config, thread)
}

fn git_root_is_exact(path: &Path) -> bool {
    crate::git::run_stdout(path, &["rev-parse", "--show-toplevel"], "discover Git root")
        .ok()
        .and_then(|root| PathBuf::from(root).canonicalize().ok())
        .is_some_and(|root| root == path)
}
