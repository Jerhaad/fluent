use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_write::atomic_write;

use super::{
    CollaborationStore, FluentThread, InboundEnvelope, InboxState, OutboundDelivery, RunnerConfig,
    validate_thread_identity,
};

#[derive(Debug, Clone)]
pub struct FileCollaborationStore {
    home: PathBuf,
    thread_commit_crash: Option<ThreadCommitCrashPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadCommitCrashPoint {
    AfterIndex,
    AfterSnapshot,
}

impl FileCollaborationStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            thread_commit_crash: None,
        }
    }

    #[doc(hidden)]
    pub fn with_thread_commit_crash(
        home: impl Into<PathBuf>,
        thread_commit_crash: ThreadCommitCrashPoint,
    ) -> Self {
        Self {
            home: home.into(),
            thread_commit_crash: Some(thread_commit_crash),
        }
    }
}

pub fn state_root(home: &Path) -> PathBuf {
    home.join(".config/fluent/slack")
}

pub fn config_path(home: &Path) -> PathBuf {
    state_root(home).join("config.json")
}

pub(super) fn operational_status_path(home: &Path) -> PathBuf {
    state_root(home).join("status.json")
}

fn inbox_path(home: &Path) -> PathBuf {
    state_root(home).join("inbox.json")
}

fn outbox_path(home: &Path) -> PathBuf {
    state_root(home).join("outbox.json")
}

fn thread_index_path(home: &Path) -> PathBuf {
    state_root(home).join("threads.json")
}

fn thread_commit_dir(home: &Path) -> PathBuf {
    state_root(home).join("thread-commits")
}

fn cursor_path(home: &Path) -> PathBuf {
    state_root(home).join("cursors.json")
}

fn thread_path(project: &Path, id: &str) -> Result<PathBuf> {
    validate_thread_identity(id)?;
    Ok(project
        .join(".fluent/work/slack/threads")
        .join(format!("{id}.json")))
}

fn project_inbox_path(project: &Path, id: &str) -> PathBuf {
    project
        .join(".fluent/work/slack/inbox")
        .join(format!("{}.json", digest(id.as_bytes())))
}

pub(super) fn ensure_root(home: &Path) -> Result<()> {
    fs::create_dir_all(state_root(home))?;
    Ok(())
}

