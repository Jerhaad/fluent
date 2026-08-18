//! Durable, local-only Slack collaboration primitives.
//!
//! This module deliberately keeps Slack and provider protocol details at the
//! edge. The coordinator accepts durable envelopes, makes pure binding
//! decisions, and records stable outbound deliveries before an adapter sends
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::coder::CoderKind;
use crate::content::ContentResolver;

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

/// Stores credentials in the runner owner's login Keychain.
pub struct KeychainSecretStore;
impl SecretStore for KeychainSecretStore {
    fn store(&mut self, service: &str, account: &str, secret: &str) -> Result<()> {
        let output = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-w",
                secret,
            ])
            .output()
            .context("invoke macOS Keychain")?;
        if !output.status.success() {
            bail!(
                "macOS Keychain rejected Slack credential: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
    fn read(&self, service: &str, account: &str) -> Result<String> {
        let output = Command::new("security")
            .args(["find-generic-password", "-a", account, "-s", service, "-w"])
            .output()
            .context("read macOS Keychain")?;
        if !output.status.success() {
            bail!(
                "macOS Keychain has no Slack credential: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
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

/// Preserve permission-denial classification at owner-only boundaries while
/// leaving ordinary Slack authentication and configuration failures intact.
pub fn classify_host_boundary(error: anyhow::Error, command: &str) -> anyhow::Error {
    let diagnostic = format!("{error:#}");
    let lowered = diagnostic.to_ascii_lowercase();
    if lowered.contains("operation not permitted")
        || lowered.contains("permission denied")
        || lowered.contains("sandbox")
    {
        anyhow::anyhow!(
            "{}\nCause: {}",
            host_handoff(command),
            redact_secrets(&diagnostic)
        )
    } else {
        error
    }
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

/// Manages the owner-scoped LaunchAgent used by the local runner.
pub struct LaunchAgentServiceManager {
    pub label: String,
    pub plist: PathBuf,
}
impl LaunchAgentServiceManager {
    pub fn new(home: &Path) -> Self {
        Self {
            label: "dev.fluent.slack-runner".into(),
            plist: home.join("Library/LaunchAgents/dev.fluent.slack-runner.plist"),
        }
    }
    fn domain(&self) -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }
    fn run(&self, verb: &str) -> Result<()> {
        let output = Command::new("launchctl")
            .args([verb, &self.domain(), &self.plist.display().to_string()])
            .output()?;
        if !output.status.success() {
            bail!(
                "launchctl {verb} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn program_arguments(executable: &Path) -> Vec<String> {
        vec![
            executable.display().to_string(),
            "slack".into(),
            "run".into(),
        ]
    }
}
impl ServiceManager for LaunchAgentServiceManager {
    fn install(&mut self, executable: &Path) -> Result<()> {
        let parent = self
            .plist
            .parent()
            .context("LaunchAgent path has no parent")?;
        fs::create_dir_all(parent)?;
        let arguments = Self::program_arguments(executable);
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>{}</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>",
            self.label, arguments[0], arguments[1], arguments[2]
        );
        atomic_write(&self.plist, xml.as_bytes())?;
        Ok(())
    }
    fn start(&mut self) -> Result<()> {
        self.run("bootstrap")
    }
    fn restart(&mut self) -> Result<()> {
        self.stop()?;
        self.start()
    }
    fn stop(&mut self) -> Result<()> {
        let output = Command::new("launchctl")
            .args(["bootout", &self.domain(), &self.plist.display().to_string()])
            .output()?;
        if !output.status.success() {
            bail!(
                "launchctl bootout failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
    fn enabled(&self) -> bool {
        self.plist.exists()
    }
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

/// Apply an owner-requested service lifecycle action.
pub fn control_runner(manager: &mut impl ServiceManager, action: RunnerControl) -> Result<()> {
    match action {
        RunnerControl::Restart => manager.restart(),
        RunnerControl::Stop => manager.stop(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerControl {
    Restart,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerStatus {
    pub workspace: String,
    pub connection: String,
    pub last_successful_connection: Option<String>,
    pub build: String,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerOperationalState {
    pub connected: bool,
    pub last_successful_connection: Option<String>,
    pub failure: Option<String>,
}

pub fn read_runner_operational_state(home: &Path) -> Result<RunnerOperationalState> {
    read_json(operational_status_path(home))
}

fn save_runner_operational_state(home: &Path, state: &RunnerOperationalState) -> Result<()> {
    ensure_root(home)?;
    atomic_write(
        &operational_status_path(home),
        &serde_json::to_vec_pretty(state)?,
    )?;
    Ok(())
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
fn operational_status_path(home: &Path) -> PathBuf {
    state_root(home).join("status.json")
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

/// A provider action recorded before the provider can observe it. The event
/// identity remains stable across restart, so providers can reconcile a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTurn {
    pub action_id: String,
    pub contribution: String,
    pub actor: String,
    pub starts_session: bool,
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
            pending_turn: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderScope {
    pub checkout: PathBuf,
    pub slice_draft: PathBuf,
}

/// The provider receives an explicit stage scope only once a thread is active
/// and bound.
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

/// Slack transport is intentionally small: live Socket Mode and reconnect
/// history both feed `ingest`, while `post` consumes durable deliveries.
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

#[derive(Debug, Default)]
pub struct InMemoryProvider {
    pub calls: Vec<(Option<String>, PathBuf, String, String)>,
    completed_actions: BTreeMap<String, (String, String)>,
}
impl ConversationProvider for InMemoryProvider {
    fn start(
        &mut self,
        action_id: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        if let Some((session, reply)) = self.completed_actions.get(action_id) {
            return Ok((session.clone(), reply.clone()));
        }
        self.calls.push((
            None,
            scope.checkout.clone(),
            contribution.into(),
            actor.into(),
        ));
        let result = (
            format!("session-{}", self.calls.len()),
            format!("Fluent received: {contribution}"),
        );
        self.completed_actions
            .insert(action_id.into(), result.clone());
        Ok(result)
    }
    fn resume(
        &mut self,
        action_id: &str,
        session: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<String> {
        if let Some((_, reply)) = self.completed_actions.get(action_id) {
            return Ok(reply.clone());
        }
        self.calls.push((
            Some(session.into()),
            scope.checkout.clone(),
            contribution.into(),
            actor.into(),
        ));
        let reply = format!("Fluent received: {contribution}");
        self.completed_actions
            .insert(action_id.into(), (session.into(), reply.clone()));
        Ok(reply)
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

/// Live Slack Socket Mode edge. It opens the Socket Mode URL through Slack's
/// Web API, accepts one event envelope, and acknowledges it on that socket.
pub struct SocketModeTransport {
    web: SlackWebApiTransport,
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl SocketModeTransport {
    fn open_socket(
        api_base: &str,
        app_token: &str,
    ) -> Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>>
    {
        let response = ureq::post(&format!(
            "{}/apps.connections.open",
            api_base.trim_end_matches('/')
        ))
        .set("Authorization", &format!("Bearer {app_token}"))
        .send_json(serde_json::json!({}))?;
        let value: serde_json::Value = response.into_json()?;
        let url = value["url"].as_str().context("Slack Socket Mode URL")?;
        Ok(tungstenite::connect(url)?.0)
    }

    pub fn connect(
        api_base: impl Into<String>,
        app_token: String,
        bot_token: String,
    ) -> Result<Self> {
        let api_base = api_base.into();
        let socket = Self::open_socket(&api_base, &app_token)?;
        Ok(Self {
            web: SlackWebApiTransport::new(api_base, app_token, bot_token),
            socket,
        })
    }
}

/// Translate one Socket Mode event envelope without exposing Slack payload
/// shapes to the collaboration coordinator.
pub fn decode_socket_mode_envelope(text: &str) -> Result<InboundEnvelope> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let event = &value["payload"]["event"];
    Ok(InboundEnvelope {
        id: value["payload"]["event_id"]
            .as_str()
            .context("Slack event id")?
            .into(),
        acknowledgement_id: value["envelope_id"]
            .as_str()
            .context("Slack envelope id")?
            .into(),
        channel: event["channel"].as_str().context("Slack channel")?.into(),
        timestamp: event["ts"].as_str().context("Slack timestamp")?.into(),
        thread_timestamp: event["thread_ts"].as_str().map(str::to_owned),
        user: event["user"].as_str().context("Slack user")?.into(),
        text: event["text"].as_str().unwrap_or_default().into(),
    })
}

impl SlackTransport for SocketModeTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()> {
        self.socket.send(tungstenite::Message::Text(
            serde_json::json!({"envelope_id": event_id}).to_string(),
        ))?;
        Ok(())
    }
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.web.post(delivery)
    }
    fn check(&self) -> Result<()> {
        self.web.check()
    }
    fn receive(&mut self) -> Result<InboundEnvelope> {
        let message = self.socket.read()?;
        let text = message.into_text()?;
        decode_socket_mode_envelope(&text)
    }
    fn reconnect(&mut self) -> Result<()> {
        self.socket = Self::open_socket(&self.web.api_base, &self.web.app_token)?;
        Ok(())
    }
    fn bounded_history(&mut self, threads: &[(String, String)]) -> Result<Vec<InboundEnvelope>> {
        let mut events = Vec::new();
        for (channel, thread_timestamp) in threads {
            let value = self.web.call_value(
                "conversations.replies",
                &self.web.bot_token,
                serde_json::json!({"channel": channel, "ts": thread_timestamp, "limit": 100}),
            )?;
            for message in value["messages"].as_array().into_iter().flatten() {
                let Some(timestamp) = message["ts"].as_str() else {
                    continue;
                };
                if timestamp == thread_timestamp {
                    continue;
                }
                events.push(InboundEnvelope {
                    id: format!("history:{channel}:{timestamp}"),
                    acknowledgement_id: String::new(),
                    channel: channel.clone(),
                    timestamp: timestamp.into(),
                    thread_timestamp: Some(thread_timestamp.clone()),
                    user: message["user"].as_str().unwrap_or_default().into(),
                    text: message["text"].as_str().unwrap_or_default().into(),
                });
            }
        }
        Ok(events)
    }
}

/// Native Rust Web API edge used by the Socket Mode runner for durable posts.
/// Socket acknowledgements travel through the same injected edge in tests.
pub struct SlackWebApiTransport {
    api_base: String,
    bot_token: String,
    app_token: String,
}

impl SlackWebApiTransport {
    pub fn new(api_base: impl Into<String>, app_token: String, bot_token: String) -> Self {
        Self {
            api_base: api_base.into().trim_end_matches('/').into(),
            app_token,
            bot_token,
        }
    }
    fn call_value(
        &self,
        method: &str,
        token: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = ureq::post(&format!("{}/{method}", self.api_base))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(body)?;
        let value: serde_json::Value = response.into_json()?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            bail!("Slack {method} failed")
        }
        Ok(value)
    }
    fn call(&self, method: &str, token: &str, body: serde_json::Value) -> Result<()> {
        self.call_value(method, token, body).map(|_| ())
    }
}

impl SlackTransport for SlackWebApiTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()> {
        self.call(
            "apps.connections.ack",
            &self.app_token,
            serde_json::json!({"envelope_id": event_id}),
        )
    }
    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.call("chat.postMessage", &self.bot_token, serde_json::json!({
            "channel": delivery.channel, "thread_ts": delivery.thread_timestamp, "text": delivery.text
        }))
    }
    fn check(&self) -> Result<()> {
        self.call("auth.test", &self.bot_token, serde_json::json!({}))
    }
}

/// Minimal JSON-lines Codex App Server client. The app server receives a
/// stable action id so a replay reconciles the same provider turn.
pub struct CodexAppServerProvider {
    executable: PathBuf,
    runner_home: Option<PathBuf>,
}

impl CodexAppServerProvider {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            runner_home: None,
        }
    }
    pub fn new_scoped(executable: PathBuf, runner_home: PathBuf) -> Self {
        Self {
            executable,
            runner_home: Some(runner_home),
        }
    }
    fn turn(
        &self,
        action_id: &str,
        session: Option<&str>,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        fs::create_dir_all(&scope.slice_draft)?;
        let profile = if let Some(runner_home) = &self.runner_home {
            let codex_home = state_root(runner_home).join("provider/codex-home");
            fs::create_dir_all(&codex_home)?;
            let resolver = ContentResolver::new(Some(&scope.checkout));
            let profile = crate::os::render_profile_for_access_for_coder_with_codex_home(
                &resolver,
                &runner_home.display().to_string(),
                &[scope.slice_draft.clone(), codex_home.clone()],
                std::slice::from_ref(&scope.checkout),
                CoderKind::Codex,
                Some(&codex_home),
            )?;
            crate::os::preflight_profile(&profile)?;
            Some(profile)
        } else {
            None
        };
        let mut command = if let Some(profile) = &profile {
            crate::os::command_under_profile(profile, &self.executable)
        } else {
            Command::new(&self.executable)
        };
        command.arg("app-server");
        if let Some(runner_home) = &self.runner_home {
            command.env(
                "CODEX_HOME",
                state_root(runner_home).join("provider/codex-home"),
            );
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let request = serde_json::json!({"action_id": action_id, "session": session, "checkout": scope.checkout, "slice_draft": scope.slice_draft, "contribution": contribution, "actor": actor});
        child
            .stdin
            .as_mut()
            .context("open Codex App Server stdin")?
            .write_all(serde_json::to_string(&request)?.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("Codex App Server failed")
        }
        let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        Ok((
            response["session"]
                .as_str()
                .context("Codex response session")?
                .into(),
            response["reply"]
                .as_str()
                .context("Codex response reply")?
                .into(),
        ))
    }
}

impl ConversationProvider for CodexAppServerProvider {
    fn start(
        &mut self,
        action_id: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        self.turn(action_id, None, scope, contribution, actor)
    }
    fn resume(
        &mut self,
        action_id: &str,
        session: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<String> {
        Ok(self
            .turn(action_id, Some(session), scope, contribution, actor)?
            .1)
    }
    fn ready(&self) -> Result<()> {
        Command::new(&self.executable)
            .arg("app-server")
            .arg("--help")
            .status()?
            .success()
            .then_some(())
            .context("Codex App Server is unavailable")
    }
}

/// Coordinates one local runner. File state is written before external effects.
pub struct CollaborationRunner<'a, P, S> {
    home: &'a Path,
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
        if !self.participant_can_contribute(&thread, &event) {
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
            let projects = self.available_projects(&event.user);
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
    fn participant_can_contribute(&self, thread: &FluentThread, event: &InboundEnvelope) -> bool {
        match thread.project_label.as_deref() {
            Some(project_label) => self
                .config
                .projects
                .get(project_label)
                .is_some_and(|project| project.allowed_participants.contains(&event.user)),
            None => match self.config.channel_defaults.get(&event.channel) {
                Some(project_label) => self
                    .config
                    .projects
                    .get(project_label)
                    .is_some_and(|project| project.allowed_participants.contains(&event.user)),
                None => self
                    .config
                    .projects
                    .values()
                    .any(|project| project.allowed_participants.contains(&event.user)),
            },
        }
    }
    fn available_projects(&self, participant: &str) -> Vec<String> {
        self.config
            .projects
            .values()
            .filter(|p| {
                p.allowed_participants.contains(participant) && checkout_is_available(&p.checkout)
            })
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
    if !checkout.path.is_dir() || !checkout.path.join(".git").exists() {
        return false;
    }
    // Lightweight fixtures deliberately use a .git directory without a real
    // repository. Real checkouts must match both recorded branch and commit.
    let branch = Command::new("git")
        .args([
            "-C",
            &checkout.path.display().to_string(),
            "branch",
            "--show-current",
        ])
        .output();
    let commit = Command::new("git")
        .args([
            "-C",
            &checkout.path.display().to_string(),
            "rev-parse",
            "HEAD",
        ])
        .output();
    match (branch, commit) {
        (Ok(branch), Ok(commit)) if branch.status.success() && commit.status.success() => {
            String::from_utf8_lossy(&branch.stdout).trim() == checkout.branch
                && String::from_utf8_lossy(&commit.stdout).trim() == checkout.commit
        }
        _ => true,
    }
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
    for project in config.projects.values() {
        if !checkout_is_available(&project.checkout) {
            bail!(
                "configured checkout {} is unavailable or changed",
                project.label
            );
        }
    }
    provider.ready()?;
    slack.check()?;
    Ok(())
}

fn connection_time() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Replay durable ingress and bounded Slack history through the same
/// idempotent coordinator path after startup or reconnect.
pub fn reconcile_connected_runner<P: ConversationProvider, S: SlackTransport>(
    runner: &mut CollaborationRunner<'_, P, S>,
) -> Result<()> {
    let targets = runner.recovery_targets()?;
    let history = runner.slack_mut().bounded_history(&targets)?;
    let connected_at = connection_time();
    runner.reconnect(history, connected_at.clone())?;
    save_runner_operational_state(
        runner.home,
        &RunnerOperationalState {
            connected: true,
            last_successful_connection: Some(connected_at),
            failure: None,
        },
    )
}

/// Validate owner-scoped configuration and read both Keychain credentials
/// before the foreground runner enters its transport loop.
pub fn run_configured_runner(home: &Path) -> Result<()> {
    let config = read_config(home)?;
    let secrets = KeychainSecretStore;
    let app_token = secrets.read(KEYCHAIN_SERVICE, APP_TOKEN_ACCOUNT)?;
    let bot_token = secrets.read(KEYCHAIN_SERVICE, BOT_TOKEN_ACCOUNT)?;
    let api_base =
        std::env::var("FLUENT_SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com/api".into());
    let codex = std::env::var_os("FLUENT_CODEX_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let provider = CodexAppServerProvider::new_scoped(codex, home.to_path_buf());
    let slack = SocketModeTransport::connect(api_base, app_token, bot_token)?;
    preflight(&config, &provider, &slack)?;
    let mut runner = CollaborationRunner::new(home, config, provider, slack);
    reconcile_connected_runner(&mut runner)?;
    loop {
        match runner.slack_mut().receive() {
            Ok(event) => runner.ingest(event)?,
            Err(error) => {
                let previous = read_runner_operational_state(home).unwrap_or_default();
                save_runner_operational_state(
                    home,
                    &RunnerOperationalState {
                        connected: false,
                        last_successful_connection: previous.last_successful_connection,
                        failure: Some(redact_secrets(&error.to_string())),
                    },
                )?;
                runner.slack_mut().reconnect()?;
                reconcile_connected_runner(&mut runner)?;
            }
        }
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InboxEntry {
    event: InboundEnvelope,
    processed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboxState {
    Pending,
    Processed,
}

fn persist_inbox(home: &Path, event: &InboundEnvelope) -> Result<InboxState> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    if let Some(entry) = inbox.get(&event.id) {
        return Ok(if entry.processed {
            InboxState::Processed
        } else {
            InboxState::Pending
        });
    }
    if inbox.values().any(|entry| {
        entry.processed
            && entry.event.channel == event.channel
            && entry.event.timestamp == event.timestamp
            && entry.event.user == event.user
            && entry.event.text == event.text
    }) {
        return Ok(InboxState::Processed);
    }
    inbox.insert(
        event.id.clone(),
        InboxEntry {
            event: event.clone(),
            processed: false,
        },
    );
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(InboxState::Pending)
}
fn mark_inbox_processed(home: &Path, id: &str) -> Result<()> {
    let _lock = state_lock(home)?;
    let mut inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let entry = inbox
        .get_mut(id)
        .context("persisted inbox event is missing")?;
    entry.processed = true;
    atomic_write(&inbox_path(home), &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(())
}
fn load_pending_inbox(home: &Path) -> Result<Vec<InboundEnvelope>> {
    let inbox: BTreeMap<String, InboxEntry> = read_json(inbox_path(home))?;
    let mut pending: Vec<_> = inbox
        .into_values()
        .filter(|entry| !entry.processed)
        .map(|entry| entry.event)
        .collect();
    pending.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then(left.id.cmp(&right.id))
    });
    Ok(pending)
}
fn load_outbox(home: &Path) -> Result<Vec<OutboundDelivery>> {
    read_json(outbox_path(home))
}
fn save_outbox(home: &Path, value: &[OutboundDelivery]) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(value)?).map_err(Into::into)
}
fn persist_outbox(home: &Path, delivery: OutboundDelivery) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut outbox = load_outbox(home)?;
    if let Some(existing) = outbox.iter_mut().find(|item| item.id == delivery.id) {
        if delivery.id.ends_with(":status") && existing.text != delivery.text {
            *existing = delivery;
            atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(&outbox)?)?;
        }
    } else {
        outbox.push(delivery);
        atomic_write(&outbox_path(home), &serde_json::to_vec_pretty(&outbox)?)?;
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("bound Slack thread snapshot {id} is unavailable")
        }
        Err(e) => Err(e.into()),
    }
}
fn save_thread(home: &Path, config: &RunnerConfig, thread: &FluentThread) -> Result<()> {
    ensure_root(home)?;
    let _lock = state_lock(home)?;
    let mut index = load_threads(home)?;
    let mut private_mapping = thread.clone();
    if private_mapping.project_label.is_some() {
        // Bound state is authoritative in the checkout.  The home index only
        // retains the mapping needed to locate that private runtime snapshot.
        private_mapping.provider_session = None;
        private_mapping.slice = None;
        private_mapping.planning_stage = None;
        private_mapping.linked_work = None;
        private_mapping.pending_question = None;
        private_mapping.processed_events.clear();
    }
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
    // Publish the private locator only after the authoritative bound snapshot
    // is durable. A crash can leave an unreferenced snapshot, never a locator
    // that resolves to stripped or missing state.
    index.insert(thread.id.clone(), private_mapping);
    atomic_write(
        &thread_index_path(home),
        &serde_json::to_vec_pretty(&index)?,
    )?;
    Ok(())
}

fn state_lock(home: &Path) -> Result<File> {
    ensure_root(home)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state_root(home).join("state.lock"))?;
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    if result != 0 {
        bail!("lock Slack collaboration state");
    }
    Ok(file)
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
            acknowledgement_id: id.into(),
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

    #[test]
    fn launch_agent_installs_the_runner_command() {
        let home = TempDir::new().unwrap();
        let mut service = LaunchAgentServiceManager::new(home.path());
        let executable = Path::new("/Applications/fluent");
        service.install(executable).unwrap();
        let plist = fs::read_to_string(&service.plist).unwrap();
        assert!(plist.contains("/Applications/fluent"));
        assert!(plist.contains("<string>slack</string><string>run</string>"));
        assert_eq!(
            LaunchAgentServiceManager::program_arguments(executable),
            ["/Applications/fluent", "slack", "run"]
        );
    }

    #[test]
    fn runner_restart_recovers_thread_and_reconnect_deduplicates_history() {
        let (home, _checkout, config) = setup();
        let start = event("E1", None, "@Fluent plan this");
        let mut first = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        first.ingest(start.clone()).unwrap();

        let mut recovered = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        recovered
            .reconnect(
                vec![start, event("E2", Some("1.0"), "continue")],
                "2026-08-17T00:00:00Z".into(),
            )
            .unwrap();
        assert_eq!(
            recovered.provider().calls.len(),
            1,
            "only the unprocessed reply reaches the recovered provider"
        );
        assert_eq!(
            recovered.provider().calls[0].0.as_deref(),
            Some("session-1")
        );
        let recovered_threads = load_threads(home.path()).unwrap();
        assert_eq!(
            recovered_threads["1.0"].last_connection.as_deref(),
            Some("2026-08-17T00:00:00Z")
        );
    }
}
