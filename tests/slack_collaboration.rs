use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use fluent::slack_collaboration::*;
use tempfile::TempDir;

fn initialize_checkout(path: &std::path::Path) -> (String, String) {
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["config", "user.name", "Fluent test"],
    ] {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(path.join("README"), "fixture").unwrap();
    for args in [vec!["add", "README"], vec!["commit", "-m", "fixture"]] {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    (
        checkout_identity(path).unwrap(),
        String::from_utf8(commit.stdout).unwrap().trim().into(),
    )
}

fn setup(default: bool) -> (TempDir, TempDir, RunnerConfig) {
    let home = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(checkout.path());
    let project = ConfiguredProject {
        label: "app".into(),
        checkout: Checkout {
            label: "main".into(),
            path: checkout.path().into(),
            identity,
            branch: "main".into(),
            commit,
        },
        allowed_participants: BTreeSet::from(["U1".into()]),
        project_approvers: BTreeSet::from(["U1".into()]),
    };
    let mut config = RunnerConfig {
        workspace: "workspace".into(),
        projects: BTreeMap::from([("app".into(), project)]),
        channel_defaults: BTreeMap::new(),
        enabled_providers: BTreeSet::from(["codex".into()]),
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

fn pending_action(kind: PendingActionKind) -> PendingAction {
    PendingAction {
        id: "approve-plan".into(),
        kind,
        artifact_revision: "revision-1".into(),
        presented_at: "now".into(),
        labels: BTreeSet::from(["approve".into()]),
    }
}

#[test]
fn unauthorized_mention_receives_private_refusal() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config, InMemoryProvider::default(), InMemorySlack::default());
    let mut request = event("E1", None, "@Fluent hello");
    request.user = "U9".into();
    runner.ingest(request).unwrap();
    assert_eq!(runner.slack().private_responses.len(), 1);
    assert!(runner.slack().private_responses[0].2.contains("not allowed"));
}

#[test]
fn unauthorized_mention_preserves_state_and_hides_projects() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    let mut request = event("E1", None, "@Fluent hello");
    request.user = "U9".into();
    runner.ingest(request).unwrap();
    assert!(FileCollaborationStore::new(home.path()).load_thread(&config, "1.0").unwrap().is_none());
    assert!(!runner.slack().private_responses[0].2.contains("app"));
}

#[test]
fn project_approver_advances_exact_requested_action_once() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config, InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    let first = runner.approve_pending_action("1.0", "U1", "approve-plan", "revision-1", "approval-event", "then").unwrap();
    let duplicate = runner.approve_pending_action("1.0", "U1", "approve-plan", "revision-1", "approval-event", "later").unwrap();
    assert_eq!(first, duplicate);
    let thread = FileCollaborationStore::new(home.path()).load_thread(&runner.config, "1.0").unwrap().unwrap();
    assert_eq!(thread.approvals.len(), 1);
    assert!(thread.pending_action.is_none());
}

#[test]
fn participant_cannot_advance_approval_controlled_action() {
    let (home, _checkout, mut config) = setup(true);
    config.projects.get_mut("app").unwrap().allowed_participants.insert("U2".into());
    let mut runner = CollaborationRunner::new(home.path(), config, InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    assert_eq!(runner.approve_pending_action("1.0", "U2", "approve-plan", "revision-1", "E2", "now").unwrap(), ApprovalOutcome::WaitingForApprover);
}

#[test]
fn stale_approval_cannot_advance_revised_action() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config, InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    assert_eq!(runner.approve_pending_action("1.0", "U1", "approve-plan", "old", "E2", "now").unwrap(), ApprovalOutcome::Stale { current_action_id: "approve-plan".into() });
}

#[test]
fn approval_uses_current_project_authority() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    config.projects.get_mut("app").unwrap().project_approvers = BTreeSet::from(["U2".into()]);
    config.projects.get_mut("app").unwrap().allowed_participants.insert("U2".into());
    write_config(home.path(), &config).unwrap();
    assert_eq!(runner.approve_pending_action("1.0", "U1", "approve-plan", "revision-1", "E2", "now").unwrap(), ApprovalOutcome::WaitingForApprover);
}

#[test]
fn conversational_approval_targets_single_current_question() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    runner.ingest(event("E2", Some("1.0"), "y")).unwrap();
    let thread = FileCollaborationStore::new(home.path()).load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(thread.approvals.len(), 1);
}

#[test]
fn non_approver_gets_private_explanation_while_thread_waits() {
    let (home, _checkout, mut config) = setup(true);
    config.projects.get_mut("app").unwrap().allowed_participants.insert("U2".into());
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    let mut reply = event("E2", Some("1.0"), "y"); reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    assert_eq!(runner.slack().private_responses.len(), 1);
    assert!(FileCollaborationStore::new(home.path()).load_thread(&config, "1.0").unwrap().unwrap().pending_action.is_some());
}