pub(super) fn read_json<T: for<'a> Deserialize<'a> + Default>(path: PathBuf) -> Result<T> {
    match fs::read(path) {
        Ok(raw) => Ok(serde_json::from_slice(&raw)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InboxEntry {
    event: InboundEnvelope,
    processed: bool,
    #[serde(default)]
    project_label: Option<String>,
    #[serde(default)]
    deduplication_identity: String,
}

fn inbox_deduplication_identity(event: &InboundEnvelope) -> String {
    digest(
        format!(
            "{}\0{}\0{}\0{}",
            event.channel, event.timestamp, event.user, event.text
        )
        .as_bytes(),
    )
}

fn routing_envelope(event: &InboundEnvelope) -> InboundEnvelope {
    InboundEnvelope {
        id: event.id.clone(),
        acknowledgement_id: String::new(),
        channel: event.channel.clone(),
        timestamp: event.timestamp.clone(),
        thread_timestamp: event.thread_timestamp.clone(),
        user: String::new(),
        text: String::new(),
    }
}

pub(super) fn persist_inbox(
    home: &Path,
    config: &RunnerConfig,
    event: &InboundEnvelope,
) -> Result<InboxState> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    recover_thread_commits(home, config)?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    if let Some(entry) = inbox.get(&event.id) {
        return Ok(if entry.processed {
            InboxState::Processed
        } else {
            InboxState::Pending
        });
    }
    let deduplication_identity = inbox_deduplication_identity(event);
    if inbox.values().any(|entry| {
        entry.processed
            && (entry.deduplication_identity == deduplication_identity
                || (entry.deduplication_identity.is_empty()
                    && entry.event.channel == event.channel
                    && entry.event.timestamp == event.timestamp
                    && entry.event.user == event.user
                    && entry.event.text == event.text))
    }) {
        return Ok(InboxState::Processed);
    }
    let project_label = load_thread_index(home)?
        .get(&event.thread_id())
        .and_then(|thread| thread.project_label.clone());
    let persisted_event = if let Some(label) = &project_label {
        let project = config
            .projects
            .get(label)
            .with_context(|| format!("configured project {label} is unavailable"))?;
        let path = project_inbox_path(&project.checkout.path, &event.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &serde_json::to_vec_pretty(event)?)?;
        routing_envelope(event)
    } else {
        event.clone()
    };
    inbox.insert(
        event.id.clone(),
        InboxEntry {
            event: persisted_event,
            processed: false,
            project_label,
            deduplication_identity,
        },
    );
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(InboxState::Pending)
}

impl CollaborationStore for FileCollaborationStore {
    fn persist_inbox(&self, config: &RunnerConfig, event: &InboundEnvelope) -> Result<InboxState> {
        persist_inbox(&self.home, config, event)
    }

    fn mark_inbox_processed(&self, id: &str) -> Result<()> {
        mark_inbox_processed(&self.home, id)
    }

    fn bind_inbox_event(&self, config: &RunnerConfig, id: &str, project_label: &str) -> Result<()> {
        bind_inbox_event(&self.home, config, id, project_label)
    }

    fn load_pending_inbox(&self, config: &RunnerConfig) -> Result<Vec<InboundEnvelope>> {
        load_pending_inbox(&self.home, config)
    }

    fn load_outbox(&self) -> Result<Vec<OutboundDelivery>> {
        load_outbox(&self.home)
    }

    fn save_outbox(&self, deliveries: &[OutboundDelivery]) -> Result<()> {
        save_outbox(&self.home, deliveries)
    }

    fn persist_outbox(&self, delivery: OutboundDelivery) -> Result<()> {
        persist_outbox(&self.home, delivery)
    }

    fn load_threads(&self, config: &RunnerConfig) -> Result<BTreeMap<String, FluentThread>> {
        load_threads(&self.home, config)
    }

    fn load_thread(&self, config: &RunnerConfig, id: &str) -> Result<Option<FluentThread>> {
        load_thread(&self.home, config, id)
    }

    fn save_thread(&self, config: &RunnerConfig, thread: &FluentThread) -> Result<()> {
        save_thread_with_crash(&self.home, config, thread, self.thread_commit_crash)
    }

    fn recovery_cursor(&self, channel: &str, thread: &str) -> Result<Option<String>> {
        let cursors: BTreeMap<String, String> = read_json(cursor_path(&self.home))?;
        Ok(cursors.get(&format!("{channel}:{thread}")).cloned())
    }

    fn save_recovery_cursor(&self, channel: &str, thread: &str, cursor: &str) -> Result<()> {
        ensure_root(&self.home)?;
        let _lock = state_lock(&self.home)?;
        let mut cursors: BTreeMap<String, String> = read_json(cursor_path(&self.home))?;
        cursors.insert(format!("{channel}:{thread}"), cursor.to_owned());
        atomic_write(
            &cursor_path(&self.home),
            &serde_json::to_vec_pretty(&cursors)?,
        )?;
        Ok(())
    }
}

pub(super) fn bind_inbox_event(
    home: &Path,
    config: &RunnerConfig,
    id: &str,
    project_label: &str,
) -> Result<()> {
    let _lock = state_lock(home)?;
    let project = config
        .projects
        .get(project_label)
        .with_context(|| format!("configured project {project_label} is unavailable"))?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let entry = inbox
        .get_mut(id)
        .context("persisted inbox event is missing")?;
    if entry.project_label.as_deref() == Some(project_label) {
        return Ok(());
    }
    if entry.project_label.is_some() {
        bail!("persisted inbox event is already bound to another project");
    }
    let project_path = project_inbox_path(&project.checkout.path, id);
    if let Some(parent) = project_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&project_path, &serde_json::to_vec_pretty(&entry.event)?)?;
    entry.event = routing_envelope(&entry.event);
    entry.project_label = Some(project_label.to_owned());
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(())
}

pub(super) fn mark_inbox_processed(home: &Path, id: &str) -> Result<()> {
    let _lock = state_lock(home)?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let entry = inbox
        .get_mut(id)
        .context("persisted inbox event is missing")?;
    entry.processed = true;
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(())
}

pub(super) fn load_pending_inbox(
    home: &Path,
    config: &RunnerConfig,
) -> Result<Vec<InboundEnvelope>> {
    let inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let mut pending = Vec::new();
    for entry in inbox.into_values().filter(|entry| !entry.processed) {
        if let Some(label) = entry.project_label {
            let project = config
                .projects
                .get(&label)
                .with_context(|| format!("configured project {label} is unavailable"))?;
            let path = project_inbox_path(&project.checkout.path, &entry.event.id);
            let bytes = fs::read(&path)
                .with_context(|| format!("bound inbox event {} is unavailable", entry.event.id))?;
            pending.push(serde_json::from_slice(&bytes)?);
        } else {
            pending.push(entry.event);
        }
    }
    pending.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then(left.id.cmp(&right.id))
    });
    Ok(pending)
}

pub(super) fn load_outbox(home: &Path) -> Result<Vec<OutboundDelivery>> {
    read_json(outbox_path(home))
}

pub(super) fn save_outbox(home: &Path, value: &[OutboundDelivery]) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(value)?).map_err(Into::into)
}

