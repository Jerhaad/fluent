use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

use super::{
    CollaborationStore, FluentThread, InboundEnvelope, InboxState, OutboundDelivery, RunnerConfig,
    validate_thread_identity,
};

#[derive(Debug, Clone)]
pub struct FileCollaborationStore {
    home: PathBuf,
}

impl FileCollaborationStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
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

fn cursor_path(home: &Path) -> PathBuf {
    state_root(home).join("cursors.json")
}

fn thread_path(project: &Path, id: &str) -> Result<PathBuf> {
    validate_thread_identity(id)?;
    Ok(project
        .join(".fluent/work/slack/threads")
        .join(format!("{id}.json")))
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
}

pub(super) fn persist_inbox(home: &Path, event: &InboundEnvelope) -> Result<InboxState> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    if let Some(entry) = inbox.get(&event.id) {
        return Ok(if entry.processed {
            InboxState::Processed
        } else {
            InboxState::Pending
        });
    }
    if inbox.values().any(|entry| {
        entry.processed
            && entry.event.channel == event.channel
            && entry.event.timestamp == event.timestamp
            && entry.event.user == event.user
            && entry.event.text == event.text
    }) {
        return Ok(InboxState::Processed);
    }
    inbox.insert(
        event.id.clone(),
        InboxEntry {
            event: event.clone(),
            processed: false,
        },
    );
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(InboxState::Pending)
}

impl CollaborationStore for FileCollaborationStore {
    fn persist_inbox(&self, event: &InboundEnvelope) -> Result<InboxState> {
        persist_inbox(&self.home, event)
    }

    fn mark_inbox_processed(&self, id: &str) -> Result<()> {
        mark_inbox_processed(&self.home, id)
    }

    fn load_pending_inbox(&self) -> Result<Vec<InboundEnvelope>> {
        load_pending_inbox(&self.home)
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

    fn load_threads(&self) -> Result<BTreeMap<String, FluentThread>> {
        load_threads(&self.home)
    }

    fn load_thread(&self, config: &RunnerConfig, id: &str) -> Result<Option<FluentThread>> {
        load_thread(&self.home, config, id)
    }

    fn save_thread(&self, config: &RunnerConfig, thread: &FluentThread) -> Result<()> {
        save_thread(&self.home, config, thread)
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

pub(super) fn load_pending_inbox(home: &Path) -> Result<Vec<InboundEnvelope>> {
    let inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let mut pending: Vec<_> = inbox
        .into_values()
        .filter(|entry| !entry.processed)
        .map(|entry| entry.event)
        .collect();
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
    if let Some(existing) = outbox.iter_mut().find(|item| item.id == delivery.id) {
        if delivery.id.ends_with(":status") && existing.text != delivery.text {
            *existing = delivery;
            atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(&outbox)?)?;
        }
    } else {
        outbox.push(delivery);
        atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(&outbox)?)?;
    }
    Ok(())
}

pub(super) fn load_threads(home: &Path) -> Result<BTreeMap<String, FluentThread>> {
    read_json(thread_index_path(home))
}

pub(super) fn load_thread(
    home: &Path,
    config: &RunnerConfig,
    id: &str,
) -> Result<Option<FluentThread>> {
    let index = load_threads(home)?;
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

pub(super) fn save_thread(home: &Path, config: &RunnerConfig, thread: &FluentThread) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut index = load_threads(home)?;
    let mut private_mapping = thread.clone();
    if private_mapping.project_label.is_some() {
        private_mapping.provider_session = None;
        private_mapping.slice = None;
        private_mapping.planning_stage = None;
        private_mapping.linked_work = None;
        private_mapping.pending_question = None;
        private_mapping.processed_events.clear();
    }
    if let Some(project) = thread
        .project_label
        .as_ref()
        .and_then(|label| config.projects.get(label))
    {
        let path = thread_path(&project.checkout.path, &thread.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &serde_json::to_vec_pretty(thread)?)?;
    }
    index.insert(thread.id.clone(), private_mapping);
    atomic_write(
        &thread_index_path(home),
        &serde_json::to_vec_pretty(&index)?,
    )?;
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
