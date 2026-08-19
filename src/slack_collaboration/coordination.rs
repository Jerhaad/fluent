use super::*;
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
        self.switch_provider_metadata(&mut thread, actor, provider)?;
        self.store.save_thread(&self.config, &thread)?;
        let event = InboundEnvelope {
            id: format!("{}:provider-switch:{}", thread.id, provider),
            acknowledgement_id: String::new(),
            channel: thread.channel.clone(),
            timestamp: thread.id.clone(),
            thread_timestamp: Some(thread.id.clone()),
            user: actor.into(),
            text: format!("[provider={provider}]"),
        };
        self.complete_pending_turn(&event, &mut thread)?;
        self.store.save_thread(&self.config, &thread)
    }

    fn switch_provider_metadata(
        &self,
        thread: &mut FluentThread,
        actor: &str,
        provider: &str,
    ) -> Result<()> {
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
        self.store.persist_outbox(OutboundDelivery {
            id: format!("{}:provider-switch:{}", thread.id, provider), channel: thread.channel.clone(),
            thread_timestamp: thread.id.clone(), text: format!("Provider switched to {provider}; Fluent will resume from the durable Slice conversation."), delivered: false,
        })
    }

    /// Persist first, then acknowledge exactly once, then process and send.
    pub fn ingest(&mut self, event: InboundEnvelope) -> Result<()> {
        self.ingest_with_completion(event, true)
    }

    fn ingest_deferred(&mut self, event: InboundEnvelope) -> Result<()> {
        self.ingest_with_completion(event, false)
    }

    fn ingest_with_completion(&mut self, event: InboundEnvelope, run_now: bool) -> Result<()> {
        event.validate_thread_identity()?;
        if let Some(error) = &self.configuration_error {
            bail!("runner configuration is invalid; refusing Slack ingress: {error}");
        }
        match self.store.persist_inbox(&self.config, &event)? {
            InboxState::Processed => return Ok(()),
            InboxState::Pending if !event.acknowledgement_id.is_empty() => {
                self.slack.acknowledge(&event.acknowledgement_id)?
            }
            InboxState::Pending => {}
        }
        self.process(event.clone(), run_now)?;
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
        self.restore_connection_with_completion(history, connected_at, report_interruption, true)
    }

    fn restore_connection_with_completion(
        &mut self,
        history: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
        report_interruption: bool,
        run_now: bool,
    ) -> Result<()> {
        let mut events: Vec<_> = self
            .store
            .load_pending_inbox(&self.config)?
            .into_iter()
            .map(|event| (false, event))
            .collect();
        events.extend(history.into_iter().map(|event| (true, event)));
        events.sort_by(|(left_history, left), (right_history, right)| {
            left.timestamp
                .cmp(&right.timestamp)
                .then(left_history.cmp(right_history).then(left.id.cmp(&right.id)))
        });
        for (recovered, event) in events {
            self.ingest_with_completion(event.clone(), run_now)?;
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

    fn run_pending_thread(&mut self, thread_id: &str) -> Result<()> {
        let mut thread = self
            .store
            .load_thread(&self.config, thread_id)?
            .context("Fluent thread is unavailable")?;
        let turn = thread
            .pending_turn
            .as_ref()
            .context("thread has no pending provider turn")?;
        let event = InboundEnvelope {
            id: if turn.delivery_attempt == 0 {
                turn.action_id.clone()
            } else {
                format!("{}:retry:{}", turn.action_id, turn.delivery_attempt)
            },
            acknowledgement_id: String::new(),
            channel: thread.channel.clone(),
            timestamp: thread.id.clone(),
            thread_timestamp: Some(thread.id.clone()),
            user: turn.actor.clone(),
            text: turn.contribution.clone(),
        };
        self.complete_pending_turn(&event, &mut thread)
    }
    fn process(&mut self, event: InboundEnvelope, run_now: bool) -> Result<()> {
        let mut thread = self
            .store
            .load_thread(&self.config, &event.thread_id())?
            .unwrap_or_else(|| FluentThread::new(&event));
        if let Some(project_label) = &thread.project_label {
            self.store
                .bind_inbox_event(&self.config, &event.id, project_label)?;
        }
        if thread.state == ThreadState::Active
            && event.thread_timestamp.is_some()
            && let Some(provider) = selected_provider(&event.text)
        {
            if run_now {
                return self.switch_provider(&thread.id, &event.user, provider);
            }
            self.switch_provider_metadata(&mut thread, &event.user, provider)?;
            return self.store.save_thread(&self.config, &thread);
        }
        if thread
            .pending_turn
            .as_ref()
            .is_some_and(|turn| turn.action_id == event.id)
        {
            return if run_now {
                self.complete_pending_turn(&event, &mut thread)
            } else {
                Ok(())
            };
        }
        if !thread.processed_events.insert(event.id.clone()) {
            return Ok(());
        }
        if !policy::participant_can_contribute(&self.config, &thread, &event) {
            return self.store.save_thread(&self.config, &thread);
        }
        if thread.state == ThreadState::WaitingForProject && event.is_top_level_mention() {
            self.start_or_offer(&event, &mut thread, run_now)?;
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
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: false,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
                delivery_attempt: 0,
            });
            self.store.save_thread(&self.config, &thread)?;
            if run_now {
                self.complete_pending_turn(&event, &mut thread)?;
            }
        }
        self.store.save_thread(&self.config, &thread)
    }
    fn start_or_offer(
        &mut self,
        event: &InboundEnvelope,
        thread: &mut FluentThread,
        run_now: bool,
    ) -> Result<()> {
        let default = self
            .config
            .channel_defaults
            .get(&event.channel)
            .and_then(|p| self.config.projects.get(p));
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
            thread.provider = Some(provider.to_owned());
            thread.pending_turn = Some(PendingTurn {
                action_id: event.id.clone(),
                contribution: event.text.clone(),
                actor: event.user.clone(),
                starts_session: true,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
                delivery_attempt: 0,
            });
            self.store.save_thread(&self.config, thread)?;
            if let Some(connected_at) =
                read_runner_operational_state(self.home)?.last_successful_connection
            {
                thread.last_connection = Some(connected_at);
                self.store.save_thread(&self.config, thread)?;
                self.queue_thread_status(thread, None)?;
            }
            if run_now {
                self.complete_pending_turn(event, thread)?;
            }
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
        let original_turn = turn.clone();
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
        if thread.provider_session.is_none() && !thread.conversation.is_empty() {
            let mut context = thread
                .conversation
                .iter()
                .map(|message| (message.actor.clone(), message.contribution.clone()))
                .collect::<Vec<_>>();
            context.push((turn.actor.clone(), turn.contribution.clone()));
            turn.contribution = format!(
                "Durable Slice conversation:\n{}",
                coalesce_contributions(context)
            );
            turn.actor = "participants".into();
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
                if let Some(latest) = store.load_thread(config, &checkpoint.id)? {
                    if latest.provider != checkpoint.provider {
                        return Ok(());
                    }
                    checkpoint.queued_contributions = latest.queued_contributions;
                    checkpoint.processed_events = latest.processed_events;
                    checkpoint.conversation = latest.conversation;
                }
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
        let provider_reply = raw_reply.clone();
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
        let reply = if previously_completed {
            raw_reply
        } else if turn.starts_session {
            format!(
                "Bound to {}; provider: {}.\n{}",
                safe_identity(self.bound_project(thread)?),
                thread.provider.as_deref().unwrap_or("unknown"),
                raw_reply
            )
        } else {
            raw_reply
        };
        let reply = if references.is_empty() {
            reply
        } else {
            format!("{reply}\n\n{}", references.join("\n"))
        };
        if let Some(latest) = self.store.load_thread(&self.config, &thread.id)? {
            if latest.provider != thread.provider {
                *thread = latest;
                return Ok(());
            }
            thread.queued_contributions = latest.queued_contributions;
            thread.processed_events = latest.processed_events;
            thread.conversation = latest.conversation;
        }
        if !thread
            .conversation
            .iter()
            .any(|message| message.action_id == original_turn.action_id)
        {
            thread.conversation.push(QueuedContribution {
                action_id: original_turn.action_id.clone(),
                contribution: original_turn.contribution,
                actor: original_turn.actor,
            });
            thread.conversation.push(QueuedContribution {
                action_id: format!("{}:reply", original_turn.action_id),
                contribution: provider_reply,
                actor: "Fluent".into(),
            });
        }
        turn.completed_reply = Some(reply.clone());
        thread.pending_turn = Some(turn);
        self.store.save_thread(&self.config, thread)?;
        self.queue_reply(event, &reply)?;
        thread.pending_turn = None;
        self.store.save_thread(&self.config, thread)?;
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
                contribution,
                actor: "participants".into(),
                starts_session: false,
                provider_turn_id: None,
                completed_reply: None,
                retry_at_unix_seconds: None,
                delivery_attempt: 0,
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
        };
        self.store.persist_outbox(delivery)
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
        })
    }
}