pub(super) fn persist_outbox(home: &Path, delivery: OutboundDelivery) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut outbox = load_outbox(home)?;
    if let Some(existing) = outbox.iter().find(|item| item.id == delivery.id) {
        if existing.channel != delivery.channel
            || existing.thread_timestamp != delivery.thread_timestamp
            || existing.text != delivery.text
        {
            bail!("durable outbox identity is immutable");
        }
    } else {
        outbox.push(delivery);
        atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(&outbox)?)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThreadCommit {
    previous: Option<FluentThread>,
    next: FluentThread,
    snapshot_digest: String,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_thread_index(home: &Path) -> Result<BTreeMap<String, FluentThread>> {
    read_json(thread_index_path(home))
}

fn recover_thread_commits(home: &Path, config: &RunnerConfig) -> Result<()> {
    let directory = thread_commit_dir(home);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut index = load_thread_index(home)?;
    for entry in entries {
        let path = entry?.path();
        let commit: ThreadCommit = serde_json::from_slice(&fs::read(&path)?)?;
        let snapshot_matches = commit
            .next
            .project_label
            .as_ref()
            .and_then(|label| config.projects.get(label))
            .and_then(|project| thread_path(&project.checkout.path, &commit.next.id).ok())
            .and_then(|snapshot| fs::read(snapshot).ok())
            .is_some_and(|bytes| digest(&bytes) == commit.snapshot_digest);
        if snapshot_matches {
            index.insert(commit.next.id.clone(), commit.next);
        } else if let Some(previous) = commit.previous {
            index.insert(previous.id.clone(), previous);
        } else {
            index.remove(&commit.next.id);
        }
        atomic_write(
            &thread_index_path(home),
            &serde_json::to_vec_pretty(&index)?,
        )?;
        fs::remove_file(path)?;
    }
    Ok(())
}

pub(super) fn load_threads(
    home: &Path,
    config: &RunnerConfig,
) -> Result<BTreeMap<String, FluentThread>> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    recover_thread_commits(home, config)?;
    load_thread_index(home)
}

pub(super) fn load_thread(
    home: &Path,
    config: &RunnerConfig,
    id: &str,
) -> Result<Option<FluentThread>> {
    let index = load_threads(home, config)?;
    let Some(stub) = index.get(id) else {
        return Ok(None);
    };
    let Some(project) = stub
        .project_label
        .as_ref()
        .and_then(|label| config.projects.get(label))
    else {
        return Ok(Some(stub.clone()));
    };
    match fs::read(thread_path(&project.checkout.path, id)?) {
        Ok(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("bound Slack thread snapshot {id} is unavailable")
        }
        Err(error) => Err(error.into()),
    }
}

fn save_thread_with_crash(
    home: &Path,
    config: &RunnerConfig,
    thread: &FluentThread,
    crash: Option<ThreadCommitCrashPoint>,
) -> Result<()> {
    validate_thread_identity(&thread.id)?;
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    recover_thread_commits(home, config)?;
    let mut index = load_thread_index(home)?;
    let mut private_mapping = thread.clone();
    if private_mapping.project_label.is_some() {
        private_mapping.provider_session = None;
        private_mapping.slice = None;
        private_mapping.planning_stage = None;
        private_mapping.linked_work = None;
        private_mapping.pending_question = None;
        private_mapping.processed_events.clear();
        private_mapping.last_connection = None;
        private_mapping.pending_turn = None;
    }
    let snapshot = if let Some(project) = thread
        .project_label
        .as_ref()
        .and_then(|label| config.projects.get(label))
    {
        let path = thread_path(&project.checkout.path, &thread.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Some(path)
    } else {
        None
    };
    let snapshot_bytes = serde_json::to_vec_pretty(thread)?;
    fs::create_dir_all(thread_commit_dir(home))?;
    let commit_path = thread_commit_dir(home).join(format!("{}.json", thread.id));
    let commit = ThreadCommit {
        previous: index.get(&thread.id).cloned(),
        next: private_mapping.clone(),
        snapshot_digest: digest(&snapshot_bytes),
    };
    atomic_write(&commit_path, &serde_json::to_vec_pretty(&commit)?)?;

    index.insert(thread.id.clone(), private_mapping);
    if crash == Some(ThreadCommitCrashPoint::AfterIndex) {
        atomic_write(
            &thread_index_path(home),
            &serde_json::to_vec_pretty(&index)?,
        )?;
        bail!("simulated crash after thread index commit");
    }
    if let Some(snapshot) = snapshot {
        atomic_write(&snapshot, &snapshot_bytes)?;
    }
    if crash == Some(ThreadCommitCrashPoint::AfterSnapshot) {
        bail!("simulated crash after thread snapshot commit");
    }
    atomic_write(
        &thread_index_path(home),
        &serde_json::to_vec_pretty(&index)?,
    )?;
    fs::remove_file(commit_path)?;
    Ok(())
}

fn state_lock(home: &Path) -> Result<File> {
    ensure_root(home)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state_root(home).join("state.lock"))?;
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    if result != 0 {
        bail!("lock Slack collaboration state");
    }
    Ok(file)
}
