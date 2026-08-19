use super::*;

/// Coordinates one local runner. File state is written before external effects.
pub struct CollaborationRunner<'a, P, S, C = FileCollaborationStore> {
    pub(super) home: &'a Path,
    pub config: RunnerConfig,
    provider: P,
    slack: S,
    store: C,
    recovering_event: bool,
}

impl<'a, P: ConversationProvider, S: SlackTransport>
    CollaborationRunner<'a, P, S, FileCollaborationStore>
{
    pub fn new(home: &'a Path, config: RunnerConfig, provider: P, slack: S) -> Self {
        Self::with_store(
            home,
            config,
            provider,
            slack,
            FileCollaborationStore::new(home),
        )
    }
}

impl<'a, P: ConversationProvider, S: SlackTransport, C: CollaborationStore>
    CollaborationRunner<'a, P, S, C>
{
    pub fn with_store(
        home: &'a Path,
        config: RunnerConfig,
        provider: P,
        slack: S,
        store: C,
    ) -> Self {
        Self {
            home,
            config,
            provider,
            slack,
            store,
            recovering_event: false,
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
        event.validate_thread_identity()?;
        self.config = self.current_authority_config()?;
        match self.store.persist_inbox(&self.config, &event)? {
            InboxState::Processed => return Ok(()),
            InboxState::Pending if !event.acknowledgement_id.is_empty() => {
                self.slack.acknowledge(&event.acknowledgement_id)?
            }
            InboxState::Pending => {}
        }
        self.process(event.clone())?;
        self.store.mark_inbox_processed(&event.id)?;
        self.deliver_pending()
    }
    pub fn reconnect(
        &mut self,
        history: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
    ) -> Result<()> {
        self.restore_connection(history, connected_at, true)
    }

    pub(super) fn restore_connection(
        &mut self,
        history: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
        report_interruption: bool,
    ) -> Result<()> {
        let mut events: Vec<_> = self
            .store
            .load_pending_inbox(&self.config)?
            .into_iter()
            .map(|event| (true, event))
            .collect();
        events.extend(history.into_iter().map(|event| (true, event)));
        events.sort_by(|(left_history, left), (right_history, right)| {
            left.timestamp
                .cmp(&right.timestamp)
                .then(left_history.cmp(right_history).then(left.id.cmp(&right.id)))
        });
        for (recovered, event) in events {
            self.recovering_event = recovered;
            self.ingest(event.clone())?;
            self.recovering_event = false;
            if recovered {
                self.store.save_recovery_cursor(
                    &event.channel,
                    &event.thread_id(),
                    &event.timestamp,
                )?;
            }
        }
        let thread_ids: Vec<_> = self.store.load_threads(&self.config)?.into_keys().collect();
        for id in thread_ids {
            let Some(mut thread) = self.store.load_thread(&self.config, &id)? else {
                continue;
            };
            thread.last_connection = Some(connected_at.clone());
            self.store.save_thread(&self.config, &thread)?;
            self.queue_thread_status(
                &thread,
                report_interruption.then_some("Slack connection was interrupted."),
            )?;
        }
        self.deliver_pending()
    }

    pub fn recovery_targets(&self) -> Result<Vec<RecoveryTarget>> {
        self.store
            .load_threads(&self.config)?
            .into_values()
            .filter(|thread| thread.state == ThreadState::Active)
            .map(|thread| {
                let oldest = self
                    .store
                    .recovery_cursor(&thread.channel, &thread.id)?
                    .unwrap_or_else(|| thread.id.clone());
                Ok(RecoveryTarget {
                    channel: thread.channel,
                    thread_timestamp: thread.id,
                    oldest,
                })
            })
            .collect()
    }
    pub fn deliver_pending(&mut self) -> Result<()> {
        let mut deliveries = self.store.load_outbox()?;
        for index in 0..deliveries.len() {
            if !deliveries[index].delivered {
                self.slack.post(&deliveries[index])?;
                deliveries[index].delivered = true;
                self.store.save_outbox(&deliveries)?;
            }
        }
        Ok(())
    }
    fn process(&mut self, event: InboundEnvelope) -> Result<()> {
        let existing = self.store.load_thread(&self.config, &event.thread_id())?;
        if existing.is_none()
            && event.is_top_level_mention()
            && !policy::participant_can_contribute(
                &self.current_authority_config()?,
                &FluentThread::new(&event),
                &event,
            )
        {
            return self.slack.respond_private(
                &event.channel,
                &event.user,
                "You are not allowed to start Fluent work.",
            );
        }
        let mut thread = existing.unwrap_or_else(|| FluentThread::new(&event));
        if let Some(project_label) = &thread.project_label {
            self.store
                .bind_inbox_event(&self.config, &event.id, project_label)?;
        }
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
        if !policy::participant_can_contribute(&self.current_authority_config()?, &thread, &event) {
            return self.store.save_thread(&self.config, &thread);
        }
        if thread.state == ThreadState::Active
            && thread.pending_action.is_some()
            && policy::is_approval_response(&thread, &event)
        {
            match self.approve_thread_action(&mut thread, &event)? {
                ApprovalOutcome::WaitingForApprover => self.slack.respond_private(
                    &event.channel,
                    &event.user,
                    "Only a project approver may authorize this action.",
                )?,
                ApprovalOutcome::FreshConfirmationRequired => self.queue_reply(
                    &event,
                    "Fresh confirmation is required before Fluent can perform this action.",
                )?,
                ApprovalOutcome::Stale { current_action_id } => self.queue_reply(
                    &event,
                    &format!(
                        "That approval is stale. Current approval request: {current_action_id}."
                    ),
                )?,
                _ => {}
            }
            return self.store.save_thread(&self.config, &thread);
        }
        if thread.state == ThreadState::WaitingForProject && event.is_top_level_mention() {
            self.start_or_offer(&event, &mut thread)?;
        } else if thread.state == ThreadState::Active && event.thread_timestamp.is_some() {
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: false,
                provider_turn_id: None,
                completed_reply: None,
            });
            self.store.save_thread(&self.config, &thread)?;
            self.complete_pending_turn(&event, &mut thread)?;
        }
        self.store.save_thread(&self.config, &thread)
    }

    /// Present exactly one revision-bound lifecycle action for approval.
    pub fn request_approval(&mut self, thread_id: &str, action: PendingAction) -> Result<()> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        if thread.state != ThreadState::Active
            || action.id.is_empty()
            || action.artifact_revision.is_empty()
        {
            bail!("pending action identity is incomplete");
        }
        thread.pending_question = Some(action.id.clone());
        let question = format!(
            "Waiting for a project approver to authorize {} at revision {}.",
            action.id, action.artifact_revision
        );
        thread.pending_action = Some(action);
        self.store.save_thread(&self.config, &thread)?;
        self.queue_thread_waiting(&thread, &question)?;
        self.deliver_pending()
    }

    /// Evaluate a direct approval attempt against current durable authority.
    pub fn approve_pending_action(
        &mut self,
        thread_id: &str,
        actor: &str,
        action_id: &str,
        artifact_revision: &str,
        event_id: &str,
        accepted_at: &str,
    ) -> Result<ApprovalOutcome> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        if let Some(record) = thread
            .approvals
            .iter()
            .find(|record| record.event_id == event_id)
        {
            return Ok(ApprovalOutcome::Accepted(record.clone()));
        }
        let Some(action) = thread.pending_action.as_ref() else {
            return Ok(ApprovalOutcome::NoPendingAction);
        };
        if action.id != action_id || action.artifact_revision != artifact_revision {
            return Ok(ApprovalOutcome::Stale {
                current_action_id: action.id.clone(),
            });
        }
        let authority = policy::project_authority(&self.current_authority_config()?, &thread);
        let outcome = policy::evaluate_approval(
            &mut thread,
            authority.as_ref(),
            actor,
            event_id,
            accepted_at,
            false,
            None,
        );
        self.store.save_thread(&self.config, &thread)?;
        Ok(outcome)
    }

    fn approve_thread_action(
        &mut self,
        thread: &mut FluentThread,
        event: &InboundEnvelope,
    ) -> Result<ApprovalOutcome> {
        let accepted_at = chrono::Utc::now().to_rfc3339();
        let authority = policy::project_authority(&self.current_authority_config()?, thread);
        Ok(policy::evaluate_approval(
            thread,
            authority.as_ref(),
            &event.user,
            &event.id,
            &accepted_at,
            self.recovering_event,
            Some(event),
        ))
    }

    fn current_authority_config(&self) -> Result<RunnerConfig> {
        let path = config_path(self.home);
        let config = if path.exists() {
            read_config(self.home)
        } else {
            Ok(self.config.clone())
        }?;
        config.validate()?;
        Ok(config)
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
            self.store
                .bind_inbox_event(&self.config, &event.id, &project.label)?;
            thread.state = ThreadState::Active;
            thread.project_label = Some(project.label.clone());
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: true,
                provider_turn_id: None,
                completed_reply: None,
            });
            self.store.save_thread(&self.config, thread)?;
            if let Some(connected_at) =
                read_runner_operational_state(self.home)?.last_successful_connection
            {
                thread.last_connection = Some(connected_at);
                self.store.save_thread(&self.config, thread)?;
                self.queue_thread_status(thread, None)?;
            }
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
        let mut turn = thread
            .pending_turn
            .clone()
            .context("thread has no pending provider turn")?;
        let previously_completed = turn.completed_reply.is_some();
        let checkout = self.bound_project(thread)?.checkout.path.clone();
        let scope = ProviderScope {
            slice_draft: checkout.join(".fluent/work/slack/drafts").join(&thread.id),
            checkout,
        };
        if turn.starts_session && thread.provider_session.is_none() {
            if let Some(session) = self.provider.prepare_session(&scope)? {
                thread.provider_session = Some(session);
                self.store.save_thread(&self.config, thread)?;
            }
        }
        if turn.completed_reply.is_none() {
            if let Some(session) = thread.provider_session.as_deref() {
                match self
                    .provider
                    .recover_turn(&turn.action_id, session, &scope)?
                {
                    ProviderTurnRecovery::Missing if turn.provider_turn_id.is_some() => {
                        bail!("accepted provider turn is missing from provider history");
                    }
                    ProviderTurnRecovery::Missing => {}
                    ProviderTurnRecovery::Accepted { turn_id } => {
                        turn.provider_turn_id = Some(turn_id);
                        thread.pending_turn = Some(turn);
                        self.store.save_thread(&self.config, thread)?;
                        bail!("provider turn is accepted but not complete");
                    }
                    ProviderTurnRecovery::Completed { turn_id, reply } => {
                        turn.provider_turn_id = Some(turn_id);
                        turn.completed_reply = Some(redact_secrets(&reply));
                    }
                }
            }
        }
        if turn.completed_reply.is_none() {
            let store = &self.store;
            let config = &self.config;
            let checkpoint = std::cell::RefCell::new(thread.clone());
            let mut accepted = |turn_id: &str| {
                let mut checkpoint = checkpoint.borrow_mut();
                let current = checkpoint
                    .pending_turn
                    .as_mut()
                    .context("thread lost its pending provider turn")?;
                current.provider_turn_id = Some(turn_id.to_owned());
                store.save_thread(config, &checkpoint)
            };
            let (session, reply) = self.provider.run_turn(
                &turn.action_id,
                thread.provider_session.as_deref(),
                &scope,
                &turn.contribution,
                &turn.actor,
                &mut accepted,
            )?;
            *thread = checkpoint.into_inner();
            if let Some(session) = session {
                thread.provider_session = Some(session);
            }
            turn = thread.pending_turn.clone().unwrap_or(turn);
            turn.completed_reply = Some(redact_secrets(&reply));
        }
        let raw_reply = turn
            .completed_reply
            .clone()
            .context("provider turn completed without a reply")?;
        let raw_reply = redact_secrets(&raw_reply);
        let reply = if previously_completed {
            raw_reply
        } else if turn.starts_session {
            format!(
                "Bound to {}.\n{}",
                safe_identity(self.bound_project(thread)?),
                raw_reply
            )
        } else {
            raw_reply
        };
        turn.completed_reply = Some(reply.clone());
        thread.pending_turn = Some(turn);
        self.store.save_thread(&self.config, thread)?;
        self.queue_reply(event, &reply)?;
        thread.pending_turn = None;
        self.store.save_thread(&self.config, thread)
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
            text: redact_secrets(text),
            delivered: false,
            approval_control: None,
        };
        self.store.persist_outbox(delivery)
    }

    fn queue_thread_waiting(&self, thread: &FluentThread, text: &str) -> Result<()> {
        let action = thread
            .pending_action
            .as_ref()
            .context("waiting thread lacks a pending action")?;
        self.store.persist_outbox(OutboundDelivery {
            id: format!(
                "{}:approval-waiting:{}:{}",
                thread.id, action.id, action.artifact_revision
            ),
            channel: thread.channel.clone(),
            thread_timestamp: thread.id.clone(),
            text: text.into(),
            delivered: false,
            approval_control: Some(ApprovalControl {
                action_id: action.id.clone(),
                artifact_revision: action.artifact_revision.clone(),
                label: "Approve".into(),
            }),
        })
    }

    fn queue_thread_status(&self, thread: &FluentThread, interruption: Option<&str>) -> Result<()> {
        let connected_at = thread
            .last_connection
            .as_deref()
            .context("active thread lacks a Slack connection time")?;
        let interruption = interruption
            .map(|message| format!(" {message}"))
            .unwrap_or_default();
        self.store.persist_outbox(OutboundDelivery {
            id: format!("{}:status:{connected_at}", thread.id),
            channel: thread.channel.clone(),
            thread_timestamp: thread.id.clone(),
            text: format!(
                "Fluent thread status:{interruption} runner last connected at {connected_at}."
            ),
            delivered: false,
            approval_control: None,
        })
    }
}
