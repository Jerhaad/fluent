use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub provider_turn_id: Option<String>,
    #[serde(default)]
    pub completed_reply: Option<String>,
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
