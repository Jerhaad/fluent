use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

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
        acknowledgement_id: id.into(),
        channel: "C1".into(),
        timestamp: "1.0".into(),
        thread_timestamp: thread.map(str::to_owned),
        user: "U1".into(),
        text: text.into(),
    }
}

#[test]
fn socket_mode_envelope_preserves_event_and_acknowledgement_identities() {
    let envelope = decode_socket_mode_envelope(
        r#"{"envelope_id":"ENV1","payload":{"event_id":"EV1","event":{"channel":"C1","ts":"1.0","thread_ts":"0.9","user":"U1","text":"continue"}}}"#,
    )
    .unwrap();
    assert_eq!(envelope.id, "EV1");
    assert_eq!(envelope.acknowledgement_id, "ENV1");
    assert_eq!(envelope.thread_timestamp.as_deref(), Some("0.9"));
}

#[test]
fn socket_mode_envelope_rejects_missing_durable_identity() {
    let error = decode_socket_mode_envelope(
        r#"{"envelope_id":"ENV1","payload":{"event":{"channel":"C1","ts":"1.0","user":"U1"}}}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Slack event id"));
}

#[test]
fn slack_thread_identity_rejects_filesystem_aliases() {
    for thread_id in ["../outside", "..", ".", "/absolute", "1.0/../../outside"] {
        let envelope = event("EV1", Some(thread_id), "continue");
        let (home, _checkout, config) = setup(true);
        let mut runner = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        let error = runner.ingest(envelope.clone()).unwrap_err();
        assert!(error.to_string().contains("Slack thread identity"));
    }
    let (home, _checkout, config) = setup(true);
    let mut top_level = event("EV2", None, "@Fluent continue");
    top_level.timestamp = "../outside".into();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    assert!(
        runner
            .ingest(top_level)
            .unwrap_err()
            .to_string()
            .contains("Slack thread identity")
    );
}

#[test]
fn slack_web_api_uses_bot_token_and_same_thread_destination() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let request = Arc::new(Mutex::new(String::new()));
    let captured = request.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        *captured.lock().unwrap() = String::from_utf8_lossy(&buffer[..count]).into();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
            .unwrap();
    });
    let mut slack = SlackWebApiTransport::new(
        format!("http://{address}"),
        "xapp-test".into(),
        "xoxb-test".into(),
    );
    slack
        .post(&OutboundDelivery {
            id: "delivery-1".into(),
            channel: "C1".into(),
            thread_timestamp: "1.0".into(),
            text: "reply".into(),
            delivered: false,
        })
        .unwrap();
    server.join().unwrap();
    let request = request.lock().unwrap();
    assert!(request.contains("POST /chat.postMessage"));
    assert!(request.contains("Authorization: Bearer xoxb-test"));
    assert!(request.contains("\"thread_ts\":\"1.0\""));
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let client_message_id = body["client_msg_id"].as_str().unwrap();
    assert_eq!(client_message_id.len(), 36);
    assert_eq!(
        client_message_id
            .chars()
            .enumerate()
            .filter(|(_, character)| *character == '-')
            .map(|(index, _)| index)
            .collect::<Vec<_>>(),
        vec![8, 13, 18, 23]
    );
}

#[test]
fn codex_app_server_provider_exchanges_a_stable_turn() {
    let fixture = TempDir::new().unwrap();
    let executable = fixture.path().join("codex-fixture");
    fs::write(
        &executable,
        "#!/bin/sh\nif [ \"${2:-}\" = \"--help\" ]; then exit 0; fi\nrequest=$(cat)\ncase \"$request\" in *'\"action_id\":\"action-1\"'*) ;; *) exit 2;; esac\ncase \"$request\" in *'\"slice_draft\":'*) ;; *) exit 3;; esac\nprintf '%s' '{\"session\":\"session-1\",\"reply\":\"provider reply\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let checkout = TempDir::new().unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };
    let mut provider = CodexAppServerProvider::new(executable);
    provider.ready().unwrap();
    let (session, reply) = provider.start("action-1", &scope, "hello", "U1").unwrap();
    assert_eq!(session, "session-1");
    assert_eq!(reply, "provider reply");
}

#[test]
fn top_level_mention_uses_available_channel_default() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
    assert_eq!(runner.provider().calls[0].1, checkout.path());
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
    assert!(runner.slack().deliveries[0].text.contains("Project: app"));
    assert!(runner.slack().deliveries[0].text.contains("checkout: main"));
    assert!(
        !runner.slack().deliveries[0]
            .text
            .contains(&checkout.path().display().to_string())
    );
}

