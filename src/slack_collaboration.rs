//! Durable, local-only Slack collaboration primitives.
//!
//! This module deliberately keeps Slack and provider protocol details at the
//! edge. The coordinator accepts durable envelopes, makes pure binding
//! decisions, and records stable outbound deliveries before an adapter sends
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

/// Slack app manifest for a local Socket Mode installation. It intentionally
/// has no request URL because the runner never accepts public HTTP ingress.
pub const SOCKET_MODE_MANIFEST: &str = r#"display_information:
  name: Fluent
features:
  bot_user:
    display_name: Fluent
    always_online: false
settings:
  socket_mode_enabled: true
  token_rotation_enabled: false
  event_subscriptions:
    bot_events:
      - app_mention
      - message.channels
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - channels:history
      - chat:write
"#;

/// Names used for the two Slack secrets in the macOS Keychain. Token values
/// are never represented in runner configuration or diagnostic structures.
pub const KEYCHAIN_SERVICE: &str = "dev.fluent.slack";
pub const APP_TOKEN_ACCOUNT: &str = "app-token";
pub const BOT_TOKEN_ACCOUNT: &str = "bot-token";

/// Host-owned secret boundary. Implement this with the macOS Keychain; tests
/// can use an in-memory implementation without ever handling diagnostics.
pub trait SecretStore {
    fn store(&mut self, service: &str, account: &str, secret: &str) -> Result<()>;
    fn read(&self, service: &str, account: &str) -> Result<String>;
}

/// Store Slack credentials exclusively at the host secret boundary.
pub fn store_slack_credentials<S: SecretStore>(
    secrets: &mut S,
    app_token: &str,
    bot_token: &str,
) -> Result<()> {
    if !app_token.starts_with("xapp-") || !bot_token.starts_with("xoxb-") {
        bail!("Slack credentials have an unexpected token type");
    }
    secrets.store(KEYCHAIN_SERVICE, APP_TOKEN_ACCOUNT, app_token)?;
    secrets.store(KEYCHAIN_SERVICE, BOT_TOKEN_ACCOUNT, bot_token)
}