#[test]
fn reconnect_applies_still_current_planning_approval() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Planning)).unwrap();
    runner.reconnect([event("E2", Some("1.0"), "y")], "reconnected".into()).unwrap();
    assert_eq!(FileCollaborationStore::new(home.path()).load_thread(&config, "1.0").unwrap().unwrap().approvals.len(), 1);
}

#[test]
fn reconnect_reconfirms_delayed_consequential_action() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner.request_approval("1.0", pending_action(PendingActionKind::Landing)).unwrap();
    runner.reconnect([event("E2", Some("1.0"), "y")], "reconnected".into()).unwrap();
    let thread = FileCollaborationStore::new(home.path()).load_thread(&config, "1.0").unwrap().unwrap();
    assert!(thread.approvals.is_empty());
    assert!(thread.pending_question.as_deref().unwrap().contains("fresh confirmation"));
    assert!(runner.slack().deliveries.iter().any(|delivery| delivery.text.contains("Fresh confirmation")));
}

#[test]
fn setup_validates_complete_collaboration_authority() {
    let (_home, _checkout, mut config) = setup(true);
    config.enabled_providers.clear();
    assert!(config.validate().unwrap_err().to_string().contains("conversation provider"));
    config.enabled_providers.insert("codex".into());
    config.projects.get_mut("app").unwrap().project_approvers.insert("U9".into());
    assert!(config.validate().unwrap_err().to_string().contains("participant or approver"));
}

#[test]
fn participant_revocation_applies_to_active_and_future_threads() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(home.path(), config.clone(), InMemoryProvider::default(), InMemorySlack::default());
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    config.projects.get_mut("app").unwrap().allowed_participants.clear();
    config.projects.get_mut("app").unwrap().project_approvers.clear();
    // A disabled project policy is invalid, so replace it with another allowed actor.
    config.projects.get_mut("app").unwrap().allowed_participants.insert("U2".into());
    config.projects.get_mut("app").unwrap().project_approvers.insert("U2".into());
    write_config(home.path(), &config).unwrap();
    runner.ingest(event("E2", Some("1.0"), "continue")).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
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
fn socket_mode_transport_connects_receives_and_acknowledges() {
    let websocket_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let websocket_address = websocket_listener.local_addr().unwrap();
    let websocket = std::thread::spawn(move || {
        let (stream, _) = websocket_listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        socket
            .send(tungstenite::Message::Text(
                r#"{"envelope_id":"ENV1","payload":{"event_id":"EV1","event":{"channel":"C1","ts":"1.0","user":"U1","text":"@Fluent hello"}}}"#
                    .into(),
            ))
            .unwrap();
        socket.read().unwrap().into_text().unwrap()
    });

    let api_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_address = api_listener.local_addr().unwrap();
    let authorization = Arc::new(Mutex::new(String::new()));
    let captured = authorization.clone();
    let api = std::thread::spawn(move || {
        let (mut stream, _) = api_listener.accept().unwrap();
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        *captured.lock().unwrap() = String::from_utf8_lossy(&buffer[..count]).into_owned();
        let body = format!(r#"{{"ok":true,"url":"ws://{websocket_address}"}}"#);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let mut transport = SocketModeTransport::connect(
        format!("http://{api_address}"),
        "xapp-contract".into(),
        "xoxb-contract".into(),
    )
    .unwrap();
    let envelope = transport.receive().unwrap();
    assert_eq!(envelope.id, "EV1");
    assert_eq!(envelope.acknowledgement_id, "ENV1");
    transport.acknowledge(&envelope.acknowledgement_id).unwrap();

    api.join().unwrap();
    let acknowledgement = websocket.join().unwrap();
    assert!(
        authorization
            .lock()
            .unwrap()
            .contains("Bearer xapp-contract")
    );
    assert_eq!(acknowledgement, r#"{"envelope_id":"ENV1"}"#);
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
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or_default();
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8(request).unwrap());
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
                .unwrap();
        }
    });
    let mut slack = SlackWebApiTransport::new(
        format!("http://{address}"),
        "xapp-test".into(),
        "xoxb-test".into(),
    );
    let delivery = OutboundDelivery {
        id: "1.0:status".into(),
        channel: "C1".into(),
        thread_timestamp: "1.0".into(),
        text: "connected at first".into(),
        delivered: false,
    };
    slack.post(&delivery).unwrap();
    slack.post(&delivery).unwrap();
    slack
        .post(&OutboundDelivery {
            text: "connected at second".into(),
            ..delivery
        })
        .unwrap();
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    let request = &requests[0];
    assert!(request.contains("POST /chat.postMessage"));
    assert!(request.contains("Authorization: Bearer xoxb-test"));
    assert!(request.contains("\"thread_ts\":\"1.0\""));
    let client_message_ids: Vec<_> = requests
        .iter()
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            body["client_msg_id"].as_str().unwrap().to_owned()
        })
        .collect();
    let client_message_id = &client_message_ids[0];
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
    assert_eq!(client_message_ids[0], client_message_ids[1]);
    assert_ne!(client_message_ids[1], client_message_ids[2]);
}