struct ProviderCompletion {
    thread_id: String,
    result: Result<()>,
}

/// Keeps Slack ingress responsive while provider turns run on bounded workers.
pub struct AsyncCollaborationRunner<'a, P, S> {
    runner: CollaborationRunner<'a, P, S>,
    provider_factory: Arc<dyn Fn() -> P + Send + Sync>,
    completions_tx: std::sync::mpsc::Sender<ProviderCompletion>,
    completions_rx: std::sync::mpsc::Receiver<ProviderCompletion>,
    pool: FairTurnPool,
    active: BTreeSet<String>,
}

impl<'a, P, S> AsyncCollaborationRunner<'a, P, S>
where
    P: ConversationProvider + Clone + Send + Sync + 'static,
    S: SlackTransport,
{
    pub fn new(
        home: &'a Path,
        config: RunnerConfig,
        provider: P,
        slack: S,
        capacity: usize,
    ) -> Self {
        let template = provider.clone();
        Self::new_with_provider_factory(home, config, provider, slack, capacity, move || {
            template.clone()
        })
    }
}

impl<'a, P, S> AsyncCollaborationRunner<'a, P, S>
where
    P: ConversationProvider + Send + 'static,
    S: SlackTransport,
{
    pub fn new_with_provider_factory(
        home: &'a Path,
        config: RunnerConfig,
        provider: P,
        slack: S,
        capacity: usize,
        provider_factory: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        let (completions_tx, completions_rx) = std::sync::mpsc::channel();
        Self {
            runner: CollaborationRunner::new(home, config, provider, slack),
            provider_factory: Arc::new(provider_factory),
            completions_tx,
            completions_rx,
            pool: FairTurnPool::new(capacity),
            active: BTreeSet::new(),
        }
    }

    pub fn slack(&self) -> &S {
        self.runner.slack()
    }

    pub fn slack_mut(&mut self) -> &mut S {
        self.runner.slack_mut()
    }

    #[doc(hidden)]
    pub fn set_clock_for_test(&mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) {
        self.runner.set_clock_for_test(clock);
    }

    pub fn ingest(&mut self, event: InboundEnvelope) -> Result<()> {
        let thread_id = event.thread_id();
        self.runner.ingest_deferred(event)?;
        self.dispatch_if_ready(&thread_id)
    }

    pub fn recovery_targets(&self) -> Result<Vec<RecoveryTarget>> {
        self.runner.recovery_targets()
    }

    pub fn restore_connection(
        &mut self,
        history: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
        report_interruption: bool,
    ) -> Result<()> {
        self.runner.restore_connection_with_completion(
            history,
            connected_at,
            report_interruption,
            false,
        )?;
        let thread_ids: Vec<_> = self
            .runner
            .store
            .load_threads(&self.runner.config)?
            .into_keys()
            .collect();
        for thread_id in thread_ids {
            self.dispatch_if_ready(&thread_id)?;
        }
        Ok(())
    }

    fn dispatch_if_ready(&mut self, thread_id: &str) -> Result<()> {
        let Some(thread) = self
            .runner
            .store
            .load_thread(&self.runner.config, thread_id)?
        else {
            return Ok(());
        };
        let ready = thread
            .pending_turn
            .as_ref()
            .is_some_and(|turn| turn.retry_at_unix_seconds.is_none());
        if ready && self.pool.request(thread_id) {
            self.spawn_admitted(thread_id.to_owned());
        }
        Ok(())
    }

    fn spawn_admitted(&mut self, thread_id: String) {
        self.active.insert(thread_id.clone());
        let provider = (self.provider_factory)();
        let config = self.runner.config.clone();
        let home = self.runner.home.to_owned();
        let clock = self.runner.clock.clone();
        let sender = self.completions_tx.clone();
        std::thread::spawn(move || {
            let mut worker = CollaborationRunner::new(
                &home,
                config,
                provider,
                crate::slack_collaboration::InMemorySlack::default(),
            );
            worker.clock = clock;
            let result = worker.run_pending_thread(&thread_id);
            let _ = sender.send(ProviderCompletion { thread_id, result });
        });
    }

    fn handle_completion(&mut self, completion: ProviderCompletion) -> Result<()> {
        self.active.remove(&completion.thread_id);
        let next = self.pool.finish(&completion.thread_id);
        self.runner.deliver_pending()?;
        if let Some(next) = next {
            self.spawn_admitted(next);
        } else if completion.result.is_ok() {
            self.dispatch_if_ready(&completion.thread_id)?;
        }
        completion.result
    }

    pub fn wait_for_completion(&mut self) -> Result<()> {
        let completion = self
            .completions_rx
            .recv()
            .context("provider worker stopped")?;
        self.handle_completion(completion)
    }

    pub fn poll_completions(&mut self) -> Result<()> {
        while let Ok(completion) = self.completions_rx.try_recv() {
            self.handle_completion(completion)?;
        }
        Ok(())
    }

    pub fn next_retry_deadline(&self) -> Result<Option<u64>> {
        Ok(self
            .runner
            .store
            .load_threads(&self.runner.config)?
            .into_values()
            .filter_map(|thread| thread.pending_turn?.retry_at_unix_seconds)
            .min())
    }

    pub fn wake_due_retries(&mut self, now: u64) -> Result<()> {
        let thread_ids: Vec<_> = self
            .runner
            .store
            .load_threads(&self.runner.config)?
            .into_keys()
            .collect();
        for thread_id in thread_ids {
            let Some(mut thread) = self
                .runner
                .store
                .load_thread(&self.runner.config, &thread_id)?
            else {
                continue;
            };
            let due = thread
                .pending_turn
                .as_ref()
                .and_then(|turn| turn.retry_at_unix_seconds)
                .is_some_and(|deadline| deadline <= now);
            if due {
                if let Some(turn) = thread.pending_turn.as_mut() {
                    turn.retry_at_unix_seconds = None;
                    turn.delivery_attempt = turn.delivery_attempt.saturating_add(1);
                }
                self.runner
                    .store
                    .save_thread(&self.runner.config, &thread)?;
                self.dispatch_if_ready(&thread_id)?;
            }
        }
        Ok(())
    }

    /// Drive completions, Slack ingress, and the earliest durable retry timer.
    pub fn drive_once(&mut self, maximum_wait: std::time::Duration) -> Result<()> {
        self.poll_completions()?;
        let now = (self.runner.clock)();
        self.wake_due_retries(now)?;
        let retry_wait = self
            .next_retry_deadline()?
            .map(|deadline| std::time::Duration::from_secs(deadline.saturating_sub(now)))
            .unwrap_or(maximum_wait);
        let wait = retry_wait
            .min(maximum_wait)
            .max(std::time::Duration::from_millis(1));
        if let Some(event) = self.runner.slack_mut().receive_timeout(wait)? {
            self.ingest(event)?;
        }
        self.wake_due_retries((self.runner.clock)())?;
        self.poll_completions()
    }
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