#[derive(Default)]
struct FailingSlack {
    fail_acknowledgements: usize,
    fail_posts: usize,
    acknowledgements: Vec<String>,
    deliveries: Vec<OutboundDelivery>,
}

struct CrashAfterAcceptSlack {
    accepted: Arc<Mutex<BTreeSet<String>>>,
    attempts: Arc<Mutex<Vec<String>>>,
    fail_after_accept: bool,
}

impl SlackTransport for CrashAfterAcceptSlack {
    fn acknowledge(&mut self, _event_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn post(&mut self, delivery: &OutboundDelivery) -> anyhow::Result<()> {
        self.attempts.lock().unwrap().push(delivery.id.clone());
        self.accepted.lock().unwrap().insert(delivery.id.clone());
        if self.fail_after_accept {
            self.fail_after_accept = false;
            anyhow::bail!("connection lost after Slack accepted delivery");
        }
        Ok(())
    }
}

#[derive(Default)]
struct FailSecondPostSlack {
    posts: usize,
}

impl SlackTransport for FailSecondPostSlack {
    fn acknowledge(&mut self, _event_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn post(&mut self, _delivery: &OutboundDelivery) -> anyhow::Result<()> {
        self.posts += 1;
        if self.posts == 2 {
            anyhow::bail!("second post failed");
        }
        Ok(())
    }
}

#[derive(Default)]
struct RecoverySlack {
    acknowledgements: Vec<String>,
    deliveries: Vec<OutboundDelivery>,
    history: Vec<InboundEnvelope>,
}

impl SlackTransport for RecoverySlack {
    fn acknowledge(&mut self, event_id: &str) -> anyhow::Result<()> {
        self.acknowledgements.push(event_id.into());
        Ok(())
    }

    fn post(&mut self, delivery: &OutboundDelivery) -> anyhow::Result<()> {
        self.deliveries.push(delivery.clone());
        Ok(())
    }

    fn bounded_history(
        &mut self,
        _threads: &[(String, String)],
    ) -> anyhow::Result<Vec<InboundEnvelope>> {
        Ok(std::mem::take(&mut self.history))
    }
}

impl SlackTransport for FailingSlack {
    fn acknowledge(&mut self, event_id: &str) -> anyhow::Result<()> {
        if self.fail_acknowledgements > 0 {
            self.fail_acknowledgements -= 1;
            anyhow::bail!("temporary acknowledgement failure");
        }
        self.acknowledgements.push(event_id.into());
        Ok(())
    }
    fn post(&mut self, delivery: &OutboundDelivery) -> anyhow::Result<()> {
        if self.fail_posts > 0 {
            self.fail_posts -= 1;
            anyhow::bail!("temporary post failure");
        }
        self.deliveries.push(delivery.clone());
        Ok(())
    }
}

#[derive(Clone, Default)]
struct CrashOnceProvider {
    actions: Arc<Mutex<BTreeMap<String, (String, String)>>>,
    fail_after_start: Arc<Mutex<bool>>,
}

impl ConversationProvider for CrashOnceProvider {
    fn start(
        &mut self,
        action_id: &str,
        _scope: &ProviderScope,
        contribution: &str,
        _actor: &str,
    ) -> anyhow::Result<(String, String)> {
        let mut actions = self.actions.lock().unwrap();
        let result = actions
            .entry(action_id.into())
            .or_insert_with(|| ("session-1".into(), format!("reply: {contribution}")))
            .clone();
        drop(actions);
        let mut fail = self.fail_after_start.lock().unwrap();
        if *fail {
            *fail = false;
            anyhow::bail!("simulated crash after provider side effect");
        }
        Ok(result)
    }