/// Remove token-shaped strings before writing a diagnostic or Slack message.
pub fn redact_secrets(message: &str) -> String {
    message
        .split_whitespace()
        .map(|word| {
            if word.starts_with("xapp-") || word.starts_with("xoxb-") {
                "[REDACTED]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Report a host action that the current sandbox cannot perform. This is not a
/// Slack authentication failure: the owner must run the exact command outside
/// the enclosing sandbox.
pub fn host_handoff(command: &str) -> String {
    format!("Host access is required. Run outside the agent sandbox: {command}")
}

/// Background-service boundary for the per-user LaunchAgent. A production
/// adapter owns `launchctl`; the coordinator never shells out to it directly.
pub trait ServiceManager {
    fn install(&mut self, executable: &Path) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn restart(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn enabled(&self) -> bool;
}

/// Deterministic service-manager double for lifecycle tests.
#[derive(Debug, Default)]
pub struct FakeServiceManager {
    pub installed: bool,
    pub running: bool,
    pub executable: Option<PathBuf>,
}
impl ServiceManager for FakeServiceManager {
    fn install(&mut self, executable: &Path) -> Result<()> {
        self.installed = true;
        self.executable = Some(executable.into());
        Ok(())
    }
    fn start(&mut self) -> Result<()> {
        if !self.installed {
            bail!("Slack runner is not installed");
        }
        self.running = true;
        Ok(())
    }
    fn restart(&mut self) -> Result<()> {
        self.stop()?;
        self.start()
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }
    fn enabled(&self) -> bool {
        self.installed
    }
}

/// Install the exact current executable, then start it under the owner account.
pub fn enable_runner(manager: &mut impl ServiceManager, executable: &Path) -> Result<()> {
    manager.install(executable)?;
    manager.start()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerStatus {
    pub workspace: String,
    pub connection: String,
    pub last_successful_connection: Option<String>,
    pub build: String,
    pub failure: Option<String>,
}

impl std::fmt::Display for RunnerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace: {}; connection: {}; build: {}",
            self.workspace, self.connection, self.build
        )?;
        if let Some(last) = &self.last_successful_connection {
            write!(f, "; last successful connection: {last}")?;
        }
        if let Some(failure) = &self.failure {
            write!(f, "; failure: {}", redact_secrets(failure))?;
        }
        Ok(())
    }
}

pub fn runner_status(
    config: &RunnerConfig,
    connected: bool,
    last_connection: Option<String>,
    failure: Option<String>,
) -> RunnerStatus {
    RunnerStatus {
        workspace: config.workspace.clone(),
        connection: if connected {
            "connected"
        } else {
            "disconnected"
        }
        .into(),
        last_successful_connection: last_connection,
        build: crate::version::version_string(),
        failure: failure.map(|text| redact_secrets(&text)),
    }
}

/// Root for private collaboration state. Bound thread snapshots live under the
/// configured checkout's ignored Fluent runtime directory instead.
pub fn state_root(home: &Path) -> PathBuf {
    home.join(".config/fluent/slack")
}
pub fn config_path(home: &Path) -> PathBuf {
    state_root(home).join("config.json")
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
fn thread_path(project: &Path, id: &str) -> PathBuf {
    project
        .join(".fluent/work/slack/threads")
        .join(format!("{id}.json"))
}

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
            if !checkout_is_available(&project.checkout) {
                bail!("project {key} checkout is unavailable or changed");
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
}

impl FluentThread {
    fn new(event: &InboundEnvelope) -> Self {
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
        }
    }
}

/// The provider receives a checkout only once a thread is active and bound.
pub trait ConversationProvider {
    fn start(
        &mut self,
        checkout: &Path,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)>;
    fn resume(
        &mut self,
        session: &str,
        checkout: &Path,
        contribution: &str,
        actor: &str,
    ) -> Result<String>;
    fn ready(&self) -> Result<()> {
        Ok(())
    }
}

/// Slack transport is intentionally small: live Socket Mode and reconnect
/// history both feed `ingest`, while `post` consumes durable deliveries.
pub trait SlackTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()>;
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()>;
    fn check(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct InMemoryProvider {
    pub calls: Vec<(Option<String>, PathBuf, String, String)>,
}
impl ConversationProvider for InMemoryProvider {
    fn start(
        &mut self,
        checkout: &Path,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        self.calls.push((
            None,
            checkout.to_path_buf(),
            contribution.into(),
            actor.into(),
        ));
        Ok((
            format!("session-{}", self.calls.len()),
            format!("Fluent received: {contribution}"),
        ))
    }
    fn resume(
        &mut self,
        session: &str,
        checkout: &Path,
        contribution: &str,
        actor: &str,
    ) -> Result<String> {
        self.calls.push((
            Some(session.into()),
            checkout.to_path_buf(),
            contribution.into(),
            actor.into(),
        ));
        Ok(format!("Fluent received: {contribution}"))
    }
}

#[derive(Debug, Default)]
pub struct InMemorySlack {
    pub acknowledgements: Vec<String>,
    pub deliveries: Vec<OutboundDelivery>,
}
impl SlackTransport for InMemorySlack {
    fn acknowledge(&mut self, event_id: &str) -> Result<()> {
        self.acknowledgements.push(event_id.into());
        Ok(())
    }
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.deliveries.push(delivery.clone());
        Ok(())
    }
}

/// Coordinates one local runner. File state is written before external effects.
pub struct CollaborationRunner<'a, P, S> {
    home: &'a Path,
    pub config: RunnerConfig,
    provider: P,
    slack: S,
}

impl<'a, P: ConversationProvider, S: SlackTransport> CollaborationRunner<'a, P, S> {
    pub fn new(home: &'a Path, config: RunnerConfig, provider: P, slack: S) -> Self {
        Self {
            home,
            config,
            provider,
            slack,
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
        let is_new = persist_inbox(self.home, &event)?;
        if is_new {
            self.slack.acknowledge(&event.id)?;
        } else {
            return Ok(());
        }
        self.process(event)
    }
    pub fn reconnect(
        &mut self,
        events: impl IntoIterator<Item = InboundEnvelope>,
        connected_at: String,
    ) -> Result<()> {
        for event in events {
            self.ingest(event)?;
        }
        for mut thread in load_threads(self.home)?.into_values() {
            thread.last_connection = Some(connected_at.clone());
            save_thread(self.home, &self.config, &thread)?;
        }
        Ok(())
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
        if !thread.processed_events.insert(event.id.clone()) {
            return Ok(());
        }
        let allowed = self
            .config
            .projects
            .values()
            .any(|p| p.allowed_participants.contains(&event.user));
        if !allowed {
            return save_thread(self.home, &self.config, &thread);
        }
        if thread.state == ThreadState::WaitingForProject && event.is_top_level_mention() {
            self.start_or_offer(&event, &mut thread)?;
        } else if thread.state == ThreadState::Active && event.thread_timestamp.is_some() {
            let checkout = self.bound_project(&thread)?.checkout.path.clone();
            let session = thread
                .provider_session
                .as_deref()
                .context("active thread lacks provider session")?;
            let reply = self
                .provider
                .resume(session, &checkout, &event.text, &event.user)?;
            self.queue_reply(&event, &reply)?;
        }
        save_thread(self.home, &self.config, &thread)?;
        self.deliver_pending()
    }
    fn start_or_offer(&mut self, event: &InboundEnvelope, thread: &mut FluentThread) -> Result<()> {
        let default = self
            .config
            .channel_defaults
            .get(&event.channel)
            .and_then(|p| self.config.projects.get(p));
        let available = default.filter(|p| checkout_is_available(&p.checkout));
        if let Some(project) = available {
            let (session, reply) =
                self.provider
                    .start(&project.checkout.path, &event.text, &event.user)?;
            thread.state = ThreadState::Active;
            thread.project_label = Some(project.label.clone());
            thread.provider_session = Some(session);
            self.queue_reply(
                event,
                &format!("Bound to {}.\n{}", safe_identity(project), reply),
            )?;
        } else {
            let projects = self.available_projects();
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
    fn available_projects(&self) -> Vec<String> {
        self.config
            .projects
            .values()
            .filter(|p| checkout_is_available(&p.checkout))
            .map(|p| p.label.clone())
            .collect()
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

pub fn safe_identity(project: &ConfiguredProject) -> String {
    format!(
        "Project: {}; checkout: {}; branch: {}; commit: {}",
        project.label, project.checkout.label, project.checkout.branch, project.checkout.commit
    )
}
pub fn checkout_is_available(checkout: &Checkout) -> bool {
    checkout.path.is_dir() && checkout.path.join(".git").exists()
}

pub fn write_config(home: &Path, config: &RunnerConfig) -> Result<()> {
    config.validate()?;
    ensure_root(home)?;
    atomic_write(&config_path(home), &serde_json::to_vec_pretty(config)?).map_err(Into::into)
}
pub fn read_config(home: &Path) -> Result<RunnerConfig> {
    let value: RunnerConfig = serde_json::from_slice(&fs::read(config_path(home))?)?;
    value.validate()?;
    Ok(value)
}
pub fn preflight<P: ConversationProvider, S: SlackTransport>(
    config: &RunnerConfig,
    provider: &P,
    slack: &S,
) -> Result<()> {
    config.validate()?;
    provider.ready()?;
    slack.check()?;
    Ok(())
}

fn ensure_root(home: &Path) -> Result<()> {
    fs::create_dir_all(state_root(home))?;
    Ok(())
}
fn read_json<T: for<'a> Deserialize<'a> + Default>(path: PathBuf) -> Result<T> {
    match fs::read(path) {
        Ok(raw) => Ok(serde_json::from_slice(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
fn persist_inbox(home: &Path, event: &InboundEnvelope) -> Result<bool> {
    ensure_root(home)?;
    let mut inbox: BTreeMap<String, InboundEnvelope> = read_json(inbox_path(home))?;
    if inbox.contains_key(&event.id) {
        return Ok(false);
    }
    inbox.insert(event.id.clone(), event.clone());
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(true)
}
fn load_outbox(home: &Path) -> Result<Vec<OutboundDelivery>> {
    read_json(outbox_path(home))
}
fn save_outbox(home: &Path, value: &[OutboundDelivery]) -> Result<()> {
    ensure_root(home)?;
    atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(value)?).map_err(Into::into)
}
fn persist_outbox(home: &Path, delivery: OutboundDelivery) -> Result<()> {
    let mut outbox = load_outbox(home)?;
    if !outbox.iter().any(|d| d.id == delivery.id) {
        outbox.push(delivery);
        save_outbox(home, &outbox)?;
    }
    Ok(())
}
fn load_threads(home: &Path) -> Result<BTreeMap<String, FluentThread>> {
    read_json(thread_index_path(home))
}
fn load_thread(home: &Path, config: &RunnerConfig, id: &str) -> Result<Option<FluentThread>> {
    let index = load_threads(home)?;
    let Some(stub) = index.get(id) else {
        return Ok(None);
    };
    let Some(project) = stub
        .project_label
        .as_ref()
        .and_then(|p| config.projects.get(p))
    else {
        return Ok(Some(stub.clone()));
    };
    let path = thread_path(&project.checkout.path, id);
    match fs::read(path) {
        Ok(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(stub.clone())),
        Err(e) => Err(e.into()),
    }
}
fn save_thread(home: &Path, config: &RunnerConfig, thread: &FluentThread) -> Result<()> {
    ensure_root(home)?;
    let mut index = load_threads(home)?;
    index.insert(thread.id.clone(), thread.clone());
    atomic_write(
        &thread_index_path(home),
        &serde_json::to_vec_pretty(&index)?,
    )?;
    if let Some(project) = thread
        .project_label
        .as_ref()
        .and_then(|p| config.projects.get(p))
    {
        let path = thread_path(&project.checkout.path, &thread.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &serde_json::to_vec_pretty(thread)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct MemorySecrets(BTreeMap<(String, String), String>);
    impl SecretStore for MemorySecrets {
        fn store(&mut self, service: &str, account: &str, secret: &str) -> Result<()> {
            self.0
                .insert((service.into(), account.into()), secret.into());
            Ok(())
        }
        fn read(&self, service: &str, account: &str) -> Result<String> {
            self.0
                .get(&(service.into(), account.into()))
                .cloned()
                .context("missing secret")
        }
    }
    use tempfile::TempDir;
    fn setup() -> (TempDir, TempDir, RunnerConfig) {
        let home = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        fs::create_dir(checkout.path().join(".git")).unwrap();
        let project = ConfiguredProject {
            label: "app".into(),
            checkout: Checkout {
                label: "main".into(),
                path: checkout.path().into(),
                identity: "id".into(),
                branch: "main".into(),
                commit: "abc".into(),
            },
            allowed_participants: ["U1".into()].into_iter().collect(),
        };
        let mut config = RunnerConfig {
            workspace: "workspace".into(),
            projects: BTreeMap::new(),
            channel_defaults: BTreeMap::new(),
        };
        config.projects.insert("app".into(), project);
        config.channel_defaults.insert("C1".into(), "app".into());
        (home, checkout, config)
    }
    fn event(id: &str, thread: Option<&str>, text: &str) -> InboundEnvelope {
        InboundEnvelope {
            id: id.into(),
            channel: "C1".into(),
            timestamp: "1.0".into(),
            thread_timestamp: thread.map(str::to_owned),
            user: "U1".into(),
            text: text.into(),
        }
    }
    #[test]
    fn top_level_mention_uses_available_channel_default() {
        let (home, _checkout, config) = setup();
        let mut r = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        r.ingest(event("E1", None, "@Fluent build this")).unwrap();
        assert_eq!(r.provider().calls.len(), 1);
        assert!(r.slack().deliveries[0].text.contains("Project: app"));
        assert!(
            !r.slack().deliveries[0]
                .text
                .contains(&home.path().display().to_string())
        );
    }
    #[test]
    fn redelivered_slack_event_has_no_duplicate_effect() {
        let (home, _checkout, config) = setup();
        let mut r = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        let e = event("E1", None, "@Fluent hi");
        r.ingest(e.clone()).unwrap();
        r.ingest(e).unwrap();
        assert_eq!(r.provider().calls.len(), 1);
        assert_eq!(r.slack().deliveries.len(), 1);
    }
    #[test]
    fn missing_default_denies_provider_access() {
        let (home, _checkout, mut config) = setup();
        config.channel_defaults.clear();
        let mut r = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        r.ingest(event("E1", None, "@Fluent hi")).unwrap();
        assert!(r.provider().calls.is_empty());
        assert!(
            r.slack().deliveries[0]
                .text
                .contains("Available projects: app")
        );
    }

    #[test]
    fn unavailable_default_offers_available_projects_without_provider_access() {
        let (home, checkout, mut config) = setup();
        fs::remove_dir(checkout.path().join(".git")).unwrap();
        let available = TempDir::new().unwrap();
        fs::create_dir(available.path().join(".git")).unwrap();
        config.projects.insert(
            "other".into(),
            ConfiguredProject {
                label: "other".into(),
                checkout: Checkout {
                    label: "other-main".into(),
                    path: available.path().into(),
                    identity: "other".into(),
                    branch: "main".into(),
                    commit: "def".into(),
                },
                allowed_participants: ["U1".into()].into_iter().collect(),
            },
        );
        let mut r = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        r.ingest(event("E1", None, "@Fluent hi")).unwrap();
        assert!(r.provider().calls.is_empty());
        assert!(r.slack().deliveries[0].text.contains("other"));
        assert!(!r.slack().deliveries[0].text.contains("app,"));
    }

    #[test]
    fn credentials_stay_in_secret_store_and_redact_output() {
        let mut secrets = MemorySecrets::default();
        store_slack_credentials(&mut secrets, "xapp-secret", "xoxb-secret").unwrap();
        assert_eq!(
            secrets.read(KEYCHAIN_SERVICE, APP_TOKEN_ACCOUNT).unwrap(),
            "xapp-secret"
        );
        assert_eq!(
            redact_secrets("failed xapp-secret xoxb-secret"),
            "failed [REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn status_reports_safe_operational_identity() {
        let (_, _checkout, config) = setup();
        let status = runner_status(
            &config,
            true,
            Some("2026-08-17T00:00:00Z".into()),
            Some("xapp-secret disconnected".into()),
        );
        let rendered = status.to_string();
        assert!(rendered.contains("workspace: workspace"));
        assert!(rendered.contains("last successful connection"));
        assert!(!rendered.contains("xapp-secret"));
    }

    #[test]
    fn setup_writes_socket_mode_manifest_and_validates_configuration() {
        let (home, _checkout, config) = setup();
        assert!(SOCKET_MODE_MANIFEST.contains("socket_mode_enabled: true"));
        assert!(!SOCKET_MODE_MANIFEST.contains("request_url"));
        write_config(home.path(), &config).unwrap();
        assert_eq!(read_config(home.path()).unwrap().workspace, "workspace");
        preflight(
            &config,
            &InMemoryProvider::default(),
            &InMemorySlack::default(),
        )
        .unwrap();
    }

    #[test]
    fn malformed_configuration_fails_closed() {
        let (home, _checkout, mut config) = setup();
        config.workspace.clear();
        assert!(write_config(home.path(), &config).is_err());
    }

    #[test]
    fn enable_installs_controllable_login_service() {
        let mut service = FakeServiceManager::default();
        enable_runner(&mut service, Path::new("/Applications/fluent")).unwrap();
        assert!(service.enabled() && service.running);
        service.restart().unwrap();
        assert!(service.running);
        service.stop().unwrap();
        assert!(!service.running);
    }
}
