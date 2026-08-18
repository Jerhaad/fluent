use super::*;

/// Coordinates one local runner. File state is written before external effects.
pub struct CollaborationRunner<'a, P, S> {
    pub(super) home: &'a Path,
    pub config: RunnerConfig,
    provider: P,
    slack: S,
    configuration_error: Option<String>,
}

impl<'a, P: ConversationProvider, S: SlackTransport> CollaborationRunner<'a, P, S> {
    pub fn new(home: &'a Path, config: RunnerConfig, provider: P, slack: S) -> Self {
        let configuration_error = config.validate().err().map(|error| error.to_string());
        Self {
            home,
            config,
            provider,
            slack,
            configuration_error,
        }
    }
    pub fn provider(&self) -> &P {
        &self.provider
    }
    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }
    pub fn slack(&self) -> &S {
        &self.slack
    }
    pub fn slack_mut(&mut self) -> &mut S {
        &mut self.slack
    }

    /// Persist first, then acknowledge exactly once, then process and send.
    pub fn ingest(&mut self, event: InboundEnvelope) -> Result<()> {
        if let Some(error) = &self.configuration_error {
            bail!("runner configuration is invalid; refusing Slack ingress: {error}");
        }
        match persist_inbox(self.home, &event)? {
            InboxState::Processed => return Ok(()),
            InboxState::Pending if !event.acknowledgement_id.is_empty() => {
                self.slack.acknowledge(&event.acknowledgement_id)?
            }
            InboxState::Pending => {}
        }
        self.process(event.clone())?;
        mark_inbox_processed(self.home, &event.id)?;
        self.deliver_pending()
    }
    pub fn reconnect(
        &mut self,
        history: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
    ) -> Result<()> {
        let mut events = load_pending_inbox(self.home)?;
        events.extend(history);
        events.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then(left.id.cmp(&right.id))
        });
        for event in events {
            self.ingest(event)?;
        }
        let thread_ids: Vec<_> = load_threads(self.home)?.into_keys().collect();
        for id in thread_ids {
            let Some(mut thread) = load_thread(self.home, &self.config, &id)? else {
                continue;
            };
            thread.last_connection = Some(connected_at.clone());
            save_thread(self.home, &self.config, &thread)?;
            persist_outbox(
                self.home,
                OutboundDelivery {
                    id: format!("{}:status", thread.id),
                    channel: thread.channel.clone(),
                    thread_timestamp: thread.id.clone(),
                    text: format!("Fluent thread status: runner last connected at {connected_at}."),
                    delivered: false,
                },
            )?;
        }
        self.deliver_pending()
    }

    pub fn recovery_targets(&self) -> Result<Vec<(String, String)>> {
        Ok(load_threads(self.home)?
            .into_values()
            .filter(|thread| thread.state == ThreadState::Active)
            .map(|thread| (thread.channel, thread.id))
            .collect())
    }
    pub fn deliver_pending(&mut self) -> Result<()> {
        let mut deliveries = load_outbox(self.home)?;
        for delivery in &mut deliveries {
            if !delivery.delivered {
                self.slack.post(delivery)?;
                delivery.delivered = true;
            }
        }
        save_outbox(self.home, &deliveries)
    }
    fn process(&mut self, event: InboundEnvelope) -> Result<()> {
        let mut thread = load_thread(self.home, &self.config, &event.thread_id())?
            .unwrap_or_else(|| FluentThread::new(&event));
        if thread
            .pending_turn
            .as_ref()
            .is_some_and(|turn| turn.action_id == event.id)
        {
            return self.complete_pending_turn(&event, &mut thread);
        }
        if !thread.processed_events.insert(event.id.clone()) {
            return Ok(());
        }
        if !policy::participant_can_contribute(&self.config, &thread, &event) {
            return save_thread(self.home, &self.config, &thread);
        }
        if thread.state == ThreadState::WaitingForProject && event.is_top_level_mention() {
            self.start_or_offer(&event, &mut thread)?;
        } else if thread.state == ThreadState::Active && event.thread_timestamp.is_some() {
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: false,
            });
            save_thread(self.home, &self.config, &thread)?;
            self.complete_pending_turn(&event, &mut thread)?;
        }
        save_thread(self.home, &self.config, &thread)
    }
    fn start_or_offer(&mut self, event: &InboundEnvelope, thread: &mut FluentThread) -> Result<()> {
        let default = self
            .config
            .channel_defaults
            .get(&event.channel)
            .and_then(|p| self.config.projects.get(p));
        let available = default.filter(|p| {
            p.allowed_participants.contains(&event.user) && checkout_is_available(&p.checkout)
        });
        if let Some(project) = available {
            thread.state = ThreadState::Active;
            thread.project_label = Some(project.label.clone());
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: true,
            });
            save_thread(self.home, &self.config, thread)?;
            self.complete_pending_turn(event, thread)?;
        } else {
            let projects =
                policy::available_projects(&self.config, &event.user, checkout_is_available);
            let reason = if default.is_some() {
                "The channel default is unavailable."
            } else {
                "Select a configured project before Fluent can access a checkout."
            };
            thread.pending_question = Some("project selection".into());
            self.queue_reply(
                event,
                &format!("{reason} Available projects: {}", projects.join(", ")),
            )?;
        }
        Ok(())
    }
    fn complete_pending_turn(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
    ) -> Result<()> {
        let turn = thread
            .pending_turn
            .clone()
            .context("thread has no pending provider turn")?;
        let checkout = self.bound_project(thread)?.checkout.path.clone();
        let scope = ProviderScope {
            slice_draft: checkout.join(".fluent/work/slack/drafts").join(&thread.id),
            checkout,
        };
        let reply = if turn.starts_session {
            let (session, reply) =
                self.provider
                    .start(&turn.action_id, &scope, &turn.contribution, &turn.actor)?;
            thread.provider_session = Some(session);
            format!(
                "Bound to {}.\n{}",
                safe_identity(self.bound_project(thread)?),
                reply
            )
        } else {
            let session = thread
                .provider_session
                .as_deref()
                .context("active thread lacks provider session")?;
            self.provider.resume(
                &turn.action_id,
                session,
                &scope,
                &turn.contribution,
                &turn.actor,
            )?
        };
        thread.pending_turn = None;
        self.queue_reply(event, &reply)?;
        save_thread(self.home, &self.config, thread)
    }
    fn bound_project(&self, thread: &FluentThread) -> Result<&ConfiguredProject> {
        let label = thread
            .project_label
            .as_deref()
            .context("thread is unbound")?;
        self.config
            .projects
            .get(label)
            .context("thread project no longer configured")
    }
    fn queue_reply(&self, event: &InboundEnvelope, text: &str) -> Result<()> {
        let delivery = OutboundDelivery {
            id: format!("{}:reply", event.id),
            channel: event.channel.clone(),
            thread_timestamp: event.thread_id(),
            text: text.into(),
            delivered: false,
        };
        persist_outbox(self.home, delivery)
    }
}