    fn resume(
        &mut self,
        action_id: &str,
        session: &str,
        _scope: &ProviderScope,
        contribution: &str,
        _actor: &str,
    ) -> anyhow::Result<String> {
        Ok(self
            .actions
            .lock()
            .unwrap()
            .entry(action_id.into())
            .or_insert_with(|| (session.into(), format!("reply: {contribution}")))
            .1
            .clone())
    }
}

#[test]
fn provider_turn_reuses_durable_action_after_crash() {
    let (home, _checkout, config) = setup(true);
    let provider = CrashOnceProvider {
        fail_after_start: Arc::new(Mutex::new(true)),
        ..Default::default()
    };
    let inbound = event("E1", None, "@Fluent keep this turn");
    let mut first = CollaborationRunner::new(
        home.path(),
        config.clone(),
        provider.clone(),
        InMemorySlack::default(),
    );
    assert!(first.ingest(inbound.clone()).is_err());
    assert_eq!(provider.actions.lock().unwrap().len(), 1);

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        provider.clone(),
        InMemorySlack::default(),
    );
    recovered.reconnect([inbound], "recovered".into()).unwrap();
    assert_eq!(provider.actions.lock().unwrap().len(), 1);
    assert_eq!(recovered.slack().deliveries.len(), 2);
}

#[test]
fn acknowledged_pending_event_replays_after_restart_once() {
    let (home, _checkout, config) = setup(true);
    let inbound = event("E1", None, "@Fluent recover this");
    let mut failed = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack {
            fail_acknowledgements: 1,
            ..Default::default()
        },
    );
    assert!(failed.ingest(inbound.clone()).is_err());
    assert!(failed.provider().calls.is_empty());

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        FailingSlack::default(),
    );
    recovered
        .reconnect([inbound.clone()], "recovered".into())
        .unwrap();
    recovered
        .reconnect([inbound], "recovered-again".into())
        .unwrap();
    assert_eq!(recovered.provider().calls.len(), 1);
    assert_eq!(
        recovered
            .slack()
            .deliveries
            .iter()
            .filter(|d| d.id == "E1:reply")
            .count(),
        1
    );
}

#[test]
fn configured_recovery_replays_pending_ingress_on_startup() {
    let (home, _checkout, config) = setup(true);
    let inbound = event("E1", None, "@Fluent recover this");
    let mut failed = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack {
            fail_acknowledgements: 1,
            ..Default::default()
        },
    );
    assert!(failed.ingest(inbound).is_err());

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        RecoverySlack::default(),
    );
    reconcile_connected_runner(&mut recovered).unwrap();

    assert_eq!(recovered.provider().calls.len(), 1);
    assert_eq!(recovered.slack().acknowledgements, ["E1"]);
    let status = read_runner_operational_state(home.path()).unwrap();
    assert!(status.connected);
    assert!(status.last_successful_connection.is_some());
}

#[test]
fn configured_recovery_reconciles_bounded_thread_history() {
    let (home, _checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent start")).unwrap();

    let mut historical = event("history:C1:2.0", Some("1.0"), "missed reply");
    historical.timestamp = "2.0".into();
    historical.acknowledgement_id.clear();
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        RecoverySlack {
            history: vec![historical],
            ..Default::default()
        },
    );
    reconcile_connected_runner(&mut recovered).unwrap();

    assert_eq!(recovered.provider().calls.len(), 1);
    assert_eq!(recovered.provider().calls[0].2, "missed reply");
    assert!(recovered.slack().acknowledgements.is_empty());
}

#[test]
fn persisted_outbound_reply_retries_without_second_provider_turn() {
    let (home, _checkout, config) = setup(true);
    let inbound = event("E1", None, "@Fluent deliver this");
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack {
            fail_posts: 1,
            ..Default::default()
        },
    );
    assert!(runner.ingest(inbound.clone()).is_err());
    assert_eq!(runner.provider().calls.len(), 1);
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        FailingSlack::default(),
    );
    recovered.reconnect([inbound], "recovered".into()).unwrap();
    assert!(recovered.provider().calls.is_empty());
    assert_eq!(
        recovered
            .slack()
            .deliveries
            .iter()
            .filter(|d| d.id == "E1:reply")
            .count(),
        1
    );
}

