use anyhow::{Result, bail};

use super::{InboundEnvelope, OutboundDelivery, ProviderScope};

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

    fn ready(&self) -> Result<()> {
        Ok(())
    }
}

/// Slack protocol boundary for live delivery and bounded recovery.
pub trait SlackTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()>;
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()>;

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
