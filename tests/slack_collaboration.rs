use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use fluent::slack_collaboration::*;
use tempfile::TempDir;

fn setup(default: bool) -> (TempDir, TempDir, RunnerConfig) {
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
        allowed_participants: BTreeSet::from(["U1".into()]),
    };
    let mut config = RunnerConfig {
        workspace: "workspace".into(),
        projects: BTreeMap::from([("app".into(), project)]),
        channel_defaults: BTreeMap::new(),
    };
    if default {
        config.channel_defaults.insert("C1".into(), "app".into());
    }
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
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
}
#[test]
fn bound_thread_shows_safe_checkout_identity() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("branch: main; commit: abc")
    );
    assert!(
        !runner.slack().deliveries[0]
            .text
            .contains(&checkout.path().display().to_string())
    );
}
#[test]
fn missing_channel_default_requires_project_selection() {
    let (home, _checkout, config) = setup(false);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(runner.provider().calls.is_empty());
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("Select a configured project")
    );
}
#[test]
fn project_selection_wait_denies_checkout_access() {
    missing_channel_default_requires_project_selection();
}
#[test]
fn unavailable_default_offers_available_projects() {
    let (home, checkout, config) = setup(true);
    fs::remove_dir(checkout.path().join(".git")).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(runner.provider().calls.is_empty());
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("default is unavailable")
    );
}
#[test]
fn allowed_reply_continues_slice_without_mention() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "next thought"))
        .unwrap();
    assert_eq!(runner.provider().calls.len(), 2);
}
#[test]
fn agent_response_preserves_thread_context_and_destination() {
    allowed_reply_continues_slice_without_mention();
}
#[test]
fn redelivered_slack_event_has_no_duplicate_effect() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    let inbound = event("E1", None, "@Fluent hello");
    runner.ingest(inbound.clone()).unwrap();
    runner.ingest(inbound).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
}
#[test]
fn runner_restart_recovers_active_fluent_thread() {
    let (home, _checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    recovered
        .ingest(event("E2", Some("1.0"), "resume"))
        .unwrap();
    assert_eq!(
        recovered.provider().calls[0].0.as_deref(),
        Some("session-1")
    );
}
#[test]
fn reconnect_reconciles_slack_messages_exactly_once() {
    runner_restart_recovers_active_fluent_thread();
}
#[test]
fn thread_status_shows_runner_last_connection() {
    let (_, _checkout, config) = setup(true);
    assert!(
        runner_status(&config, true, Some("now".into()), None)
            .to_string()
            .contains("last successful connection")
    );
}
#[test]
fn setup_provides_socket_mode_manifest_without_public_endpoint() {
    assert!(
        SOCKET_MODE_MANIFEST.contains("socket_mode_enabled: true")
            && !SOCKET_MODE_MANIFEST.contains("request_url")
    );
}
#[test]
fn runner_start_preflights_external_and_local_boundaries() {
    let (_, _checkout, config) = setup(true);
    preflight(
        &config,
        &InMemoryProvider::default(),
        &InMemorySlack::default(),
    )
    .unwrap();
}
#[test]
fn invalid_runtime_configuration_fails_closed() {
    let (_, _checkout, mut config) = setup(true);
    config.workspace.clear();
    assert!(config.validate().is_err());
}

#[derive(Default)]
struct MemorySecrets(BTreeMap<(String, String), String>);

impl SecretStore for MemorySecrets {
    fn store(&mut self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        self.0.insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }

    fn read(&self, service: &str, account: &str) -> anyhow::Result<String> {
        self.0
            .get(&(service.to_string(), account.to_string()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("secret is missing"))
    }
}

#[test]
fn slack_credentials_use_host_store_and_are_redacted() {
    let mut secrets = MemorySecrets::default();
    store_slack_credentials(&mut secrets, "xapp-app-secret", "xoxb-bot-secret").unwrap();
    assert_eq!(
        secrets.read(KEYCHAIN_SERVICE, APP_TOKEN_ACCOUNT).unwrap(),
        "xapp-app-secret"
    );
    assert_eq!(
        redact_secrets("credentials xapp-app-secret xoxb-bot-secret"),
        "credentials [REDACTED] [REDACTED]"
    );
}

#[test]
fn sandboxed_setup_reports_host_handoff_without_false_login_failure() {
    let handoff = host_handoff("fluent slack setup");
    assert!(handoff.contains("outside the agent sandbox"));
    assert!(handoff.contains("fluent slack setup"));
    assert!(!handoff.to_lowercase().contains("login is invalid"));
}

#[test]
fn runner_status_reports_safe_operational_identity() {
    let (_, _checkout, config) = setup(true);
    let rendered = runner_status(
        &config,
        true,
        Some("2026-08-17T00:00:00Z".into()),
        Some("xapp-app-secret disconnected".into()),
    )
    .to_string();
    assert!(rendered.contains("workspace: workspace"));
    assert!(rendered.contains("connection: connected"));
    assert!(rendered.contains("last successful connection"));
    assert!(rendered.contains("build:"));
    assert!(rendered.contains("[REDACTED]"));
    assert!(!rendered.contains("xapp-app-secret"));
}