#[test]
fn accepted_outbound_reply_is_not_duplicated_after_crash() {
    let (home, _checkout, config) = setup(true);
    let accepted = Arc::new(Mutex::new(BTreeSet::new()));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let inbound = event("E1", None, "@Fluent deliver this");
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        CrashAfterAcceptSlack {
            accepted: accepted.clone(),
            attempts: attempts.clone(),
            fail_after_accept: true,
        },
    );
    assert!(runner.ingest(inbound.clone()).is_err());

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        CrashAfterAcceptSlack {
            accepted: accepted.clone(),
            attempts: attempts.clone(),
            fail_after_accept: false,
        },
    );
    recovered.reconnect([inbound], "recovered".into()).unwrap();

    assert_eq!(
        accepted
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == "E1:reply")
            .count(),
        1
    );
    assert_eq!(
        attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == "E1:reply")
            .count(),
        2,
        "recovery retries the stable identity while Slack accepts it once"
    );
}

#[test]
fn outbound_progress_is_persisted_after_each_accepted_delivery() {
    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    for id in ["delivery-1", "delivery-2"] {
        store
            .persist_outbox(OutboundDelivery {
                id: id.into(),
                channel: "C1".into(),
                thread_timestamp: "1.0".into(),
                text: id.into(),
                delivered: false,
            })
            .unwrap();
    }
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        FailSecondPostSlack::default(),
    );
    assert!(runner.deliver_pending().is_err());

    let deliveries = FileCollaborationStore::new(home.path())
        .load_outbox()
        .unwrap();
    assert!(deliveries[0].delivered);
    assert!(!deliveries[1].delivered);
}

