use super::*;
use crate::work_model::WorkModelStore;
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

/// Fair, bounded admission for provider turns. A thread may hold at most one
/// slot, and waiting threads are admitted in first-ready order.
#[derive(Debug)]
pub struct FairTurnPool {
    capacity: usize,
    active: BTreeSet<String>,
    waiting: VecDeque<String>,
}

impl FairTurnPool {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            active: BTreeSet::new(),
            waiting: VecDeque::new(),
        }
    }

    pub fn request(&mut self, thread: impl Into<String>) -> bool {
        let thread = thread.into();
        if self.active.contains(&thread) || self.waiting.contains(&thread) {
            return false;
        }
        if self.active.len() < self.capacity {
            self.active.insert(thread);
            true
        } else {
            self.waiting.push_back(thread);
            false
        }
    }

    pub fn finish(&mut self, thread: &str) -> Option<String> {
        self.active.remove(thread);
        if let Some(next) = self.waiting.pop_front() {
            self.active.insert(next.clone());
            Some(next)
        } else {
            None
        }
    }
}

/// Preserve Slack ordering and attribution when several replies form one turn.
pub fn coalesce_contributions(replies: impl IntoIterator<Item = (String, String)>) -> String {
    replies
        .into_iter()
        .map(|(actor, text)| format!("Slack user {actor}: {text}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Coordinates one local runner. File state is written before external effects.
pub struct CollaborationRunner<'a, P, S, C = FileCollaborationStore> {
    pub(super) home: &'a Path,
    pub config: RunnerConfig,
    provider: P,
    slack: S,
    store: C,
    recovering_event: bool,
    configuration_error: Option<String>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    turn_pool: FairTurnPool,
}

impl<'a, P: ConversationProvider, S: SlackTransport>
    CollaborationRunner<'a, P, S, FileCollaborationStore>
{
    pub fn new(home: &'a Path, config: RunnerConfig, provider: P, slack: S) -> Self {
        Self::new_with_turn_capacity(home, config, provider, slack, 4)
    }

    pub fn new_with_turn_capacity(
        home: &'a Path,
        config: RunnerConfig,
        provider: P,
        slack: S,
        capacity: usize,
    ) -> Self {
        Self::with_store_and_turn_capacity(
            home,
            config,
            provider,
            slack,
            FileCollaborationStore::new(home),
            capacity,
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
        Self::with_store_and_turn_capacity(home, config, provider, slack, store, 4)
    }

    pub fn with_store_and_turn_capacity(
        home: &'a Path,
        config: RunnerConfig,
        provider: P,
        slack: S,
        store: C,
        capacity: usize,
    ) -> Self {
        let configuration_error = config.validate().err().map(|error| error.to_string());
        Self {
            home,
            config,
            provider,
            slack,
            store,
            recovering_event: false,
            configuration_error,
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            }),
            turn_pool: FairTurnPool::new(capacity),
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

    /// Store and present an immutable planning revision before it can advance.
    pub fn present_planning_artifact(
        &mut self,
        thread_id: &str,
        kind: &str,
        markdown: &str,
    ) -> Result<ArtifactRevision> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        self.present_planning_artifact_for_thread(&mut thread, kind, markdown)
    }

    fn present_planning_artifact_for_thread(
        &mut self,
        thread: &mut FluentThread,
        kind: &str,
        markdown: &str,
    ) -> Result<ArtifactRevision> {
        if let Some(work_item_id) = thread.linked_work.as_deref() {
            WorkModelStore::new(&self.bound_project(&thread)?.checkout.path)
                .read_work_item(work_item_id)?
                .ensure_planning_revision_can_advance(kind)?;
        }
        let revision = ArtifactRevision::new(kind, markdown);
        if !thread
            .artifact_revisions
            .iter()
            .any(|existing| existing.id == revision.id)
        {
            thread.artifact_revisions.push(revision.clone());
        }
        thread.pending_artifact_revision = Some(revision.id.clone());
        thread.planning_stage = Some(revision.kind.clone());
        thread.pending_question = Some(format!(
            "approve {} revision {}",
            revision.kind, revision.id
        ));
        self.store.save_thread(&self.config, &thread)?;
        let parts = Self::artifact_delivery_parts(&revision);
        let part_count = parts.len();
        let part_width = part_count.to_string().len();
        for (index, part) in parts.into_iter().enumerate() {
            self.store.persist_outbox(OutboundDelivery {
                id: format!(
                    "{}:artifact:{}:{:0part_width$}",
                    thread.id, revision.id, index
                ),
                channel: thread.channel.clone(),
                thread_timestamp: thread.id.clone(),
                text: format!(
                    "Planning revision {} ({}/{part_count})\n\n{part}",
                    revision.id,
                    index + 1
                ),
                delivered: false,
                approval_control: None,
            })?;
        }
        self.deliver_pending()?;
        Ok(revision)
    }

    /// Advance only the exact revision currently waiting for approval.
    pub fn approve_planning_revision(
        &mut self,
        thread_id: &str,
        revision_id: &str,
        actor: &str,
    ) -> Result<()> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        if !self
            .bound_project(&thread)?
            .project_approvers
            .contains(actor)
        {
            bail!("participant is not a project approver for this Fluent thread");
        }
        if thread.pending_artifact_revision.as_deref() != Some(revision_id) {
            bail!("approval must identify the revision waiting at the planning gate");
        }
        let revision = thread
            .artifact_revisions
            .iter()
            .find(|revision| revision.id == revision_id)
            .context("pending planning revision is unavailable")?;
        if thread.planning_stage.as_deref() != Some(revision.kind.as_str())
            || thread.pending_question.as_deref()
                != Some(&format!("approve {} revision {revision_id}", revision.kind))
        {
            bail!("planning lifecycle is not waiting for this artifact revision");
        }
        self.approve_work_item_planning_revision(&thread, revision)?;
        thread.pending_artifact_revision = None;
        thread.pending_question = None;
        self.store.save_thread(&self.config, &thread)
    }

    fn approve_work_item_planning_revision(
        &self,
        thread: &FluentThread,
        revision: &ArtifactRevision,
    ) -> Result<()> {
        let work_item_id = thread
            .linked_work
            .as_deref()
            .context("planning artifact is not bound to a Fluent Work Item")?;
        let checkout = &self.bound_project(thread)?.checkout.path;
        WorkModelStore::new(checkout)
            .mutate_work_item(work_item_id, |work_item| {
                work_item.approve_planning_revision(
                    &revision.kind,
                    &revision.id,
                    revision.markdown.clone(),
                )
            })
            .with_context(|| format!("approve planning Work Item {work_item_id}"))
    }

    #[doc(hidden)]
    pub fn set_clock_for_test(&mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) {
        self.clock = Arc::new(clock);
    }

    /// Re-admit every durable provider turn whose retry deadline has passed.
    pub fn retry_due_turns(&mut self) -> Result<()> {
        let now = (self.clock)();
        let thread_ids: Vec<_> = self.store.load_threads(&self.config)?.into_keys().collect();
        for thread_id in thread_ids {
            let Some(mut thread) = self.store.load_thread(&self.config, &thread_id)? else {
                continue;
            };
            let due = thread
                .pending_turn
                .as_ref()
                .and_then(|turn| turn.retry_at_unix_seconds)
                .is_some_and(|deadline| deadline <= now);
            if !due {
                continue;
            }
            if let Some(turn) = thread.pending_turn.as_mut() {
                turn.retry_at_unix_seconds = None;
            }
            self.store.save_thread(&self.config, &thread)?;
            let event = InboundEnvelope {
                id: format!("{}:provider-retry:{now}", thread.id),
                acknowledgement_id: String::new(),
                channel: thread.channel.clone(),
                timestamp: thread.id.clone(),
                thread_timestamp: Some(thread.id.clone()),
                user: thread
                    .pending_turn
                    .as_ref()
                    .map(|turn| turn.actor.clone())
                    .unwrap_or_default(),
                text: String::new(),
                change: InboundChange::Message,
                approval_action_id: None,
                approval_artifact_revision: None,
            };
            self.complete_pending_turn(&event, &mut thread)?;
        }
        self.deliver_pending()
    }

    /// Switch an interrupted conversation and run its preserved turn through
    /// the selected provider. Scheduler Work is not represented or mutated at
    /// this boundary.
    pub fn switch_provider(&mut self, thread_id: &str, actor: &str, provider: &str) -> Result<()> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        let project = self.bound_project(&thread)?;
        if !project.project_approvers.contains(actor) {
            bail!("only a project approver may switch providers");
        }
        if thread.pending_turn.is_none() {
            bail!("only an interrupted Fluent thread may switch providers");
        }
        if !project.enabled_providers.contains(provider) {
            bail!("conversation provider is not enabled");
        }
        thread.provider = Some(provider.into());
        thread.provider_session = None;
        self.store.save_thread(&self.config, &thread)?;
        self.store.persist_outbox(OutboundDelivery {
            id: format!("{}:provider-switch:{}", thread.id, provider), channel: thread.channel.clone(),
            thread_timestamp: thread.id.clone(), text: format!("Provider switched to {provider}; Fluent will resume from the durable Slice conversation."), delivered: false,
            approval_control: None,
        })?;
        let event = InboundEnvelope {
            id: format!("{}:provider-switch:{}", thread.id, provider),
            acknowledgement_id: String::new(),
            channel: thread.channel.clone(),
            timestamp: thread.id.clone(),
            thread_timestamp: Some(thread.id.clone()),
            user: actor.into(),
            text: format!("[provider={provider}]"),
            change: InboundChange::Message,
            approval_action_id: None,
            approval_artifact_revision: None,
        };
        self.complete_pending_turn(&event, &mut thread)?;
        self.store.save_thread(&self.config, &thread)
    }

    /// Persist first, then acknowledge exactly once, then process and send.
    pub fn ingest(&mut self, event: InboundEnvelope) -> Result<()> {
        event.validate_thread_identity()?;
        self.config = self.current_authority_config()?;
        if let Some(thread) = self.store.load_thread(&self.config, &event.thread_id())?
            && thread.state == ThreadState::Active
            && !policy::participant_can_contribute(&self.config, &thread, &event)
        {
            if !event.acknowledgement_id.is_empty() {
                self.slack.acknowledge(&event.acknowledgement_id)?;
            }
            self.slack.respond_private(
                &event.channel,
                &event.user,
                "You cannot participate in this Fluent thread.",
            )?;
            return Ok(());
        }
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

    fn artifact_delivery_parts(revision: &ArtifactRevision) -> Vec<String> {
        let mut part_count = 1;
        loop {
            let header = format!(
                "Planning revision {} ({part_count}/{part_count})\n\n",
                revision.id
            );
            let payload_limit = SLACK_MESSAGE_CONTENT_LIMIT
                .checked_sub(header.len())
                .expect("artifact revision header must fit in Slack delivery");
            let parts = split_artifact_markdown(&revision.markdown, payload_limit);
            if parts.len() == part_count {
                return parts;
            }
            part_count = parts.len();
        }
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
        if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && let Some(provider) = selected_provider(&event.text)
        {
            return self.switch_provider(&thread.id, &event.user, provider);
        }
        if thread
            .pending_turn
            .as_ref()
            .is_some_and(|turn| turn.action_id == event.id)
        {
            return self.complete_pending_turn(&event, &mut thread);
        }
        if !policy::participant_can_contribute(&self.config, &thread, &event) {
            self.slack.respond_private(
                &event.channel,
                &event.user,
                "You cannot participate in this Fluent thread.",
            )?;
            return Ok(());
        }
        if !thread.processed_events.insert(event.id.clone()) {
            return Ok(());
        }
        if !policy::participant_can_contribute(&self.config, &thread, &event) {
            self.slack.respond_private(
                &event.channel,
                &event.user,
                "You cannot participate in this Fluent thread.",
            )?;
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
        match event.change {
            InboundChange::Edit => return self.apply_edit(event, thread),
            InboundChange::Delete => return self.apply_deletion(event, thread),
            InboundChange::Message => {}
        }
        if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && self.approve_revision_if_requested(&event, &mut thread)?
        {
        } else if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && self.offer_linked_thread_if_requested(&event, &mut thread)?
        {
        } else if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && thread.pending_artifact_revision.is_some()
        {
            self.queue_reply(
                &event,
                "Planning is waiting for approval of the presented revision.",
            )?;
        } else if thread.state == ThreadState::WaitingForProject && event.is_top_level_mention() {
            self.start_or_offer(&event, &mut thread)?;
        } else if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && thread.pending_turn.is_some()
        {
            thread.queued_contributions.push(QueuedContribution {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
            });
            self.store.save_thread(&self.config, &thread)?;
            self.queue_reply(&event, "Reply queued for the next conversation turn.")?;
        } else if thread.state == ThreadState::Active && event.thread_timestamp.is_some() {
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                source_timestamp: event.timestamp.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: false,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
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

    fn apply_edit(&mut self, event: InboundEnvelope, mut thread: FluentThread) -> Result<()> {
        if let Some(turn) = thread
            .pending_turn
            .as_mut()
            .filter(|turn| turn.source_timestamp == event.timestamp)
        {
            turn.contribution = event.text;
            return self.store.save_thread(&self.config, &thread);
        }
        if let Some(input) = thread
            .conversation_inputs
            .iter_mut()
            .find(|input| input.source_timestamp == event.timestamp)
        {
            input.corrections.push(AttributedCorrection {
                event_id: event.id,
                actor: event.user,
                content: event.text,
            });
        }
        self.store.save_thread(&self.config, &thread)
    }

    fn apply_deletion(&mut self, event: InboundEnvelope, mut thread: FluentThread) -> Result<()> {
        if let Some(input) = thread
            .conversation_inputs
            .iter_mut()
            .find(|input| input.source_timestamp == event.timestamp)
        {
            input.source_deleted = true;
        }
        self.store.save_thread(&self.config, &thread)
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
        let default = policy::default_project(&self.config, &event.channel)
            .and_then(|project| self.config.projects.get(project));
        let available = default.filter(|p| {
            p.allowed_participants.contains(&event.user) && checkout_is_available(&p.checkout)
        });
        if let Some(project) = available {
            let provider = selected_provider(&event.text).unwrap_or(&project.default_provider);
            if !project.enabled_providers.contains(provider) {
                self.queue_reply(
                    event,
                    "The selected conversation provider is not enabled for this project.",
                )?;
                return Ok(());
            }
            self.store
                .bind_inbox_event(&self.config, &event.id, &project.label)?;
            thread.state = ThreadState::Active;
            thread.project_label = Some(project.label.clone());
            if let Some(work_item_id) = work_item_id_from_request(&event.text) {
                WorkModelStore::new(&project.checkout.path)
                    .read_work_item(work_item_id)
                    .with_context(|| format!("read linked Work Item {work_item_id}"))?;
                thread.linked_work = Some(work_item_id.to_owned());
            }
            thread.provider = Some(provider.to_owned());
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                source_timestamp: event.timestamp.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: true,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
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

    fn approve_revision_if_requested(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
    ) -> Result<bool> {
        let Some(revision_id) = event.text.trim().strip_prefix("approve ") else {
            return Ok(false);
        };
        if revision_id.trim().is_empty() {
            return Ok(false);
        }
        self.approve_planning_revision(&thread.id, revision_id.trim(), &event.user)?;
        thread.pending_artifact_revision = None;
        thread.pending_question = None;
        self.queue_reply(
            event,
            &format!("Approved planning revision {}.", revision_id.trim()),
        )?;
        Ok(true)
    }

    fn offer_linked_thread_if_requested(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
    ) -> Result<bool> {
        let Some(project) = event.text.strip_prefix("project: ").map(str::trim) else {
            return Ok(false);
        };
        if !self.config.projects.contains_key(project)
            || thread.project_label.as_deref() == Some(project)
        {
            return Ok(false);
        }
        thread.linked_thread_intent = Some(project.into());
        self.store.save_thread(&self.config, thread)?;
        self.queue_reply(
            event,
            &format!("This thread remains bound to its original project. Start a linked Fluent thread for project {project}."),
        )?;
        Ok(true)
    }
    fn complete_pending_turn(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
    ) -> Result<()> {
        if !self.turn_pool.request(&thread.id) {
            self.queue_reply(event, "Turn queued for fair provider admission.")?;
            return Ok(());
        }
        let mut current_event = event.clone();
        loop {
            let action_id = thread
                .pending_turn
                .as_ref()
                .map(|turn| turn.action_id.clone());
            let result = self.execute_pending_turn(&current_event, thread);
            self.turn_pool.finish(&thread.id);
            result?;
            let ready = thread.pending_turn.as_ref().is_some_and(|turn| {
                turn.retry_at_unix_seconds.is_none() && Some(&turn.action_id) != action_id.as_ref()
            });
            if !ready || !self.turn_pool.request(&thread.id) {
                return Ok(());
            }
            if let Some(turn) = &thread.pending_turn {
                current_event.id = turn.action_id.clone();
                current_event.acknowledgement_id.clear();
                current_event.user = turn.actor.clone();
                current_event.text.clear();
            }
        }
    }

    fn execute_pending_turn(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
    ) -> Result<()> {
        let mut turn = thread
            .pending_turn
            .clone()
            .context("thread has no pending provider turn")?;
        self.provider.select(
            thread
                .provider
                .as_deref()
                .context("thread has no selected provider")?,
        )?;
        let previously_completed = turn.completed_reply.is_some();
        let checkout = match self.bound_project(thread) {
            Ok(project) => project.checkout.path.clone(),
            Err(error) if error.to_string().contains("bound checkout is unavailable") => {
                self.queue_reply(
                    event,
                    "The bound checkout is unavailable. Fluent stopped provider access and will not rebind this thread automatically.",
                )?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
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
            let run = self.provider.run_turn(
                &turn.action_id,
                thread.provider_session.as_deref(),
                &scope,
                &turn.contribution,
                &turn.actor,
                &mut accepted,
            );
            let (session, reply) = match run {
                Ok(result) => result,
                Err(error) => {
                    let diagnostic = error.to_string();
                    if !is_recoverable_provider_failure(&diagnostic) {
                        return Err(error);
                    }
                    let failure = classify_provider_failure(&diagnostic);
                    let can_switch = self
                        .bound_project(thread)
                        .is_ok_and(|project| !project.project_approvers.is_empty());
                    self.queue_reply(event, &provider_recovery_message(&failure, can_switch))?;
                    let mut checkpoint = checkpoint.into_inner();
                    if let ProviderFailure::RateLimited {
                        retry_after_seconds,
                    } = failure
                        && let Some(turn) = checkpoint.pending_turn.as_mut()
                    {
                        turn.retry_at_unix_seconds =
                            Some((self.clock)().saturating_add(retry_after_seconds));
                    }
                    *thread = checkpoint;
                    self.store.save_thread(&self.config, thread)?;
                    return Ok(());
                }
            };
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
        let mut provider_references = self.provider.code_references(&scope, &turn.contribution)?;
        for reference in code_references_in_reply(&raw_reply, &scope.checkout) {
            if !provider_references.contains(&reference) {
                provider_references.push(reference);
            }
        }
        let references = provider_references
            .into_iter()
            .map(|reference| {
                if reference.path.is_absolute()
                    || reference
                        .path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    bail!("provider code reference is outside the bound checkout");
                }
                crate::slack_collaboration::format_code_reference(
                    &self.bound_project(thread)?.checkout,
                    &scope.checkout.join(reference.path),
                    reference.line,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let output = serde_json::from_str::<ProviderOutput>(&raw_reply).unwrap_or_else(|_| {
            ProviderOutput::Message {
                text: raw_reply.clone(),
            }
        });
        let accepted_contribution = turn.contribution.clone();
        if !thread
            .conversation_inputs
            .iter()
            .any(|input| input.event_id == event.id)
        {
            thread.conversation_inputs.push(ConversationInput {
                event_id: event.id.clone(),
                source_timestamp: event.timestamp.clone(),
                actor: event.user.clone(),
                content: accepted_contribution,
                corrections: Vec::new(),
                source_deleted: false,
            });
        }
        match output {
            ProviderOutput::Message { text } => {
                let reply = if previously_completed {
                    text
                } else if turn.starts_session {
                    format!(
                        "Bound to {}; provider: {}.\n{}",
                        safe_identity(self.bound_project(thread)?),
                        thread.provider.as_deref().unwrap_or("unknown"),
                        text
                    )
                } else {
                    text
                };
                let reply = if references.is_empty() {
                    reply
                } else {
                    format!("{reply}\n\n{}", references.join("\n"))
                };
                turn.completed_reply = Some(reply.clone());
                thread.pending_turn = Some(turn);
                self.store.save_thread(&self.config, thread)?;
                self.queue_reply(event, &reply)?;
                thread.pending_turn = None;
                self.store.save_thread(&self.config, thread)
            }
            ProviderOutput::PlanningArtifact { kind, markdown } => {
                turn.completed_reply = Some(raw_reply);
                thread.pending_turn = Some(turn);
                self.store.save_thread(&self.config, thread)?;
                self.present_planning_artifact_for_thread(thread, &kind, &markdown)?;
                thread.pending_turn = None;
                self.store.save_thread(&self.config, thread)
            }
        }?;
        if !thread.queued_contributions.is_empty() {
            let queued = std::mem::take(&mut thread.queued_contributions);
            let action_id = queued
                .iter()
                .map(|item| item.action_id.as_str())
                .collect::<Vec<_>>()
                .join("+");
            let contribution = coalesce_contributions(
                queued
                    .iter()
                    .map(|item| (item.actor.clone(), item.contribution.clone())),
            );
            thread.pending_turn = Some(PendingTurn {
                action_id: action_id.clone(),
                source_timestamp: event.timestamp.clone(),
                contribution,
                actor: "participants".into(),
                starts_session: false,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
            });
            self.store.save_thread(&self.config, thread)?;
        }
        Ok(())
    }
    fn bound_project(&self, thread: &FluentThread) -> Result<&ConfiguredProject> {
        let label = thread
            .project_label
            .as_deref()
            .context("thread is unbound")?;
        let project = self
            .config
            .projects
            .get(label)
            .context("thread project no longer configured")?;
        if !checkout_is_available(&project.checkout) {
            bail!("bound checkout is unavailable; provider access stopped without rebinding");
        }
        Ok(project)
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

fn work_item_id_from_request(text: &str) -> Option<&str> {
    text.split_once("work-item:")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .filter(|id| !id.is_empty())
}

fn selected_provider(text: &str) -> Option<&str> {
    let marker = "[provider=";
    let (_, suffix) = text.split_once(marker)?;
    let suffix = suffix.split_once(']')?.0;
    (!suffix.is_empty()).then_some(suffix)
}

fn is_recoverable_provider_failure(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "auth",
        "login",
        "credential",
        "unavailable",
        "rate limit",
        "429",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn code_references_in_reply(reply: &str, checkout: &Path) -> Vec<CodeReference> {
    reply
        .split_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|character: char| {
                matches!(
                    character,
                    '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | '.' | ';'
                )
            });
            let (path, line) = token.rsplit_once(':')?;
            let line = line.parse().ok()?;
            let path = PathBuf::from(path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                || !checkout.join(&path).is_file()
            {
                return None;
            }
            Some(CodeReference { path, line })
        })
        .collect()
}
