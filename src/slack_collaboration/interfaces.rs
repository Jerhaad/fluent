use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{FluentThread, InboundEnvelope, OutboundDelivery};

/// Host checkout configuration resolved by persistence and provider adapters.
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
    #[serde(default = "default_conversation_provider")]
    pub default_provider: String,
    #[serde(default = "default_enabled_conversation_providers")]
    pub enabled_providers: BTreeSet<String>,
    #[serde(default)]
    pub project_approvers: BTreeSet<String>,
}

fn default_conversation_provider() -> String {
    "codex".into()
}

fn default_enabled_conversation_providers() -> BTreeSet<String> {
    BTreeSet::from([default_conversation_provider()])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerConfig {
    pub workspace: String,
    pub projects: BTreeMap<String, ConfiguredProject>,
    pub channel_defaults: BTreeMap<String, String>,
    #[serde(default)]
    pub direct_message_defaults: BTreeMap<String, String>,
    pub enabled_providers: BTreeSet<String>,
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
                || !project.checkout.identity.starts_with("checkout-v1:")
                || project.checkout.branch.trim().is_empty()
                || project.checkout.commit.trim().is_empty()
            {
                bail!("project {key} has an incomplete checkout identity");
            }
            if project.default_provider.trim().is_empty()
                || !project
                    .enabled_providers
                    .contains(&project.default_provider)
                || project.enabled_providers.iter().any(|provider| {
                    provider.trim().is_empty() || !matches!(provider.as_str(), "codex" | "claude")
                })
            {
                bail!("project {key} has an invalid conversation provider policy");
            }
            if project.allowed_participants.is_empty()
                || !project
                    .project_approvers
                    .is_subset(&project.allowed_participants)
            {
                bail!("project {key} has incomplete participant or approver authority");
            }
            if !project
                .allowed_participants
                .iter()
                .chain(project.project_approvers.iter())
                .all(|identity| {
                    !identity.is_empty()
                        && identity.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
            {
                bail!("project {key} has an invalid Slack identity");
            }
        }
        if self.enabled_providers.is_empty() {
            bail!("at least one conversation provider is required");
        }
        for (channel, project) in &self.channel_defaults {
            if channel.trim().is_empty() || !self.projects.contains_key(project) {
                bail!("channel default references an unavailable configured project");
            }
        }
        for (conversation, project) in &self.direct_message_defaults {
            if conversation.trim().is_empty() || !self.projects.contains_key(project) {
                bail!("direct-message default references an unavailable configured project");
            }
        }
        Ok(())
    }
}

/// Filesystem authority resolved at the provider interface boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderScope {
    pub checkout: PathBuf,
    pub slice_draft: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxState {
    Pending,
    Processed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryTarget {
    pub channel: String,
    pub thread_timestamp: String,
    pub oldest: String,
}

/// Durable collaboration state boundary for inbox, aggregate snapshots,
/// outbox, and recovery cursors.
pub trait CollaborationStore {
    fn persist_inbox(&self, config: &RunnerConfig, event: &InboundEnvelope) -> Result<InboxState>;
    fn bind_inbox_event(&self, config: &RunnerConfig, id: &str, project_label: &str) -> Result<()>;
    fn mark_inbox_processed(&self, id: &str) -> Result<()>;
    fn load_pending_inbox(&self, config: &RunnerConfig) -> Result<Vec<InboundEnvelope>>;
    fn load_outbox(&self) -> Result<Vec<OutboundDelivery>>;
    fn save_outbox(&self, deliveries: &[OutboundDelivery]) -> Result<()>;
    fn persist_outbox(&self, delivery: OutboundDelivery) -> Result<()>;
    fn load_threads(&self, config: &RunnerConfig) -> Result<BTreeMap<String, FluentThread>>;
    fn load_thread(&self, config: &RunnerConfig, id: &str) -> Result<Option<FluentThread>>;
    fn save_thread(&self, config: &RunnerConfig, thread: &FluentThread) -> Result<()>;
    fn recovery_cursor(&self, channel: &str, thread: &str) -> Result<Option<String>>;
    fn save_recovery_cursor(&self, channel: &str, thread: &str, cursor: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerRequest {
    pub version: u32,
    pub operation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerStatus {
    pub reachable: bool,
}

pub trait SchedulerClient {
    fn status(&self, request: &SchedulerRequest) -> Result<SchedulerStatus>;
}

/// Exact host-side effect identity passed across the sandbox boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostActionKind {
    Enable { executable: PathBuf },
    Restart,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    pub action: HostActionKind,
    pub artifact_revision: String,
    pub actor: String,
    pub idempotency_identity: String,
}

pub trait HostActionExecutor {
    fn execute_host_action(&mut self, action: &HostAction) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    Message(String),
    Interrupted,
}

/// Structured result returned by a provider turn. Plain-text provider replies
/// remain compatible and are interpreted as messages by the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderOutput {
    Message { text: String },
    PlanningArtifact { kind: String, markdown: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderTurnRecovery {
    Missing,
    Accepted { turn_id: String },
    Completed { turn_id: String, reply: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFailure {
    Authentication,
    Unavailable,
    RateLimited { retry_after_seconds: u64 },
}

pub fn classify_provider_failure(message: &str) -> ProviderFailure {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("rate limit") || lowered.contains("429") {
        ProviderFailure::RateLimited {
            retry_after_seconds: 60,
        }
    } else if lowered.contains("auth")
        || lowered.contains("login")
        || lowered.contains("credential")
    {
        ProviderFailure::Authentication
    } else {
        ProviderFailure::Unavailable
    }
}

/// Provider-neutral turn boundary with an explicit filesystem authority scope.
pub trait ConversationProvider {
    /// Reports the provider implementation that owns persisted session ids.
    fn name(&self) -> &str {
        "codex"
    }
    fn start(
        &mut self,
        action_id: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)>;

    fn resume(
        &mut self,
        action_id: &str,
        session: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<String>;

    fn prepare_session(&mut self, _scope: &ProviderScope) -> Result<Option<String>> {
        Ok(None)
    }

    fn recover_turn(
        &mut self,
        _action_id: &str,
        _session: &str,
        _scope: &ProviderScope,
    ) -> Result<ProviderTurnRecovery> {
        Ok(ProviderTurnRecovery::Missing)
    }

    fn run_turn(
        &mut self,
        action_id: &str,
        session: Option<&str>,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
        accepted: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<(Option<String>, String)> {
        let (new_session, reply) = if let Some(session) = session {
            (
                None,
                self.resume(action_id, session, scope, contribution, actor)?,
            )
        } else {
            let (session, reply) = self.start(action_id, scope, contribution, actor)?;
            (Some(session), reply)
        };
        accepted(action_id)?;
        Ok((new_session, reply))
    }

    fn send(&mut self, _session: &str, _message: &str) -> Result<()> {
        bail!("conversation provider does not support direct messages")
    }

    fn interrupt(&mut self, _session: &str) -> Result<()> {
        bail!("conversation provider does not support interruption")
    }

    fn structured_events(&mut self, _session: &str) -> Result<Vec<ProviderEvent>> {
        Ok(Vec::new())
    }

    fn ready(&self) -> Result<()> {
        Ok(())
    }
}

/// Slack protocol boundary for live delivery and bounded recovery.
pub trait SlackTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()>;
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()>;

    fn update(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.post(delivery)
    }

    fn respond_private(&mut self, _channel: &str, _user: &str, _text: &str) -> Result<()> {
        bail!("Slack transport does not support private responses")
    }

    fn check(&self) -> Result<()> {
        Ok(())
    }

    fn receive(&mut self) -> Result<InboundEnvelope> {
        bail!("Slack transport does not receive Socket Mode envelopes")
    }

    fn reconnect(&mut self) -> Result<()> {
        bail!("Slack transport cannot reconnect")
    }

    fn bounded_history(&mut self, _threads: &[RecoveryTarget]) -> Result<Vec<InboundEnvelope>> {
        Ok(Vec::new())
    }
}
