use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub label: String,
    pub path: PathBuf,
    pub identity: String,
    pub branch: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredProject {
    pub label: String,
    pub checkout: Checkout,
    pub allowed_participants: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerConfig {
    pub workspace: String,
    pub projects: BTreeMap<String, ConfiguredProject>,
    pub channel_defaults: BTreeMap<String, String>,
}

impl RunnerConfig {
    /// Reject incomplete policy rather than accepting with partial authority.
    pub fn validate(&self) -> Result<()> {
        if self.workspace.trim().is_empty() {
            bail!("Slack workspace is required");
        }
        for (key, project) in &self.projects {
            if key != &project.label || key.trim().is_empty() {
                bail!("configured project labels must be non-empty map keys");
            }
            if project.checkout.label.trim().is_empty()
                || project.checkout.identity.trim().is_empty()
                || project.checkout.branch.trim().is_empty()
                || project.checkout.commit.trim().is_empty()
            {
                bail!("project {key} has an incomplete checkout identity");
            }
        }
        for (channel, project) in &self.channel_defaults {
            if channel.trim().is_empty() || !self.projects.contains_key(project) {
                bail!("channel default references an unavailable configured project");
            }
        }
        Ok(())
    }
}

/// A persisted, idempotent Slack event. `id` is Slack's event id, never a
/// generated retry id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundEnvelope {
    pub id: String,
    #[serde(default)]
    pub acknowledgement_id: String,
    pub channel: String,
    pub timestamp: String,
    pub thread_timestamp: Option<String>,
    pub user: String,
    pub text: String,
}

impl InboundEnvelope {
    pub fn thread_id(&self) -> String {
        self.thread_timestamp
            .clone()
            .unwrap_or_else(|| self.timestamp.clone())
    }

    pub fn is_top_level_mention(&self) -> bool {
        self.thread_timestamp.is_none() && self.text.contains("@Fluent")
    }

    pub fn validate_thread_identity(&self) -> Result<()> {
        validate_thread_identity(&self.thread_id())
    }
}

pub(super) fn validate_thread_identity(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        bail!("Slack thread identity is not a canonical timestamp")
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundDelivery {
    pub id: String,
    pub channel: String,
    pub thread_timestamp: String,
    pub text: String,
    pub delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadState {
    WaitingForProject,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluentThread {
    pub id: String,
    pub channel: String,
    pub state: ThreadState,
    pub project_label: Option<String>,
    pub provider_session: Option<String>,
    pub slice: Option<String>,
    pub planning_stage: Option<String>,
    pub linked_work: Option<String>,
    pub pending_question: Option<String>,
    pub processed_events: BTreeSet<String>,
    pub last_connection: Option<String>,
    #[serde(default)]
    pub pending_turn: Option<PendingTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTurn {
    pub action_id: String,
    pub contribution: String,
    pub actor: String,
    pub starts_session: bool,
}

impl FluentThread {
    pub(super) fn new(event: &InboundEnvelope) -> Self {
        Self {
            id: event.thread_id(),
            channel: event.channel.clone(),
            state: ThreadState::WaitingForProject,
            project_label: None,
            provider_session: None,
            slice: None,
            planning_stage: None,
            linked_work: None,
            pending_question: None,
            processed_events: BTreeSet::new(),
            last_connection: None,
            pending_turn: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderScope {
    pub checkout: PathBuf,
    pub slice_draft: PathBuf,
}