const CODEX_APP_SERVER_FIXTURE: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

if "--help" in sys.argv:
    sys.exit(0)

state_path = sys.argv[0] + ".state"
log_path = sys.argv[0] + ".log"
environment_path = sys.argv[0] + ".environment"

with open(environment_path, "a") as output:
    output.write(json.dumps({
        "HOME": os.environ.get("HOME"),
        "CODEX_HOME": os.environ.get("CODEX_HOME")
    }) + "\n")

def load_state():
    if os.path.exists(state_path):
        with open(state_path) as source:
            return json.load(source)
    return {"thread_id": "thread-1", "turns": []}

def save_state(state):
    with open(state_path, "w") as output:
        json.dump(state, output)

def send(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    with open(log_path, "a") as output:
        output.write(json.dumps(request) + "\n")
    method = request.get("method")
    if "id" not in request:
        continue
    request_id = request["id"]
    state = load_state()
    if method == "initialize":
        result = {"userAgent": "codex-fixture"}
    elif method == "thread/start":
        result = {"thread": {"id": state["thread_id"]}}
        save_state(state)
    elif method == "thread/resume":
        result = {"thread": {"id": state["thread_id"], "turns": state["turns"]}}
    elif method == "thread/read":
        result = {"thread": {"id": state["thread_id"], "turns": state["turns"]}}
    elif method == "turn/start":
        action_id = request["params"]["clientUserMessageId"]
        held = os.path.exists(sys.argv[0] + ".hold")
        turn = {
            "id": "turn-" + action_id,
            "status": "inProgress" if held else "completed",
            "items": [
                {"id": "user-1", "type": "userMessage", "clientId": action_id,
                 "content": request["params"]["input"]}
            ]
        }
        if not held:
            turn["items"].append(
                {"id": "agent-1", "type": "agentMessage", "text": "provider reply"})
        state["turns"].append(turn)
        save_state(state)
        result = {"turn": {"id": turn["id"], "status": "inProgress", "items": []}}
        send({"jsonrpc": "2.0", "id": request_id, "result": result})
        if held:
            continue
        send({"jsonrpc": "2.0", "method": "item/completed", "params": {
            "threadId": state["thread_id"], "turnId": turn["id"],
            "completedAtMs": 1, "item": turn["items"][1]}})
        send({"jsonrpc": "2.0", "method": "turn/completed", "params": {
            "threadId": state["thread_id"], "turn": turn}})
        continue
    else:
        send({"jsonrpc": "2.0", "id": request_id,
              "error": {"code": -32601, "message": "unsupported"}})
        continue
    send({"jsonrpc": "2.0", "id": request_id, "result": result})
"#;

fn codex_fixture(fixture: &TempDir) -> std::path::PathBuf {
    let executable = fixture.path().join("codex-fixture");
    fs::write(&executable, CODEX_APP_SERVER_FIXTURE).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    executable
}

fn codex_requests(executable: &std::path::Path) -> Vec<serde_json::Value> {
    fs::read_to_string(format!("{}.log", executable.display()))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn codex_environments(executable: &std::path::Path) -> Vec<serde_json::Value> {
    fs::read_to_string(format!("{}.environment", executable.display()))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn transparent_sandbox_launcher(fixture: &TempDir) -> std::path::PathBuf {
    let launcher = fixture.path().join("sandbox-exec");
    fs::write(
        &launcher,
        "#!/bin/sh\nset -eu\n[ \"$1\" = \"-f\" ]\nshift 2\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    launcher
}

#[test]
fn codex_app_server_provider_exchanges_a_stable_turn() {
    let fixture = TempDir::new().unwrap();
    let executable = codex_fixture(&fixture);
    let checkout = TempDir::new().unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };
    let mut provider = CodexAppServerProvider::new(executable.clone());
    provider.ready().unwrap();
    let (session, reply) = provider.start("action-1", &scope, "hello", "U1").unwrap();
    assert_eq!(session, "thread-1");
    assert_eq!(reply, "provider reply");
    let requests = codex_requests(&executable);
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|request| request["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/start",
            "initialize",
            "initialized",
            "thread/resume",
            "turn/start"
        ]
    );
    let turn = requests
        .iter()
        .find(|request| request["method"] == "turn/start")
        .unwrap();
    assert_eq!(turn["params"]["clientUserMessageId"], "action-1");
    assert_eq!(turn["params"]["threadId"], "thread-1");
}

#[test]
fn scoped_codex_provider_accesses_checkout_below_runner_home() {
    let fixture = TempDir::new().unwrap();
    let executable = codex_fixture(&fixture);
    let runner_home = TempDir::new().unwrap();
    let checkout = runner_home.path().join("Workspace/project");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("README"), "fixture").unwrap();
    let scope = ProviderScope {
        slice_draft: checkout.join(".fluent/work/slack/drafts/test"),
        checkout,
    };
    let mut provider = CodexAppServerProvider::new_scoped_with_test_launcher(
        executable.clone(),
        transparent_sandbox_launcher(&fixture),
    );
    let profile = provider.render_scope_profile(&scope).unwrap().unwrap();
    let rendered = fs::read_to_string(profile.path).unwrap();
    let checkout = scope.checkout.display();
    let private_home = scope.slice_draft.join(".provider-home");
    let codex_home = scope.slice_draft.join(".codex-home");

    assert!(rendered.contains(&format!("(allow file-read*  (subpath \"{checkout}\"))")));
    assert!(rendered.contains(&format!(
        "(allow file-write* (subpath \"{}\"))",
        scope.slice_draft.display()
    )));
    assert!(rendered.contains(&private_home.display().to_string()));
    assert!(rendered.contains(&codex_home.display().to_string()));
    assert!(!rendered.contains(&format!(
        "(allow file-read* (literal \"{}\"))",
        runner_home.path().display()
    )));
    for broad_grant in [
        "(allow file-read* (subpath \"/private/var\"))",
        "(allow file-read* (subpath \"/private/tmp\"))",
        "(allow file-read* (subpath \"/var\"))",
        "(allow file-read* (subpath \"/tmp\"))",
        "(allow file-read*  (subpath \"/private/var/folders\"))",
        "(allow file-write* (subpath \"/private/var/folders\"))",
        "(allow file-write* (subpath \"/private/tmp\"))",
        "(allow file-read* (subpath \"/Volumes\"))",
    ] {
        assert!(!rendered.contains(broad_grant), "found {broad_grant}");
    }

    provider
        .start("scoped-action", &scope, "inspect the checkout", "U1")
        .unwrap();
    let observed = codex_environments(&executable);
    assert_eq!(observed.len(), 2);
    for environment in observed {
        assert_eq!(environment["HOME"], private_home.display().to_string());
        assert_eq!(environment["CODEX_HOME"], codex_home.display().to_string());
    }
}

#[test]
fn pure_collaboration_domain_has_no_filesystem_authority() {
    let domain = fs::read_to_string("src/slack_collaboration/domain.rs").unwrap();

    assert!(!domain.contains("std::path"));
    assert!(!domain.contains("PathBuf"));
    assert!(!domain.contains("ProviderScope"));
    assert!(!domain.contains("pub struct Checkout"));
}

fn assert_codex_turn_recovers_without_resubmission(crash: ThreadCommitCrashPoint) {
    let fixture = TempDir::new().unwrap();
    let executable = codex_fixture(&fixture);
    if crash == ThreadCommitCrashPoint::AfterProviderAccepted {
        fs::write(format!("{}.hold", executable.display()), "").unwrap();
    }
    let (home, _checkout, config) = setup(true);
    let inbound = event("E1", None, "@Fluent settle this turn");
    let mut first = CollaborationRunner::with_store(
        home.path(),
        config.clone(),
        CodexAppServerProvider::new(executable.clone()),
        InMemorySlack::default(),
        FileCollaborationStore::with_thread_commit_crash(home.path(), crash),
    );
    assert!(first.ingest(inbound.clone()).is_err());
    assert!(first.slack().deliveries.is_empty());
    let persisted = FileCollaborationStore::new(home.path())
        .load_thread(&config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(persisted.provider_session.as_deref(), Some("thread-1"));
    let pending = persisted.pending_turn.unwrap();
    assert_eq!(pending.provider_turn_id.as_deref(), Some("turn-E1"));
    assert_eq!(
        pending.completed_reply.is_some(),
        crash == ThreadCommitCrashPoint::AfterProviderCompleted
    );
    assert_eq!(
        codex_requests(&executable)
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config.clone(),
        CodexAppServerProvider::new(executable.clone()),
        InMemorySlack::default(),
    );
    let recovery = recovered.reconnect([inbound.clone()], "recovered".into());
    if crash == ThreadCommitCrashPoint::AfterProviderAccepted {
        assert!(recovery.is_err());
        assert!(recovered.slack().deliveries.is_empty());
        let state_path = format!("{}.state", executable.display());
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["turns"][0]["status"] = "completed".into();
        state["turns"][0]["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "agent-1", "type": "agentMessage", "text": "provider reply"
            }));
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        recovered = CollaborationRunner::new(
            home.path(),
            config,
            CodexAppServerProvider::new(executable.clone()),
            InMemorySlack::default(),
        );
        recovered.reconnect([inbound], "completed".into()).unwrap();
    } else {
        recovery.unwrap();
    }
    let requests = codex_requests(&executable);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );
    if crash == ThreadCommitCrashPoint::AfterProviderAccepted {
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "thread/read")
        );
    }
    assert_eq!(
        recovered
            .slack()
            .deliveries
            .iter()
            .filter(|delivery| delivery.id == "E1:reply")
            .count(),
        1
    );
}