#[test]
fn changed_checkout_identity_is_unavailable() {
    let (_home, checkout, mut config) = setup(true);
    let path = checkout.path();
    assert!(
        std::process::Command::new("git")
            .args(["-C", &path.display().to_string(), "init", "-b", "main"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "-C",
                &path.display().to_string(),
                "config",
                "user.email",
                "test@example.invalid"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "-C",
                &path.display().to_string(),
                "config",
                "user.name",
                "Fluent test"
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::write(path.join("README"), "fixture").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-C", &path.display().to_string(), "add", "README"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["-C", &path.display().to_string(), "commit", "-m", "fixture"])
            .status()
            .unwrap()
            .success()
    );
    let commit = String::from_utf8(
        std::process::Command::new("git")
            .args(["-C", &path.display().to_string(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    config.projects.get_mut("app").unwrap().checkout.commit = commit.trim().into();
    assert!(checkout_is_available(&config.projects["app"].checkout));
    config.projects.get_mut("app").unwrap().checkout.commit = "different".into();
    assert!(!checkout_is_available(&config.projects["app"].checkout));
    assert!(
        preflight(
            &config,
            &InMemoryProvider::default(),
            &InMemorySlack::default(),
        )
        .is_err()
    );
    fs::remove_dir_all(path.join(".git")).unwrap();
    assert!(
        preflight(
            &config,
            &InMemoryProvider::default(),
            &InMemorySlack::default(),
        )
        .is_err()
    );
}

#[test]
fn bound_thread_home_index_excludes_active_snapshot_state() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let index: std::collections::BTreeMap<String, FluentThread> =
        serde_json::from_slice(&fs::read(state_root(home.path()).join("threads.json")).unwrap())
            .unwrap();
    let mapping = &index["1.0"];
    assert_eq!(mapping.project_label.as_deref(), Some("app"));
    assert!(mapping.provider_session.is_none());
    assert!(mapping.processed_events.is_empty());
}

#[test]
fn missing_bound_snapshot_fails_instead_of_loading_stripped_index_state() {
    let (home, checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent hello")).unwrap();
    fs::remove_file(checkout.path().join(".fluent/work/slack/threads/1.0.json")).unwrap();

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    let error = recovered
        .ingest(event("E2", Some("1.0"), "resume"))
        .unwrap_err();
    assert!(error.to_string().contains("snapshot 1.0 is unavailable"));
    assert!(recovered.provider().calls.is_empty());
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
    let (home, _checkout, config) = setup(false);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.ingest(event("E2", Some("1.0"), "do work")).unwrap();
    assert!(runner.provider().calls.is_empty());
    assert_eq!(runner.slack().deliveries.len(), 1);
}
#[test]
fn unavailable_default_offers_available_projects() {
    let (home, checkout, mut config) = setup(true);
    let alternative = TempDir::new().unwrap();
    fs::create_dir(alternative.path().join(".git")).unwrap();
    let mut other = config.projects["app"].clone();
    other.label = "other".into();
    other.checkout.label = "other-checkout".into();
    other.checkout.path = alternative.path().into();
    config.projects.insert("other".into(), other);
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
    assert!(runner.slack().deliveries[0].text.contains("other"));
}

#[test]
fn participant_cannot_start_or_continue_another_projects_thread() {
    let (home, _checkout, mut config) = setup(true);
    let other_checkout = TempDir::new().unwrap();
    fs::create_dir(other_checkout.path().join(".git")).unwrap();
    config.projects.insert(
        "other".into(),
        ConfiguredProject {
            label: "other".into(),
            checkout: Checkout {
                label: "other-checkout".into(),
                path: other_checkout.path().into(),
                identity: "other-id".into(),
                branch: "main".into(),
                commit: "def".into(),
            },
            allowed_participants: BTreeSet::from(["U2".into()]),
        },
    );
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );

    let mut unauthorized_start = event("E1", None, "@Fluent hello");
    unauthorized_start.user = "U2".into();
    runner.ingest(unauthorized_start).unwrap();
    assert!(runner.provider().calls.is_empty());

    runner.ingest(event("E2", None, "@Fluent hello")).unwrap();
    let mut unauthorized_reply = event("E3", Some("1.0"), "continue");
    unauthorized_reply.user = "U2".into();
    runner.ingest(unauthorized_reply).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
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
    let (home, checkout, config) = setup(true);
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
    let call = &runner.provider().calls[1];
    assert_eq!(call.0.as_deref(), Some("session-1"));
    assert_eq!(call.1, checkout.path());
    assert_eq!(call.3, "U1");
    assert_eq!(runner.slack().deliveries[1].thread_timestamp, "1.0");
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
    let (home, checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let snapshot = checkout.path().join(".fluent/work/slack/threads/1.0.json");
    let mut thread: FluentThread = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    thread.slice = Some("slice-7".into());
    thread.planning_stage = Some("approach".into());
    thread.linked_work = Some("work-7".into());
    thread.pending_question = Some("approve plan?".into());
    fs::write(&snapshot, serde_json::to_vec_pretty(&thread).unwrap()).unwrap();
    initial.slack_mut().fail_acknowledgements = 1;
    assert!(
        initial
            .ingest(event("E2", Some("1.0"), "resume after restart"))
            .is_err()
    );
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        RecoverySlack::default(),
    );
    reconcile_connected_runner(&mut recovered).unwrap();
    assert_eq!(
        recovered.provider().calls[0].0.as_deref(),
        Some("session-1")
    );
    let restored: FluentThread = serde_json::from_slice(&fs::read(snapshot).unwrap()).unwrap();
    assert_eq!(restored.slice.as_deref(), Some("slice-7"));
    assert_eq!(restored.planning_stage.as_deref(), Some("approach"));
    assert_eq!(restored.linked_work.as_deref(), Some("work-7"));
    assert_eq!(restored.pending_question.as_deref(), Some("approve plan?"));
    assert!(restored.processed_events.contains("E2"));
}
#[test]
fn reconnect_reconciles_slack_messages_exactly_once() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut later = event("E3", Some("1.0"), "later");
    later.timestamp = "3.0".into();
    let mut earlier = event("E2", Some("1.0"), "earlier");
    earlier.timestamp = "2.0".into();
    runner
        .reconnect([later.clone(), earlier.clone(), earlier], "now".into())
        .unwrap();
    assert_eq!(runner.provider().calls[1].2, "earlier");
    assert_eq!(runner.provider().calls[2].2, "later");
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|d| d.id == "1.0:status" && d.text.contains("last connected at now"))
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|d| d.id == "1.0:status" && d.text.contains("connection was interrupted"))
    );
    runner.reconnect([], "later".into()).unwrap();
    assert!(
        runner
            .slack()
            .deliveries
            .last()
            .unwrap()
            .text
            .contains("last connected at later")
    );
}
#[test]
fn thread_status_shows_runner_last_connection() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        RecoverySlack::default(),
    );
    reconcile_connected_runner(&mut runner).unwrap();
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let operational = read_runner_operational_state(home.path()).unwrap();
    let rendered = runner_status(
        &config,
        operational.connected,
        operational.last_successful_connection.clone(),
        operational.failure,
    )
    .to_string();
    assert!(rendered.contains("connection: connected"));
    assert!(rendered.contains("last successful connection"));
    let thread_status = runner
        .slack()
        .deliveries
        .iter()
        .find(|delivery| delivery.id == "1.0:status")
        .expect("active thread status delivery");
    assert!(
        thread_status
            .text
            .contains(operational.last_successful_connection.as_deref().unwrap())
    );
}
#[test]
fn setup_provides_socket_mode_manifest_without_public_endpoint() {
    assert!(
        SOCKET_MODE_MANIFEST.contains("socket_mode_enabled: true")
            && !SOCKET_MODE_MANIFEST.contains("request_url")
    );
    let guide = fs::read_to_string("docs/slack-runner.md").unwrap();
    assert!(
        guide.contains("Socket Mode")
            && guide.contains("Keychain")
            && guide.contains("LaunchAgent")
            && guide.contains("security add-generic-password -U -a app-token")
            && guide.contains("security add-generic-password -U -a bot-token")
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
fn coordinator_uses_pinned_store_and_scheduler_boundaries() {
    struct UnreachableScheduler;
    impl SchedulerClient for UnreachableScheduler {
        fn status(&self, request: &SchedulerRequest) -> anyhow::Result<SchedulerStatus> {
            assert_eq!(request.version, 1);
            assert_eq!(request.operation, "status");
            Ok(SchedulerStatus { reachable: false })
        }
    }

    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    store.save_recovery_cursor("C1", "1.0", "cursor-1").unwrap();
    assert_eq!(
        store.recovery_cursor("C1", "1.0").unwrap().as_deref(),
        Some("cursor-1")
    );
    let mut runner = CollaborationRunner::with_store(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
        store,
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();

    let error = preflight_with_scheduler(
        &config,
        &InMemoryProvider::default(),
        &InMemorySlack::default(),
        &UnreachableScheduler,
    )
    .unwrap_err();
    assert!(error.to_string().contains("scheduler boundary"));

    let action = HostAction {
        action: HostActionKind::Restart,
        artifact_revision: "build-1".into(),
        actor: "runner-owner".into(),
        idempotency_identity: "restart:build-1".into(),
    };
    assert_eq!(action.idempotency_identity, "restart:build-1");
}
#[test]
fn invalid_runtime_configuration_fails_closed() {
    let (home, _checkout, mut config) = setup(true);
    config.workspace.clear();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    assert!(runner.ingest(event("E1", None, "@Fluent hello")).is_err());
    assert!(runner.slack().acknowledgements.is_empty());
    assert!(runner.provider().calls.is_empty());
    assert!(runner.slack().deliveries.is_empty());
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
    let handoff = classify_host_boundary(
        anyhow::anyhow!("launchctl: Operation not permitted by sandbox"),
        "fluent slack enable",
    )
    .to_string();
    assert!(handoff.contains("outside the agent sandbox"));
    assert!(handoff.contains("fluent slack enable"));
    assert!(!handoff.to_lowercase().contains("login is invalid"));

    let authentication = classify_host_boundary(
        anyhow::anyhow!("Slack authentication failed"),
        "fluent slack run",
    );
    assert_eq!(authentication.to_string(), "Slack authentication failed");
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
