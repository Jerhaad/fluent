use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{FluentThread, InboundEnvelope, OutboundDelivery, ProviderScope, RunnerConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxState {
    Pending,
    Processed,
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

/// Provider-neutral turn boundary with an explicit filesystem authority scope.
pub trait ConversationProvider {
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

    fn bounded_history(&mut self, _threads: &[(String, String)]) -> Result<Vec<InboundEnvelope>> {
        Ok(Vec::new())
    }
}