#[test]
fn codex_turn_recovers_after_provider_acceptance() {
    assert_codex_turn_recovers_without_resubmission(ThreadCommitCrashPoint::AfterProviderAccepted);
}

#[test]
fn codex_turn_recovers_after_provider_completion() {
    assert_codex_turn_recovers_without_resubmission(ThreadCommitCrashPoint::AfterProviderCompleted);
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
    let expected = format!(
        "branch: main; commit: {}",
        config.projects["app"].checkout.commit
    );
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(runner.slack().deliveries[0].text.contains(&expected));
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
    targets: Vec<RecoveryTarget>,
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
        threads: &[RecoveryTarget],
    ) -> anyhow::Result<Vec<InboundEnvelope>> {
        self.targets = threads.to_vec();
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
    assert_eq!(recovered.slack().targets[0].oldest, "1.0");
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .recovery_cursor("C1", "1.0")
            .unwrap()
            .as_deref(),
        Some("2.0")
    );
}

#[test]
fn slack_history_paginates_after_the_durable_high_water_mark() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let server = std::thread::spawn(move || {
        for page in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or_default();
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8(request).unwrap());
            let range = if page == 0 { 1..=100 } else { 101..=125 };
            let messages: Vec<_> = range
                .map(|index| {
                    serde_json::json!({
                        "ts": format!("2.{index:06}"),
                        "user": "U1",
                        "text": format!("reply {index}")
                    })
                })
                .collect();
            let next_cursor = if page == 0 { "page-2" } else { "" };
            let body = serde_json::json!({
                "ok": true,
                "messages": messages,
                "response_metadata": {"next_cursor": next_cursor}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    let slack = SlackWebApiTransport::new(
        format!("http://{address}"),
        "xapp-test".into(),
        "xoxb-test".into(),
    );
    let events = slack
        .recover_history(&[RecoveryTarget {
            channel: "C1".into(),
            thread_timestamp: "1.0".into(),
            oldest: "1.500000".into(),
        }])
        .unwrap();
    server.join().unwrap();

    assert_eq!(events.len(), 125);
    assert_eq!(events.first().unwrap().timestamp, "2.000001");
    assert_eq!(events.last().unwrap().timestamp, "2.000125");
    let requests = requests.lock().unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap())
        .collect();
    assert_eq!(bodies[0]["oldest"], "1.500000");
    assert!(bodies[0].get("cursor").is_none());
    assert_eq!(bodies[1]["cursor"], "page-2");
}

#[derive(Clone)]
struct FailContributionOnceProvider {
    failed: Arc<Mutex<bool>>,
    completed: Arc<Mutex<Vec<String>>>,
}

impl ConversationProvider for FailContributionOnceProvider {
    fn start(
        &mut self,
        _action_id: &str,
        _scope: &ProviderScope,
        contribution: &str,
        _actor: &str,
    ) -> anyhow::Result<(String, String)> {
        Ok(("session-1".into(), format!("reply: {contribution}")))
    }

    fn resume(
        &mut self,
        _action_id: &str,
        _session: &str,
        _scope: &ProviderScope,
        contribution: &str,
        _actor: &str,
    ) -> anyhow::Result<String> {
        if contribution == "second" && !*self.failed.lock().unwrap() {
            *self.failed.lock().unwrap() = true;
            anyhow::bail!("crash midway through Slack recovery");
        }
        self.completed.lock().unwrap().push(contribution.into());
        Ok(format!("reply: {contribution}"))
    }
}

fn recovered_reply(id: &str, timestamp: &str, text: &str) -> InboundEnvelope {
    InboundEnvelope {
        id: id.into(),
        acknowledgement_id: String::new(),
        channel: "C1".into(),
        timestamp: timestamp.into(),
        thread_timestamp: Some("1.0".into()),
        user: "U1".into(),
        text: text.into(),
    }
}

#[test]
fn recovery_retains_the_last_safe_cursor_and_deduplicates_live_overlap() {
    let (home, _checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent start")).unwrap();

    let live = recovered_reply("EV-LIVE", "2.0", "first");
    let mut pending_live = live.clone();
    pending_live.acknowledgement_id = "ENV-LIVE".into();
    let mut disconnected = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        FailingSlack {
            fail_acknowledgements: 1,
            ..Default::default()
        },
    );
    assert!(disconnected.ingest(pending_live).is_err());

    let completed = Arc::new(Mutex::new(Vec::new()));
    let provider = FailContributionOnceProvider {
        failed: Arc::new(Mutex::new(false)),
        completed: completed.clone(),
    };
    let history = vec![
        recovered_reply("history:C1:2.0", "2.0", "first"),
        recovered_reply("history:C1:3.0", "3.0", "second"),
        recovered_reply("history:C1:4.0", "4.0", "third"),
    ];
    let mut crashed = CollaborationRunner::new(
        home.path(),
        config.clone(),
        provider.clone(),
        RecoverySlack {
            history: history.clone(),
            ..Default::default()
        },
    );
    assert!(reconcile_connected_runner(&mut crashed).is_err());
    let store = FileCollaborationStore::new(home.path());
    assert_eq!(
        store.recovery_cursor("C1", "1.0").unwrap().as_deref(),
        Some("2.0")
    );

    let mut restarted = CollaborationRunner::new(
        home.path(),
        config,
        provider,
        RecoverySlack {
            history: history.into_iter().skip(1).collect(),
            ..Default::default()
        },
    );
    reconcile_connected_runner(&mut restarted).unwrap();
    assert_eq!(restarted.slack().targets[0].oldest, "2.0");
    assert_eq!(
        store.recovery_cursor("C1", "1.0").unwrap().as_deref(),
        Some("4.0")
    );
    assert_eq!(&*completed.lock().unwrap(), &["first", "second", "third"]);
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
fn durable_outbox_identity_cannot_be_rewritten() {
    let (home, _checkout, _config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    let original = OutboundDelivery {
        id: "1.0:status:first".into(),
        channel: "C1".into(),
        thread_timestamp: "1.0".into(),
        text: "connected at first".into(),
        delivered: false,
    };
    store.persist_outbox(original.clone()).unwrap();
    let error = store
        .persist_outbox(OutboundDelivery {
            text: "connected at second".into(),
            ..original.clone()
        })
        .unwrap_err();
    assert!(error.to_string().contains("immutable"));
    assert_eq!(store.load_outbox().unwrap(), vec![original]);
}

#[test]
fn changed_checkout_identity_is_unavailable() {
    let (_home, checkout, mut config) = setup(true);
    let path = checkout.path();
    assert!(checkout_is_available(&config.projects["app"].checkout));
    config.projects.get_mut("app").unwrap().checkout.identity = "checkout-v1:different".into();
    assert!(!checkout_is_available(&config.projects["app"].checkout));
    config.projects.get_mut("app").unwrap().checkout.identity = checkout_identity(path).unwrap();
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
fn replacing_git_directory_invalidates_checkout_identity() {
    let (_home, checkout, config) = setup(true);
    let configured = &config.projects["app"].checkout;
    assert!(checkout_is_available(configured));
    fs::rename(
        checkout.path().join(".git"),
        checkout.path().join(".git-original"),
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success()
    );
    assert!(!checkout_is_available(configured));
}

#[test]
fn replacing_checkout_directory_invalidates_checkout_identity() {
    let (_home, checkout, config) = setup(true);
    let configured = &config.projects["app"].checkout;
    let replacement = checkout.path().with_extension("original");
    fs::rename(checkout.path(), &replacement).unwrap();
    fs::create_dir(checkout.path()).unwrap();
    initialize_checkout(checkout.path());
    assert!(!checkout_is_available(configured));
    fs::remove_dir_all(replacement).unwrap();
}

#[test]
fn git_inspection_errors_make_checkout_unavailable() {
    let (_home, checkout, config) = setup(true);
    let configured = &config.projects["app"].checkout;
    assert!(checkout_is_available(configured));
    fs::write(
        checkout.path().join(".git/HEAD"),
        "ref: refs/heads/missing\n",
    )
    .unwrap();
    assert!(!checkout_is_available(configured));
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
    assert!(mapping.last_connection.is_none());
}

#[test]
fn bound_payloads_are_persisted_only_in_project_runtime() {
    let (home, checkout, config) = setup(true);
    let mut thread = FluentThread {
        id: "1.0".into(),
        channel: "C1".into(),
        state: ThreadState::Active,
        project_label: Some("app".into()),
        provider_session: None,
        slice: None,
        planning_stage: None,
        linked_work: None,
        pending_question: None,
        pending_action: None,
        approvals: Vec::new(),
        processed_events: BTreeSet::from(["E1".into()]),
        last_connection: None,
        pending_turn: Some(PendingTurn {
            action_id: "E1".into(),
            contribution: "private contribution".into(),
            actor: "U1".into(),
            starts_session: true,
            provider_turn_id: None,
            completed_reply: None,
        }),
    };
    let store = FileCollaborationStore::new(home.path());
    store.save_thread(&config, &thread).unwrap();
    let owner_index = fs::read_to_string(state_root(home.path()).join("threads.json")).unwrap();
    assert!(!owner_index.contains("private contribution"));
    assert!(!owner_index.contains("\"pending_turn\": {"));
    let snapshot = checkout.path().join(".fluent/work/slack/threads/1.0.json");
    assert!(
        fs::read_to_string(snapshot)
            .unwrap()
            .contains("private contribution")
    );

    thread.pending_turn = None;
    thread.provider_session = Some("session-1".into());
    store.save_thread(&config, &thread).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E2", Some("1.0"), "another private contribution"))
        .unwrap();
    let owner_inbox = fs::read_to_string(state_root(home.path()).join("inbox.json")).unwrap();
    assert!(!owner_inbox.contains("another private contribution"));
    assert!(!owner_inbox.contains("\"user\": \"U1\""));
    let project_event = fs::read_dir(checkout.path().join(".fluent/work/slack/inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        fs::read_to_string(project_event)
            .unwrap()
            .contains("another private contribution")
    );
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
fn bound_thread_commit_recovers_both_partial_write_orders() {
    for crash_point in [
        ThreadCommitCrashPoint::AfterSnapshot,
        ThreadCommitCrashPoint::AfterIndex,
    ] {
        let (home, _checkout, config) = setup(true);
        let inbound = event("E1", None, "@Fluent bind this");
        let mut crashing = CollaborationRunner::with_store(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
            FileCollaborationStore::with_thread_commit_crash(home.path(), crash_point),
        );
        assert!(crashing.ingest(inbound.clone()).is_err());

        let mut recovered = CollaborationRunner::new(
            home.path(),
            config,
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        recovered.reconnect([inbound], "recovered".into()).unwrap();
        assert_eq!(recovered.provider().calls.len(), 1);
        assert!(
            recovered
                .slack()
                .deliveries
                .iter()
                .any(|delivery| delivery.text.contains("Bound to Project: app"))
        );
    }
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
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let waiting = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(waiting.state, ThreadState::WaitingForProject);
    assert!(waiting.project_label.is_none());
    runner.ingest(event("E2", Some("1.0"), "do work")).unwrap();
    let still_waiting = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(still_waiting.state, ThreadState::WaitingForProject);
    assert!(still_waiting.project_label.is_none());
    assert!(runner.provider().calls.is_empty());
    assert_eq!(runner.slack().deliveries.len(), 1);
}
#[test]
fn unavailable_default_offers_available_projects() {
    fn assert_fallback(mut break_default: impl FnMut(&mut ConfiguredProject)) {
        let (home, _checkout, mut config) = setup(true);
        let alternative = TempDir::new().unwrap();
        let (identity, commit) = initialize_checkout(alternative.path());
        let mut other = config.projects["app"].clone();
        other.label = "other".into();
        other.checkout.label = "other-checkout".into();
        other.checkout.path = alternative.path().into();
        other.checkout.identity = identity;
        other.checkout.commit = commit;
        config.projects.insert("other".into(), other);
        break_default(config.projects.get_mut("app").unwrap());
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

    assert_fallback(|project| project.checkout.identity = "checkout-v1:different".into());
    assert_fallback(|project| fs::remove_dir_all(project.checkout.path.join(".git")).unwrap());
}

#[test]
fn participant_cannot_start_or_continue_another_projects_thread() {
    let (home, _checkout, mut config) = setup(true);
    let other_checkout = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(other_checkout.path());
    config.projects.insert(
        "other".into(),
        ConfiguredProject {
            label: "other".into(),
            checkout: Checkout {
                label: "other-checkout".into(),
                path: other_checkout.path().into(),
                identity,
                branch: "main".into(),
                commit,
            },
            allowed_participants: BTreeSet::from(["U2".into()]),
            project_approvers: BTreeSet::from(["U2".into()]),
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
    let (home, _checkout, mut config) = setup(true);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut reply = event("E2", Some("1.0"), "next thought");
    reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    assert_eq!(runner.provider().calls.len(), 2);
    assert_eq!(runner.provider().calls[1].3, "U2");
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
    let deliveries = runner.slack().deliveries.clone();
    let outbox = FileCollaborationStore::new(home.path())
        .load_outbox()
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap();
    runner.ingest(inbound).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
    assert_eq!(runner.slack().deliveries, deliveries);
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_outbox()
            .unwrap(),
        outbox
    );
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap(),
        thread
    );
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
            .any(|d| d.id == "1.0:status:now" && d.text.contains("last connected at now"))
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|d| d.id == "1.0:status:now" && d.text.contains("connection was interrupted"))
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
        .find(|delivery| delivery.id.starts_with("1.0:status:"))
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
    let guide = fs::read_to_string("docs/slack-runner.md")
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        guide.contains("Socket Mode")
            && guide.contains("Keychain")
            && guide.contains("LaunchAgent")
            && guide.contains("mkdir -p ~/.config/fluent/slack")
            && guide.contains("Copy member ID")
            && guide.contains("channel ID (`C...`)")
            && guide.contains("/invite @Fluent")
            && guide.contains("@Fluent plan the next release")
            && guide.contains("keeps the thread unbound")
            && guide.contains("fluent slack checkout-identity")
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

    fn web_api_check(scopes: &str) -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let scopes = scopes.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            stream.read(&mut buffer).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-OAuth-Scopes: {scopes}\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{{\"ok\":true}}"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let result = SlackWebApiTransport::new(
            format!("http://{address}"),
            "xapp-test".into(),
            "xoxb-test".into(),
        )
        .check();
        server.join().unwrap();
        result
    }

    web_api_check("app_mentions:read,channels:history,chat:write").unwrap();
    let error = web_api_check("app_mentions:read,chat:write").unwrap_err();
    assert!(error.to_string().contains("channels:history"));
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

#[test]
fn slack_credentials_use_host_store_and_are_redacted() {
    let fake = TempDir::new().unwrap();
    let security = fake.path().join("security");
    fs::write(
        &security,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.log\"\n",
    )
    .unwrap();
    fs::set_permissions(&security, fs::Permissions::from_mode(0o755)).unwrap();
    let mut secrets = KeychainSecretStore::with_executable(&security);
    let app_token = "xapp-app-secret";
    let bot_token = "xoxb-bot-secret";
    store_slack_credentials(&mut secrets, app_token, bot_token).unwrap();

    let calls = fs::read_to_string(format!("{}.log", security.display())).unwrap();
    assert!(calls.contains(&format!(
        "add-generic-password -U -a {APP_TOKEN_ACCOUNT} -s {KEYCHAIN_SERVICE} -w {app_token}"
    )));
    assert!(calls.contains(&format!(
        "add-generic-password -U -a {BOT_TOKEN_ACCOUNT} -s {KEYCHAIN_SERVICE} -w {bot_token}"
    )));

    struct CredentialEchoProvider {
        app_token: String,
        bot_token: String,
    }
    impl ConversationProvider for CredentialEchoProvider {
        fn start(
            &mut self,
            _action_id: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> anyhow::Result<(String, String)> {
            Ok((
                "session-1".into(),
                format!("provider error: {} {}", self.app_token, self.bot_token),
            ))
        }

        fn resume(
            &mut self,
            _action_id: &str,
            _session: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> anyhow::Result<String> {
            unreachable!()
        }
    }

    fn assert_tree_excludes(root: &std::path::Path, tokens: &[&str]) {
        let mut paths = vec![root.to_path_buf()];
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                paths.extend(
                    fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            for token in tokens {
                assert!(
                    !bytes
                        .windows(token.len())
                        .any(|window| window == token.as_bytes()),
                    "credential leaked to {}",
                    path.display()
                );
            }
        }
    }

    let (home, checkout, config) = setup(true);
    write_config(home.path(), &config).unwrap();
    record_runner_failure(
        home.path(),
        &anyhow::anyhow!("connection failed with {app_token} and {bot_token}"),
    )
    .unwrap();
    let operational = read_runner_operational_state(home.path()).unwrap();
    let diagnostics = runner_status(
        &config,
        operational.connected,
        operational.last_successful_connection,
        operational.failure,
    )
    .to_string();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        CredentialEchoProvider {
            app_token: app_token.into(),
            bot_token: bot_token.into(),
        },
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent start")).unwrap();

    for token in [app_token, bot_token] {
        assert!(!diagnostics.contains(token));
        assert!(
            runner
                .slack()
                .deliveries
                .iter()
                .all(|delivery| !delivery.text.contains(token))
        );
    }
    assert!(diagnostics.contains("[REDACTED]"));
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.text.contains("[REDACTED]"))
    );
    assert_tree_excludes(home.path(), &[app_token, bot_token]);
    assert_tree_excludes(checkout.path(), &[app_token, bot_token]);
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
