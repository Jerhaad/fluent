use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use fluent::slack_collaboration::*;
use fluent::work_model::{PlanningContext, WorkItem, WorkModelStore};
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
        default_provider: "codex".into(),
        enabled_providers: BTreeSet::from(["codex".into(), "claude".into()]),
        project_approvers: BTreeSet::from(["U1".into()]),
        github_remote: Some("https://github.com/acme/app".into()),
    };
    let mut config = RunnerConfig {
        workspace: "workspace".into(),
        projects: BTreeMap::from([("app".into(), project)]),
        channel_defaults: BTreeMap::new(),
        direct_message_defaults: BTreeMap::new(),
        enabled_providers: BTreeSet::from(["codex".into(), "claude".into()]),
    };
    if default {
        config.channel_defaults.insert("C1".into(), "app".into());
    }
    (home, checkout, config)
}

#[test]
fn planning_artifact_renders_exact_revision_in_thread() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let source = "# Brief\n\nUse `exact` Markdown.\n";
    let revision = runner
        .present_planning_artifact("1.0", "brief", source)
        .unwrap();
    let deliveries: Vec<_> = runner
        .slack()
        .deliveries
        .iter()
        .filter(|delivery| delivery.id.starts_with("1.0:artifact:"))
        .collect();
    assert_eq!(reassemble_artifact_parts(&deliveries).unwrap(), source);
    assert!(deliveries[0].text.contains(&revision.id));
}

#[test]
fn provider_planning_output_is_presented_for_approval() {
    #[derive(Default)]
    struct PlanningProvider;

    impl ConversationProvider for PlanningProvider {
        fn start(
            &mut self,
            _action_id: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session-1".into(),
                serde_json::json!({
                    "type": "planning_artifact",
                    "kind": "brief",
                    "markdown": "# Brief\n\nBuild the requested change.\n"
                })
                .to_string(),
            ))
        }

        fn resume(
            &mut self,
            _action_id: &str,
            _session: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<String> {
            unreachable!("the first planning turn starts a provider session")
        }
    }

    let (home, checkout, config) = setup(true);
    WorkModelStore::new(checkout.path())
        .create_work_item(&WorkItem::planned("work-1", "Plan from Slack"))
        .unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        PlanningProvider,
        InMemorySlack::default(),
    );

    runner
        .ingest(event(
            "E1",
            None,
            "@Fluent work-item:work-1 draft the Brief",
        ))
        .unwrap();

    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    let revision = thread.artifact_revisions.last().unwrap();
    assert_eq!(revision.kind, "brief");
    assert_eq!(
        revision.markdown,
        "# Brief\n\nBuild the requested change.\n"
    );
    assert_eq!(
        thread.pending_artifact_revision.as_deref(),
        Some(revision.id.as_str())
    );
    assert_eq!(
        thread.pending_question.as_deref(),
        Some(format!("approve brief revision {}", revision.id).as_str())
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.id.contains(":artifact:brief:"))
    );
    assert!(
        WorkModelStore::new(checkout.path())
            .read_work_item("work-1")
            .unwrap()
            .planning_context
            .is_none()
    );
}

#[test]
fn same_markdown_planning_stages_use_distinct_delivery_ids() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let markdown = "# Shared planning text\n";

    let brief = runner
        .present_planning_artifact("1.0", "brief", markdown)
        .unwrap();
    let behaviors = runner
        .present_planning_artifact("1.0", "behaviors", markdown)
        .unwrap();

    assert_ne!(brief.id, behaviors.id);
    let delivery_ids: BTreeSet<_> = runner
        .slack()
        .deliveries
        .iter()
        .filter(|delivery| delivery.id.starts_with("1.0:artifact:"))
        .map(|delivery| delivery.id.as_str())
        .collect();
    assert_eq!(delivery_ids.len(), 2);
}

#[test]
fn large_planning_artifact_preserves_exact_content_across_messages() {
    let source = format!(
        "{}🙂{}",
        "a".repeat(SLACK_MESSAGE_CONTENT_LIMIT - 2),
        "b".repeat(8)
    );
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let revision = runner
        .present_planning_artifact("1.0", "brief", &source)
        .unwrap();
    let deliveries: Vec<_> = runner
        .slack()
        .deliveries
        .iter()
        .filter(|delivery| delivery.id.starts_with("1.0:artifact:"))
        .collect();
    assert!(deliveries.len() > 1);
    assert!(
        deliveries
            .iter()
            .all(|delivery| delivery.text.len() <= SLACK_MESSAGE_CONTENT_LIMIT)
    );
    for (index, delivery) in deliveries.iter().enumerate() {
        assert!(delivery.text.starts_with(&format!(
            "Planning revision {} ({}/{})\n\n",
            revision.id,
            index + 1,
            deliveries.len()
        )));
    }
    assert_eq!(reassemble_artifact_parts(&deliveries).unwrap(), source);
}

#[test]
fn planning_artifact_waits_for_existing_approval_gate() {
    let (home, checkout, mut config) = setup(true);
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
    let work_item = WorkItem::planned("work-1", "Approve planning");
    WorkModelStore::new(checkout.path())
        .create_work_item(&work_item)
        .unwrap();
    let store = FileCollaborationStore::new(home.path());
    let mut thread = store.load_thread(&runner.config, "1.0").unwrap().unwrap();
    thread.linked_work = Some("work-1".into());
    store.save_thread(&runner.config, &thread).unwrap();
    for (stage, markdown) in [
        ("brief", "# Brief\n"),
        ("behaviors", "# Behaviors\n"),
        ("approach", "# Approach\n"),
    ] {
        let revision = runner
            .present_planning_artifact("1.0", stage, markdown)
            .unwrap();
        runner
            .approve_planning_revision("1.0", &revision.id, "U1")
            .unwrap();
    }
    let revision = runner
        .present_planning_artifact("1.0", "plan", "# Plan\n")
        .unwrap();
    let provider_calls_before_reply = runner.provider().calls.len();
    runner
        .ingest(event("E2", Some("1.0"), "continue planning"))
        .unwrap();
    assert_eq!(runner.provider().calls.len(), provider_calls_before_reply);
    assert_eq!(
        store
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .pending_artifact_revision
            .as_deref(),
        Some(revision.id.as_str())
    );
    assert!(
        runner
            .approve_planning_revision("1.0", "different", "U1")
            .is_err()
    );
    assert!(
        runner
            .approve_planning_revision("1.0", &revision.id, "U2")
            .is_err()
    );
    runner
        .approve_planning_revision("1.0", &revision.id, "U1")
        .unwrap();
    let thread = store.load_thread(&runner.config, "1.0").unwrap().unwrap();
    assert_eq!(thread.planning_stage.as_deref(), Some("plan"));
    assert!(thread.pending_question.is_none());
    assert_eq!(
        WorkModelStore::new(checkout.path())
            .read_work_item("work-1")
            .unwrap()
            .planning_context
            .unwrap()
            .plan
            .as_deref(),
        Some("# Plan\n")
    );
}

#[test]
fn planning_artifact_reloads_exact_revisions_for_all_stages() {
    let (home, checkout, config) = setup(true);
    let stages = [
        ("brief", "# Brief\n"),
        ("behaviors", "# Behaviors\n"),
        ("approach", "# Approach\n"),
        ("plan", "# Plan\n"),
    ];
    let work_item = WorkItem::planned("work-1", "Approve planning");
    WorkModelStore::new(checkout.path())
        .create_work_item(&work_item)
        .unwrap();
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut thread = store.load_thread(&runner.config, "1.0").unwrap().unwrap();
    thread.linked_work = Some("work-1".into());
    store.save_thread(&runner.config, &thread).unwrap();
    for (stage, markdown) in stages {
        let revision = runner
            .present_planning_artifact("1.0", stage, markdown)
            .unwrap();
        let reloaded = store.load_thread(&runner.config, "1.0").unwrap().unwrap();
        let stored = reloaded.artifact_revisions.last().unwrap();
        assert_eq!(stored.markdown, markdown);
        assert_eq!(stored.sha256, ArtifactRevision::new(stage, markdown).sha256);
        runner
            .approve_planning_revision("1.0", &revision.id, "U1")
            .unwrap();
        let approved = WorkModelStore::new(checkout.path())
            .read_work_item("work-1")
            .unwrap()
            .planning_context
            .unwrap();
        let approved_stage = match stage {
            "brief" => approved.brief,
            "behaviors" => approved.behaviors,
            "approach" => approved.approach,
            "plan" => approved.plan,
            _ => unreachable!(),
        };
        assert_eq!(approved_stage.as_deref(), Some(markdown));
    }
}

#[test]
fn slack_approval_reply_advances_the_bound_work_item() {
    let (home, checkout, config) = setup(true);
    WorkModelStore::new(checkout.path())
        .create_work_item(&WorkItem::planned("work-1", "Slack planning"))
        .unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan this work-item: work-1"))
        .unwrap();
    let revision = runner
        .present_planning_artifact("1.0", "brief", "# Brief\n")
        .unwrap();
    runner
        .ingest(event(
            "E2",
            Some("1.0"),
            &format!("approve {}", revision.id),
        ))
        .unwrap();
    let mut approved_work_item = WorkModelStore::new(checkout.path())
        .read_work_item("work-1")
        .unwrap();
    let context = approved_work_item.planning_context.as_ref().unwrap();
    assert_eq!(context.brief.as_deref(), Some("# Brief\n"));
    assert_eq!(context.approved_revisions["brief"].revision_id, revision.id);
    assert!(
        approved_work_item
            .clone()
            .add_initial_attempt("attempt-1")
            .is_ok()
    );
    approved_work_item.planning_context.as_mut().unwrap().brief = Some("# Revised\n".into());
    assert!(approved_work_item.add_initial_attempt("attempt-1").is_err());
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert!(thread.pending_artifact_revision.is_none());
}

#[test]
fn planning_approval_requires_order_and_an_unstarted_work_item() {
    let mut work_item = WorkItem::planned("work-1", "Slack planning");
    let behaviors = ArtifactRevision::new("behaviors", "# Behaviors\n");
    assert!(
        work_item
            .approve_planning_revision(&behaviors.kind, &behaviors.id, behaviors.markdown.clone())
            .is_err()
    );

    let brief = ArtifactRevision::new("brief", "# Brief\n");
    work_item
        .approve_planning_revision(&brief.kind, &brief.id, brief.markdown.clone())
        .unwrap();
    work_item.add_initial_attempt("attempt-1").unwrap();
    let revised_brief = ArtifactRevision::new("brief", "# Revised brief\n");
    assert!(
        work_item
            .approve_planning_revision(
                &revised_brief.kind,
                &revised_brief.id,
                revised_brief.markdown.clone(),
            )
            .is_err()
    );
}

#[test]
fn legacy_planning_context_preserves_its_implied_stage() {
    let mut work_item = WorkItem::planned("work-1", "Legacy planning");
    work_item.planning_context = Some(PlanningContext {
        brief: Some("# Brief\n".into()),
        behaviors: Some("# Behaviors\n".into()),
        approach: Some("# Approach\n".into()),
        plan: Some("# Plan\n".into()),
        ..Default::default()
    });
    let brief = ArtifactRevision::new("brief", "# Revised brief\n");
    assert!(
        work_item
            .approve_planning_revision(&brief.kind, &brief.id, brief.markdown.clone())
            .is_err()
    );
    let plan = ArtifactRevision::new("plan", "# Revised plan\n");
    assert!(
        work_item
            .approve_planning_revision(&plan.kind, &plan.id, plan.markdown.clone())
            .is_ok()
    );
}

#[test]
fn slack_planning_presentation_rejects_started_work_item() {
    let (home, checkout, config) = setup(true);
    let mut work_item = WorkItem::planned("work-1", "Started planning");
    work_item.add_initial_attempt("attempt-1").unwrap();
    WorkModelStore::new(checkout.path())
        .create_work_item(&work_item)
        .unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan this work-item: work-1"))
        .unwrap();
    assert!(
        runner
            .present_planning_artifact("1.0", "brief", "# Revised brief\n")
            .is_err()
    );
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .artifact_revisions
            .is_empty()
    );
}

#[test]
fn queued_message_edit_replaces_unprocessed_content() {
    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut thread = store.load_thread(&config, "1.0").unwrap().unwrap();
    let mut original = event("E-original", Some("1.0"), "before");
    original.timestamp = "2.0".into();
    thread.pending_turn = Some(PendingTurn {
        action_id: "E-original".into(),
        source_timestamp: "2.0".into(),
        contribution: "before".into(),
        actor: "U1".into(),
        starts_session: false,
        provider_turn_id: None,
        completed_reply: None,
        dispatch_state: PendingTurnDispatchState::Runnable,
        retry_at_unix_seconds: None,
        delivery_attempt: 0,
    });
    thread.processed_events.insert(original.id.clone());
    store.save_thread(&config, &thread).unwrap();
    let mut edit = event("E2", Some("1.0"), "after");
    edit.timestamp = "2.0".into();
    edit.change = InboundChange::Edit;
    runner.ingest(edit).unwrap();
    let reloaded = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(reloaded.pending_turn.unwrap().contribution, "after");

    let mut restarted = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    restarted.ingest(original).unwrap();
    assert_eq!(restarted.provider().calls[0].2, "after");
    let reloaded = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(
        reloaded.conversation_inputs.last().unwrap().event_id,
        "E-original"
    );
    assert_eq!(
        reloaded.conversation_inputs.last().unwrap().content,
        "after"
    );
}

#[test]
fn processed_message_edit_becomes_attributed_correction() {
    let (home, _checkout, mut config) = setup(true);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut edit = event("E2", Some("1.0"), "correction");
    edit.user = "U2".into();
    edit.change = InboundChange::Edit;
    runner.ingest(edit).unwrap();
    let reloaded = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(reloaded.conversation_inputs[0].content, "@Fluent hello");
    assert_eq!(
        reloaded.conversation_inputs[0].corrections[0].content,
        "correction"
    );
    assert_eq!(
        reloaded.conversation_inputs[0].corrections[0].event_id,
        "E2"
    );
    assert_eq!(reloaded.conversation_inputs[0].corrections[0].actor, "U2");
}

#[test]
fn processed_message_deletion_preserves_audited_context() {
    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut deletion = event("E2", Some("1.0"), "");
    deletion.change = InboundChange::Delete;
    runner.ingest(deletion).unwrap();
    let reloaded = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(reloaded.conversation_inputs[0].content, "@Fluent hello");
    assert!(reloaded.conversation_inputs[0].source_deleted);
}

#[test]
fn unauthorized_thread_reply_receives_private_refusal() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let deliveries_before_refusal = runner.slack().deliveries.clone();
    let mut reply = event("E2", Some("1.0"), "private thought");
    reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    assert_eq!(runner.slack().private_responses.len(), 1);
    assert_eq!(runner.slack().private_responses[0].1, "U2");
    assert_eq!(runner.slack().deliveries, deliveries_before_refusal);
}

#[test]
fn unauthorized_thread_reply_does_not_reach_agent() {
    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let before = store.load_thread(&config, "1.0").unwrap().unwrap();
    let calls = runner.provider().calls.len();
    let mut reply = event("E2", Some("1.0"), "private thought");
    reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    assert_eq!(runner.provider().calls.len(), calls);
    assert_eq!(store.load_thread(&config, "1.0").unwrap().unwrap(), before);
}

#[test]
fn unauthorized_thread_reply_does_not_enter_project_inbox() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let inbox = checkout.path().join(".fluent/work/slack/inbox");
    let before: Vec<_> = fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let mut reply = event("E2", Some("1.0"), "private thought");
    reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    let after: Vec<_> = fs::read_dir(inbox)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(after, before);
}

#[test]
fn direct_message_uses_same_fluent_thread_contract() {
    let (home, checkout, mut config) = setup(false);
    config
        .direct_message_defaults
        .insert("D1".into(), "app".into());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    let mut request = event("E1", None, "@Fluent plan this");
    request.channel = "D1".into();
    runner.ingest(request).unwrap();
    assert_eq!(runner.provider().calls[0].1, checkout.path());
    assert_eq!(runner.slack().deliveries[0].channel, "D1");
}

#[test]
fn single_workspace_runner_routes_multiple_projects_and_conversations() {
    let (home, _checkout, mut config) = setup(true);
    let other = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(other.path());
    config.projects.insert(
        "other".into(),
        ConfiguredProject {
            label: "other".into(),
            checkout: Checkout {
                label: "other-main".into(),
                path: other.path().into(),
                identity,
                branch: "main".into(),
                commit,
            },
            allowed_participants: BTreeSet::from(["U1".into()]),
            default_provider: "codex".into(),
            enabled_providers: BTreeSet::from(["codex".into()]),
            project_approvers: BTreeSet::from(["U1".into()]),
            github_remote: None,
        },
    );
    config.channel_defaults.insert("C2".into(), "other".into());
    config
        .direct_message_defaults
        .insert("D1".into(), "other".into());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent app work"))
        .unwrap();
    let mut second = event("E2", None, "@Fluent other work");
    second.channel = "C2".into();
    second.timestamp = "2.0".into();
    runner.ingest(second).unwrap();
    let mut dm = event("E3", None, "@Fluent dm work");
    dm.channel = "D1".into();
    dm.timestamp = "3.0".into();
    runner.ingest(dm).unwrap();
    assert_eq!(runner.provider().calls.len(), 3);
    assert_ne!(runner.provider().calls[0].1, runner.provider().calls[1].1);
    assert_eq!(runner.provider().calls[1].1, runner.provider().calls[2].1);
}

#[test]
fn project_change_preserves_binding_and_offers_linked_thread() {
    let (home, checkout, mut config) = setup(true);
    let other = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(other.path());
    config.projects.insert(
        "other".into(),
        ConfiguredProject {
            label: "other".into(),
            checkout: Checkout {
                label: "other-main".into(),
                path: other.path().into(),
                identity,
                branch: "main".into(),
                commit,
            },
            allowed_participants: BTreeSet::from(["U1".into()]),
            default_provider: "codex".into(),
            enabled_providers: BTreeSet::from(["codex".into()]),
            project_approvers: BTreeSet::from(["U1".into()]),
            github_remote: None,
        },
    );
    let store = FileCollaborationStore::new(home.path());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent app work"))
        .unwrap();
    let revision = runner
        .present_planning_artifact("1.0", "brief", "# Brief\n")
        .unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "project: other"))
        .unwrap();
    let thread = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(thread.project_label.as_deref(), Some("app"));
    assert_eq!(thread.linked_thread_intent.as_deref(), Some("other"));
    assert_eq!(
        thread.pending_artifact_revision.as_deref(),
        Some(revision.id.as_str())
    );
    assert_eq!(runner.provider().calls.len(), 1);
    assert!(
        runner
            .slack()
            .deliveries
            .last()
            .unwrap()
            .text
            .contains("linked Fluent thread")
    );
    assert!(
        !runner
            .slack()
            .deliveries
            .last()
            .unwrap()
            .text
            .contains(&checkout.path().display().to_string())
    );
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
        change: InboundChange::Message,
        approval_action_id: None,
        approval_artifact_revision: None,
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

fn approval_event(id: &str, text: &str, action: &PendingAction) -> InboundEnvelope {
    let mut event = event(id, Some("1.0"), text);
    event.text = "fluent_approval".into();
    event.approval_action_id = Some(action.id.clone());
    event.approval_artifact_revision = Some(action.artifact_revision.clone());
    event
}

#[test]
fn unrelated_interaction_cannot_authorize_an_action() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let action = pending_action(PendingActionKind::Planning);
    runner.request_approval("1.0", action.clone()).unwrap();
    let mut interaction = approval_event("E2", "ignored", &action);
    interaction.text = "other_control".into();
    runner.ingest(interaction).unwrap();
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .is_empty()
    );
}

#[test]
fn unauthorized_mention_receives_private_refusal() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    let mut request = event("E1", None, "@Fluent hello");
    request.user = "U9".into();
    runner.ingest(request).unwrap();
    assert_eq!(runner.slack().private_responses.len(), 1);
    assert!(
        runner.slack().private_responses[0]
            .2
            .contains("not allowed")
    );
}

#[test]
fn unauthorized_mention_preserves_state_and_hides_projects() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    let mut request = event("E1", None, "@Fluent hello");
    request.user = "U9".into();
    runner.ingest(request).unwrap();
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .is_none()
    );
    assert!(!runner.slack().private_responses[0].2.contains("app"));
}

#[test]
fn project_approver_advances_exact_requested_action_once() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    let first = runner
        .approve_pending_action(
            "1.0",
            "U1",
            "approve-plan",
            "revision-1",
            "approval-event",
            "then",
        )
        .unwrap();
    let duplicate = runner
        .approve_pending_action(
            "1.0",
            "U1",
            "approve-plan",
            "revision-1",
            "approval-event",
            "later",
        )
        .unwrap();
    let ApprovalOutcome::Accepted(record) = &first else {
        panic!("project approver should authorize the pending action");
    };
    assert_eq!(record.project_label, "app");
    assert_eq!(record.thread_id, "1.0");
    assert_eq!(record.pending_action_id, "approve-plan");
    assert_eq!(record.action_kind, PendingActionKind::Planning);
    assert_eq!(record.artifact_revision, "revision-1");
    assert_eq!(record.presented_at, "now");
    assert_eq!(record.actor, "U1");
    assert_eq!(record.accepted_at, "then");
    assert_eq!(record.event_id, "approval-event");
    assert_eq!(first, duplicate);
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.approvals.len(), 1);
    assert!(thread.pending_action.is_none());
}

#[test]
fn participant_cannot_advance_approval_controlled_action() {
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
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    assert_eq!(
        runner
            .approve_pending_action("1.0", "U2", "approve-plan", "revision-1", "E2", "now")
            .unwrap(),
        ApprovalOutcome::WaitingForApprover
    );
}

#[test]
fn stale_approval_cannot_advance_revised_action() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    assert_eq!(
        runner
            .approve_pending_action("1.0", "U1", "approve-plan", "old", "E2", "now")
            .unwrap(),
        ApprovalOutcome::Stale {
            current_action_id: "approve-plan".into()
        }
    );
}

#[test]
fn approval_uses_current_project_authority() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    config.projects.get_mut("app").unwrap().project_approvers = BTreeSet::from(["U2".into()]);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    write_config(home.path(), &config).unwrap();
    assert_eq!(
        runner
            .approve_pending_action("1.0", "U1", "approve-plan", "revision-1", "E2", "now")
            .unwrap(),
        ApprovalOutcome::WaitingForApprover
    );
}

#[test]
fn invalid_reloaded_authority_fails_closed() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    config.projects.get_mut("app").unwrap().project_approvers = BTreeSet::from(["U2".into()]);
    fs::write(
        config_path(home.path()),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    assert!(
        runner
            .approve_pending_action("1.0", "U2", "approve-plan", "revision-1", "E2", "now")
            .unwrap_err()
            .to_string()
            .contains("participant or approver authority")
    );
}

#[test]
fn invalid_reloaded_config_fails_before_inbox_persistence() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    config.enabled_providers.clear();
    fs::write(
        config_path(home.path()),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();

    let approval = event("E2", Some("1.0"), "y");
    assert!(
        runner
            .ingest(approval)
            .unwrap_err()
            .to_string()
            .contains("conversation provider")
    );
    let inbox = fs::read_to_string(state_root(home.path()).join("inbox.json")).unwrap();
    assert!(!inbox.contains("\"E2\""));
    assert!(
        runner
            .approve_pending_action("1.0", "U1", "approve-plan", "revision-1", "E3", "now")
            .unwrap_err()
            .to_string()
            .contains("conversation provider")
    );
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .is_empty()
    );
}

#[test]
fn valid_disk_reload_clears_constructor_config_error() {
    let (home, _checkout, valid_config) = setup(true);
    let mut invalid_config = valid_config.clone();
    invalid_config.enabled_providers.clear();
    let mut runner = CollaborationRunner::new(
        home.path(),
        invalid_config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    write_config(home.path(), &valid_config).unwrap();

    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();

    assert_eq!(runner.config, valid_config);
    assert_eq!(runner.provider().calls.len(), 1);
}

#[test]
fn conversational_approval_targets_single_current_question() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    for (index, answer) in ["y", "approve", ""].into_iter().enumerate() {
        let action = pending_action(PendingActionKind::Planning);
        runner.request_approval("1.0", action.clone()).unwrap();
        let event_id = format!("approval-{index}");
        let event = if answer.is_empty() {
            approval_event(&event_id, answer, &action)
        } else {
            event(&event_id, Some("1.0"), answer)
        };
        runner.ingest(event).unwrap();
        let thread = FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap();
        assert_eq!(thread.approvals.len(), index + 1);
    }
}

#[test]
fn non_approver_gets_private_explanation_while_thread_waits() {
    let (home, _checkout, mut config) = setup(true);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    let mut reply = event("E2", Some("1.0"), "y");
    reply.user = "U2".into();
    runner.ingest(reply).unwrap();
    assert_eq!(runner.slack().private_responses.len(), 1);
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.text.contains("Waiting for a project approver"))
    );
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .pending_action
            .is_some()
    );
}

#[test]
fn reconnect_applies_still_current_planning_approval() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut action = pending_action(PendingActionKind::Planning);
    action.presented_at = "1.5".into();
    runner.request_approval("1.0", action).unwrap();
    let mut recovered = event("E2", Some("1.0"), "y");
    recovered.timestamp = "2.0".into();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.reconnect([recovered], "reconnected".into()).unwrap();
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .len(),
        1
    );
}

#[test]
fn reconnect_rejects_reply_before_presented_microsecond() {
    let (home, _checkout, config) = setup(true);
    let mut initial = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    initial.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let mut action = pending_action(PendingActionKind::Planning);
    action.presented_at = "1700000000.000002".into();
    initial.request_approval("1.0", action).unwrap();
    let mut reply = event("E2", Some("1.0"), "y");
    reply.timestamp = "1700000000.000001".into();
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    recovered.reconnect([reply], "reconnected".into()).unwrap();
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .is_empty()
    );
}

#[test]
fn reconnect_rejects_stale_or_revoked_planning_approval() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let stale = pending_action(PendingActionKind::Planning);
    let mut replacement = stale.clone();
    replacement.artifact_revision = "revision-2".into();
    runner.request_approval("1.0", replacement).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .reconnect([approval_event("E2", "y", &stale)], "reconnected".into())
        .unwrap();
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .is_empty()
    );

    let current = pending_action(PendingActionKind::Planning);
    runner.request_approval("1.0", current.clone()).unwrap();
    config
        .projects
        .get_mut("app")
        .unwrap()
        .project_approvers
        .clear();
    config.projects.get_mut("app").unwrap().allowed_participants = BTreeSet::from(["U2".into()]);
    config.projects.get_mut("app").unwrap().project_approvers = BTreeSet::from(["U2".into()]);
    write_config(home.path(), &config).unwrap();
    runner
        .reconnect(
            [approval_event("E3", "y", &current)],
            "reconnected-again".into(),
        )
        .unwrap();
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .is_empty()
    );
}

#[test]
fn reconnect_reconfirms_delayed_consequential_action() {
    for kind in [
        PendingActionKind::Execution,
        PendingActionKind::Resume,
        PendingActionKind::Landing,
    ] {
        let (home, _checkout, config) = setup(true);
        let mut runner = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
        let action = pending_action(kind);
        runner.request_approval("1.0", action.clone()).unwrap();
        let mut runner = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        runner
            .reconnect([approval_event("E2", "y", &action)], "reconnected".into())
            .unwrap();
        let thread = FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap();
        assert!(thread.approvals.is_empty());
        assert!(
            thread
                .pending_question
                .as_deref()
                .unwrap()
                .contains("fresh confirmation")
        );
        assert!(
            runner
                .slack()
                .deliveries
                .iter()
                .any(|delivery| delivery.text.contains("Fresh confirmation"))
        );
    }
}

#[test]
fn pending_inbox_reconnect_reconfirms_consequential_action() {
    for kind in [
        PendingActionKind::Execution,
        PendingActionKind::Resume,
        PendingActionKind::Landing,
    ] {
        let (home, _checkout, config) = setup(true);
        let mut initial = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        initial.ingest(event("E1", None, "@Fluent hello")).unwrap();
        let action = pending_action(kind);
        initial.request_approval("1.0", action.clone()).unwrap();
        let approval = approval_event("E2", "y", &action);
        assert_eq!(
            FileCollaborationStore::new(home.path())
                .persist_inbox(&config, &approval)
                .unwrap(),
            InboxState::Pending
        );

        let mut recovered = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        recovered.reconnect([], "reconnected".into()).unwrap();
        let thread = FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap();
        assert!(thread.approvals.is_empty());
        assert!(
            thread
                .pending_question
                .as_deref()
                .unwrap()
                .contains("fresh confirmation")
        );
        assert!(
            recovered
                .slack()
                .deliveries
                .iter()
                .any(|delivery| delivery.text.contains("Fresh confirmation"))
        );
    }
}

#[test]
fn setup_validates_complete_collaboration_authority() {
    let (_home, _checkout, mut config) = setup(true);
    config.enabled_providers.clear();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("conversation provider")
    );
    config
        .enabled_providers
        .extend(["codex".into(), "claude".into()]);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .project_approvers
        .insert("U9".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("participant or approver")
    );
    config
        .projects
        .get_mut("app")
        .unwrap()
        .project_approvers
        .remove("U9");
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("invalid user".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Slack identity")
    );

    let (_home, _checkout, mut config) = setup(true);
    config.workspace.clear();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("workspace")
    );

    let (_home, _checkout, mut config) = setup(true);
    config
        .channel_defaults
        .insert("C2".into(), "missing".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("channel default")
    );

    let (_home, _checkout, mut config) = setup(true);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .checkout
        .label
        .clear();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("checkout identity")
    );
}

#[test]
fn project_provider_policy_must_be_within_runner_allowlist() {
    let (_home, _checkout, mut config) = setup(true);
    config.enabled_providers.remove("claude");

    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not locally enabled")
    );
}

#[test]
fn participant_revocation_applies_to_active_and_future_threads() {
    let (home, _checkout, mut config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .clear();
    config
        .projects
        .get_mut("app")
        .unwrap()
        .project_approvers
        .clear();
    // A disabled project policy is invalid, so replace it with another allowed actor.
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    config
        .projects
        .get_mut("app")
        .unwrap()
        .project_approvers
        .insert("U2".into());
    write_config(home.path(), &config).unwrap();
    runner.ingest(event("E2", Some("1.0"), "continue")).unwrap();
    let mut future = event("E3", None, "@Fluent new work");
    future.timestamp = "2.0".into();
    runner.ingest(future).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "2.0")
            .unwrap()
            .is_none()
    );
    assert_eq!(runner.slack().private_responses.len(), 2);
}

#[test]
fn default_project_change_does_not_rebind_existing_threads() {
    let (home, _checkout, mut config) = setup(true);
    let other_checkout = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(other_checkout.path());
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent first")).unwrap();
    config.projects.insert(
        "other".into(),
        ConfiguredProject {
            label: "other".into(),
            checkout: Checkout {
                label: "other".into(),
                path: other_checkout.path().into(),
                identity,
                branch: "main".into(),
                commit,
            },
            allowed_participants: BTreeSet::from(["U1".into()]),
            default_provider: "codex".into(),
            enabled_providers: BTreeSet::from(["codex".into(), "claude".into()]),
            project_approvers: BTreeSet::from(["U1".into()]),
            github_remote: None,
        },
    );
    config.channel_defaults.insert("C1".into(), "other".into());
    write_config(home.path(), &config).unwrap();
    let mut second = event("E2", None, "@Fluent second");
    second.timestamp = "2.0".into();
    runner.ingest(second).unwrap();
    let store = FileCollaborationStore::new(home.path());
    assert_eq!(
        store
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .project_label
            .as_deref(),
        Some("app")
    );
    assert_eq!(
        store
            .load_thread(&runner.config, "2.0")
            .unwrap()
            .unwrap()
            .project_label
            .as_deref(),
        Some("other")
    );
}

#[test]
fn explicit_approver_request_authorizes_eligible_action() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Execution))
        .unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "execute this work"))
        .unwrap();
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .len(),
        1
    );
}

#[test]
fn explicit_action_requests_authorize_each_eligible_action() {
    for (kind, request) in [
        (PendingActionKind::Planning, "plan this work"),
        (PendingActionKind::Execution, "execute this work"),
        (PendingActionKind::Resume, "resume this work"),
        (PendingActionKind::Landing, "land this work"),
    ] {
        let (home, _checkout, config) = setup(true);
        let mut runner = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
        runner
            .request_approval("1.0", pending_action(kind))
            .unwrap();

        runner.ingest(event("E2", Some("1.0"), request)).unwrap();

        let thread = FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap();
        assert_eq!(thread.approvals.len(), 1, "request: {request}");
        assert!(thread.pending_action.is_none(), "request: {request}");
    }
}

#[test]
fn contextual_action_mentions_do_not_authorize_pending_actions() {
    for (kind, contribution) in [
        (PendingActionKind::Planning, "the plan needs more work"),
        (PendingActionKind::Execution, "I cannot execute it today"),
        (PendingActionKind::Execution, "should we execute this work?"),
        (PendingActionKind::Resume, "do not resume this yet"),
        (PendingActionKind::Landing, "do not land this yet"),
        (PendingActionKind::Landing, "landing will happen tomorrow"),
    ] {
        let (home, _checkout, config) = setup(true);
        let mut runner = CollaborationRunner::new(
            home.path(),
            config.clone(),
            InMemoryProvider::default(),
            InMemorySlack::default(),
        );
        runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
        runner
            .request_approval("1.0", pending_action(kind))
            .unwrap();

        runner
            .ingest(event("E2", Some("1.0"), contribution))
            .unwrap();

        let thread = FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap();
        assert!(thread.approvals.is_empty(), "contribution: {contribution}");
        assert!(
            thread.pending_action.is_some(),
            "contribution: {contribution}"
        );
        assert_eq!(
            runner.provider().calls.len(),
            2,
            "contribution: {contribution}"
        );
    }
}

#[test]
fn duplicate_approval_event_is_idempotent() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .request_approval("1.0", pending_action(PendingActionKind::Planning))
        .unwrap();
    let event = event("E2", Some("1.0"), "y");
    runner.ingest(event.clone()).unwrap();
    runner.ingest(event).unwrap();
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .len(),
        1
    );
}

fn read_http_request(stream: &mut impl Read) {
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        let count = stream.read(&mut buffer).unwrap();
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
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
            return;
        }
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
fn socket_mode_block_action_carries_revision_bound_approval() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let action = pending_action(PendingActionKind::Planning);
    runner.request_approval("1.0", action.clone()).unwrap();
    let control = runner
        .slack()
        .deliveries
        .iter()
        .find_map(|delivery| delivery.approval_control.as_ref())
        .expect("revision-bound approval control");
    assert_eq!(control.action_id, action.id);
    assert_eq!(control.artifact_revision, action.artifact_revision);
    let matching = decode_socket_mode_envelope(&format!(
        r#"{{"envelope_id":"A1","payload":{{"type":"block_actions","user":{{"id":"U1"}},"channel":{{"id":"C1"}},"message":{{"ts":"2.0","thread_ts":"1.0"}},"actions":[{{"action_id":"fluent_approval","value":"{}","block_id":"{}"}}]}}}}"#,
        action.id, action.artifact_revision
    ))
    .unwrap();
    assert_eq!(matching.approval_action_id.as_deref(), Some("approve-plan"));
    runner.ingest(matching).unwrap();
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .len(),
        1
    );

    let mut replacement = pending_action(PendingActionKind::Planning);
    replacement.artifact_revision = "revision-2".into();
    runner.request_approval("1.0", replacement).unwrap();
    let stale = decode_socket_mode_envelope(
        r#"{"envelope_id":"A2","payload":{"type":"block_actions","user":{"id":"U1"},"channel":{"id":"C1"},"message":{"ts":"3.0","thread_ts":"1.0"},"actions":[{"action_id":"fluent_approval","value":"approve-plan","block_id":"revision-1"}]}}"#,
    )
    .unwrap();
    runner.ingest(stale).unwrap();
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&config, "1.0")
            .unwrap()
            .unwrap()
            .approvals
            .len(),
        1
    );
}

#[test]
fn socket_mode_envelope_normalizes_message_changes() {
    let edit = decode_socket_mode_envelope(
        r#"{"envelope_id":"ENV2","payload":{"event_id":"EV2","event":{"channel":"C1","subtype":"message_changed","message":{"ts":"2.0","thread_ts":"1.0","user":"U1","text":"revised"}}}}"#,
    )
    .unwrap();
    assert_eq!(edit.change, InboundChange::Edit);
    assert_eq!(edit.timestamp, "2.0");
    assert_eq!(edit.text, "revised");

    let deletion = decode_socket_mode_envelope(
        r#"{"envelope_id":"ENV3","payload":{"event_id":"EV3","event":{"channel":"C1","subtype":"message_deleted","deleted_ts":"2.0","previous_message":{"ts":"2.0","thread_ts":"1.0","user":"U1"}}}}"#,
    )
    .unwrap();
    assert_eq!(deletion.change, InboundChange::Delete);
    assert_eq!(deletion.timestamp, "2.0");
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
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
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
        *captured.lock().unwrap() = String::from_utf8(request).unwrap();
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
        approval_control: None,
        update_timestamp: None,
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

#[test]
fn slack_web_api_sends_private_refusals_ephemerally() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let request = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
            .unwrap();
        String::from_utf8(buffer[..count].to_vec()).unwrap()
    });
    let mut slack = SlackWebApiTransport::new(
        format!("http://{address}"),
        "xapp-test".into(),
        "xoxb-test".into(),
    );
    slack.respond_private("C1", "U1", "not allowed").unwrap();
    let request = request.join().unwrap();
    assert!(request.contains("POST /chat.postEphemeral"));
    assert!(request.contains("\"channel\":\"C1\""));
    assert!(request.contains("\"user\":\"U1\""));
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
        reply = json.dumps({
            "type": "message",
            "text": "provider reply",
            "references": [{"path": "README", "line": 1}]
        })
        if os.path.exists(sys.argv[0] + ".reply"):
            with open(sys.argv[0] + ".reply") as source:
                reply = source.read()
        turn = {
            "id": "turn-" + action_id,
            "status": "inProgress" if held else "completed",
            "items": [
                {"id": "user-1", "type": "userMessage", "clientId": action_id,
                 "content": request["params"]["input"]}
            ]
        }
        if not held:
            turn["items"].append({
                "id": "agent-1",
                "type": "agentMessage",
                "text": reply
            })
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
    fs::write(checkout.path().join("README"), "fixture\n").unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };
    let mut provider = CodexAppServerProvider::new(executable.clone());
    provider.ready().unwrap();
    let (session, reply) = provider.start("action-1", &scope, "hello", "U1").unwrap();
    assert_eq!(session, "thread-1");
    assert_eq!(
        serde_json::from_str::<ProviderOutput>(&reply).unwrap(),
        ProviderOutput::Message {
            text: "provider reply".into(),
            references: vec![CodeReference {
                path: "README".into(),
                line: 1,
            }],
        }
    );
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
    let input = turn["params"]["input"][0]["text"].as_str().unwrap();
    assert!(input.contains("Slack user U1: hello"));
    assert!(input.contains(r#""type":"planning_artifact""#));
    assert!(input.contains(r#""references""#));
    assert!(input.contains("repository-relative"));
    assert!(input.contains("complete Markdown"));
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
fn provider_login_uses_runner_scoped_homes() {
    let fixture = TempDir::new().unwrap();
    let executable = fixture.path().join("provider-login");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$HOME\" \"${CODEX_HOME-}\" \"${CLAUDE_CONFIG_DIR-}\" \"$*\" > \"$0.log\"\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let (_home, checkout, config) = setup(true);
    let draft = checkout.path().join(".fluent/work/slack/drafts/1.0");

    authenticate_provider_with_executable(&config, "app", "1.0", "codex", &executable).unwrap();
    let codex = fs::read_to_string(executable.with_extension("log")).unwrap();
    assert_eq!(
        codex.trim(),
        format!(
            "{}|{}||login",
            draft.join(".provider-home").display(),
            draft.join(".codex-home").display()
        )
    );

    authenticate_provider_with_executable(&config, "app", "1.0", "claude", &executable).unwrap();
    let claude = fs::read_to_string(executable.with_extension("log")).unwrap();
    assert_eq!(
        claude.trim(),
        format!(
            "{}||{}|",
            draft.join(".provider-home").display(),
            draft.join(".claude-home").display()
        )
    );
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
                "id": "agent-1",
                "type": "agentMessage",
                "text": serde_json::json!({
                    "type": "message",
                    "text": "provider reply",
                    "references": [{"path": "README", "line": 1}]
                }).to_string()
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
    let reply = recovered
        .slack()
        .deliveries
        .iter()
        .find(|delivery| delivery.id == "E1:reply")
        .unwrap();
    assert!(reply.text.contains("README:1"));
    assert!(!reply.text.contains(&_checkout.path().display().to_string()));
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
fn thread_uses_and_displays_project_default_provider() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.provider.as_deref(), Some("codex"));
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("provider: codex")
    );
}

#[test]
fn thread_can_select_enabled_nondefault_provider() {
    struct NamedProvider {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }
    impl ConversationProvider for NamedProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push(self.name.into());
            Ok((
                format!("{}-session", self.name),
                format!("{} reply", self.name),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = ProviderRegistry::new([
        Box::new(NamedProvider {
            name: "codex",
            calls: calls.clone(),
        }) as Box<dyn ConversationProvider>,
        Box::new(NamedProvider {
            name: "claude",
            calls: calls.clone(),
        }) as Box<dyn ConversationProvider>,
    ])
    .unwrap();
    let (home, _checkout, config) = setup(true);
    let mut runner =
        CollaborationRunner::new(home.path(), config, registry, InMemorySlack::default());
    runner
        .ingest(event("E1", None, "@Fluent [provider=claude] hello"))
        .unwrap();
    assert_eq!(runner.config.projects["app"].default_provider, "codex");
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.provider.as_deref(), Some("claude"));
    assert_eq!(&*calls.lock().unwrap(), &["claude"]);
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("provider: claude")
    );
    assert!(runner.slack().deliveries[0].text.contains("claude reply"));
}

#[test]
fn selected_provider_dispatches_to_matching_adapter() {
    struct NamedProvider {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }
    impl ConversationProvider for NamedProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push(self.name.into());
            Ok((
                format!("{}-session", self.name),
                format!("{} reply", self.name),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = ProviderRegistry::new([
        Box::new(NamedProvider {
            name: "codex",
            calls: calls.clone(),
        }) as Box<dyn ConversationProvider>,
        Box::new(NamedProvider {
            name: "claude",
            calls: calls.clone(),
        }) as Box<dyn ConversationProvider>,
    ])
    .unwrap();
    let (home, _checkout, config) = setup(true);
    let mut runner =
        CollaborationRunner::new(home.path(), config, registry, InMemorySlack::default());
    runner
        .ingest(event("E1", None, "@Fluent [provider=claude] hello"))
        .unwrap();
    assert_eq!(&*calls.lock().unwrap(), &["claude"]);
    assert!(runner.slack().deliveries[0].text.contains("claude reply"));
}

#[test]
fn claude_structured_session_starts_and_resumes() {
    let fixture = TempDir::new().unwrap();
    let executable = fixture.path().join("claude");
    fs::write(
        &executable,
        "#!/bin/sh\nset -eu\nprintf '%s|%s|%s\\n' \"$HOME\" \"$CLAUDE_CONFIG_DIR\" \"$*\" >> \"$0.log\"\nprintf '%s\\n' '{\"session_id\":\"claude-session\",\"result\":\"{\\\"type\\\":\\\"message\\\",\\\"text\\\":\\\"provider reply\\\",\\\"references\\\":[{\\\"path\\\":\\\"README\\\",\\\"line\\\":1}]}\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let checkout = TempDir::new().unwrap();
    fs::write(checkout.path().join("README"), "fixture\n").unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };
    let mut provider = ClaudeStructuredSessionProvider::new_scoped_with_test_launcher(
        executable.clone(),
        transparent_sandbox_launcher(&fixture),
    );

    let (session, reply) = provider
        .start("action-1", &scope, "inspect this", "U1")
        .unwrap();
    assert_eq!(session, "claude-session");
    let expected = ProviderOutput::Message {
        text: "provider reply".into(),
        references: vec![CodeReference {
            path: "README".into(),
            line: 1,
        }],
    };
    assert_eq!(
        serde_json::from_str::<ProviderOutput>(&reply).unwrap(),
        expected
    );
    assert_eq!(
        provider
            .resume("action-2", &session, &scope, "continue", "U2")
            .unwrap(),
        serde_json::to_string(&expected).unwrap()
    );

    let invocations = fs::read_to_string(executable.with_extension("log")).unwrap();
    assert!(
        invocations.contains(
            &scope
                .slice_draft
                .join(".provider-home")
                .display()
                .to_string()
        )
    );
    assert!(invocations.contains(&scope.slice_draft.join(".claude-home").display().to_string()));
    assert!(invocations.contains("--print --output-format json --permission-mode plan"));
    assert!(invocations.contains("Slack user U1: inspect this"));
    assert!(invocations.contains("repository-relative"));
    assert!(invocations.contains("references"));
    assert!(invocations.contains("--resume claude-session Slack user U2: continue"));
}

#[test]
fn production_adapters_reject_unsafe_structured_references() {
    let fixture = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    fs::write(checkout.path().join("README"), "fixture\n").unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };
    let unsafe_reply = serde_json::json!({
        "type": "message",
        "text": "unsafe",
        "references": [{"path": "../secret", "line": 1}]
    })
    .to_string();

    let codex = codex_fixture(&fixture);
    fs::write(format!("{}.reply", codex.display()), &unsafe_reply).unwrap();
    let mut codex_provider = CodexAppServerProvider::new(codex);
    let codex_error = codex_provider
        .start("action-1", &scope, "inspect", "U1")
        .unwrap_err();
    assert_eq!(codex_error.failure, ProviderFailure::IncompatibleProtocol);

    let claude = fixture.path().join("claude-unsafe-reference");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            serde_json::json!({"session_id": "claude-session", "result": unsafe_reply})
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let mut claude_provider = ClaudeStructuredSessionProvider::new(claude);
    let claude_error = claude_provider
        .start("action-2", &scope, "inspect", "U1")
        .unwrap_err();
    assert_eq!(claude_error.failure, ProviderFailure::IncompatibleProtocol);
}

#[test]
fn production_adapters_require_complete_structured_references() {
    let fixture = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    fs::write(checkout.path().join("README"), "fixture\n").unwrap();
    let scope = ProviderScope {
        checkout: checkout.path().into(),
        slice_draft: checkout.path().join(".fluent/work/slack/drafts/test"),
    };

    let codex = codex_fixture(&fixture);
    fs::write(
        format!("{}.reply", codex.display()),
        serde_json::json!({"type": "message", "text": "missing references"}).to_string(),
    )
    .unwrap();
    let mut codex_provider = CodexAppServerProvider::new(codex);
    let codex_error = codex_provider
        .start("action-1", &scope, "inspect", "U1")
        .unwrap_err();
    assert_eq!(codex_error.failure, ProviderFailure::IncompatibleProtocol);

    let claude = fixture.path().join("claude-invalid-line");
    let invalid_line = serde_json::json!({
        "type": "message",
        "text": "invalid line",
        "references": [{"path": "README", "line": 2}]
    })
    .to_string();
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            serde_json::json!({"session_id": "claude-session", "result": invalid_line})
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let mut claude_provider = ClaudeStructuredSessionProvider::new(claude);
    let claude_error = claude_provider
        .start("action-2", &scope, "inspect", "U1")
        .unwrap_err();
    assert_eq!(claude_error.failure, ProviderFailure::IncompatibleProtocol);
}

#[test]
fn provider_adapters_return_typed_setup_and_recovery_failures() {
    let fixture = TempDir::new().unwrap();
    let scope = ProviderScope {
        checkout: fixture.path().into(),
        slice_draft: fixture.path().join(".fluent/work/slack/drafts/test"),
    };

    let claude = fixture.path().join("claude-incompatible");
    fs::write(&claude, "#!/bin/sh\nprintf 'not-json\\n'\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let mut claude_provider = ClaudeStructuredSessionProvider::new(claude);
    let claude_error = claude_provider
        .start("action-1", &scope, "inspect this", "U1")
        .unwrap_err();
    assert_eq!(claude_error.failure, ProviderFailure::IncompatibleProtocol);
    assert!(
        claude_error
            .diagnostic()
            .contains("Claude provider protocol")
    );
    let claude_resume_error = claude_provider
        .resume("action-2", "claude-session", &scope, "continue", "U1")
        .unwrap_err();
    assert_eq!(
        claude_resume_error.failure,
        ProviderFailure::IncompatibleProtocol
    );

    let codex = fixture.path().join("codex-incompatible");
    fs::write(
        &codex,
        "#!/bin/sh\nread request\nprintf 'not-json\\n'\nwhile read request; do :; done\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let mut codex_provider = CodexAppServerProvider::new(codex);
    let codex_error = codex_provider.prepare_session(&scope).unwrap_err();
    assert_eq!(codex_error.failure, ProviderFailure::IncompatibleProtocol);
    assert!(codex_error.diagnostic().contains("Codex provider protocol"));
    let codex_recovery_error = codex_provider
        .recover_turn("action-1", "codex-session", &scope)
        .unwrap_err();
    assert_eq!(
        codex_recovery_error.failure,
        ProviderFailure::IncompatibleProtocol
    );
}

fn queued_runner_scenario() -> (Vec<String>, Vec<OutboundDelivery>) {
    #[derive(Clone)]
    struct QueueProvider {
        calls: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<(usize, bool)>, Condvar)>,
    }
    impl ConversationProvider for QueueProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push(contribution.into());
            let (lock, changed) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            changed.notify_all();
            while !state.1 {
                state = changed.wait(state).unwrap();
            }
            Ok(("session".into(), "continued".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<String> {
            self.calls.lock().unwrap().push(contribution.into());
            let (lock, changed) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            changed.notify_all();
            Ok("follow-up".into())
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new((0, false)), Condvar::new()));
    let provider = QueueProvider {
        calls: calls.clone(),
        gate: gate.clone(),
    };
    let (home, _checkout, mut config) = setup(true);
    config
        .projects
        .get_mut("app")
        .unwrap()
        .allowed_participants
        .insert("U2".into());
    let mut runner =
        AsyncCollaborationRunner::new(home.path(), config, provider, InMemorySlack::default(), 1);
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while state.0 == 0 {
            state = changed.wait(state).unwrap();
        }
    }
    runner
        .ingest(event("E2", Some("1.0"), "first queued"))
        .unwrap();
    let mut second = event("E3", Some("1.0"), "second queued");
    second.user = "U2".into();
    runner.ingest(second).unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    runner.wait_for_completion().unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while state.0 < 2 {
            state = changed.wait(state).unwrap();
        }
    }
    let calls = calls.lock().unwrap().clone();
    (calls, runner.slack().deliveries.clone())
}

#[test]
fn reply_during_active_turn_shows_queued_state() {
    let (_, deliveries) = queued_runner_scenario();
    assert!(deliveries.iter().any(|delivery| {
        delivery
            .text
            .contains("queued for the next conversation turn")
    }));
}

#[test]
fn queued_replies_form_one_ordered_attributed_turn() {
    let (calls, _) = queued_runner_scenario();
    assert_eq!(
        calls.last().unwrap(),
        "Slack user U1: first queued\nSlack user U2: second queued"
    );
}

#[test]
fn edit_during_active_turn_updates_queued_attributed_contribution() {
    #[derive(Clone)]
    struct EditingProvider {
        calls: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<(usize, bool)>, Condvar)>,
    }
    impl ConversationProvider for EditingProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push(contribution.into());
            let (lock, changed) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            changed.notify_all();
            while !state.1 {
                state = changed.wait(state).unwrap();
            }
            Ok(("session".into(), "initial reply".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<String> {
            self.calls.lock().unwrap().push(contribution.into());
            let (lock, changed) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            changed.notify_all();
            Ok("follow-up".into())
        }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new((0, false)), Condvar::new()));
    let (home, _checkout, config) = setup(true);
    let store = FileCollaborationStore::new(home.path());
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        EditingProvider {
            calls: calls.clone(),
            gate: gate.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while state.0 == 0 {
            state = changed.wait(state).unwrap();
        }
    }
    let mut queued = event("E2", Some("1.0"), "draft queued reply");
    queued.timestamp = "2.0".into();
    runner.ingest(queued).unwrap();
    let mut edit = event("E3", Some("1.0"), "safe edited reply");
    edit.timestamp = "2.0".into();
    edit.change = InboundChange::Edit;
    runner.ingest(edit).unwrap();

    let durable = store.load_thread(&config, "1.0").unwrap().unwrap();
    assert_eq!(
        durable.queued_contributions[0].contribution,
        "safe edited reply"
    );
    assert_eq!(durable.queued_contributions[0].actor, "U1");

    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    runner.wait_for_completion().unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while state.0 < 2 {
            state = changed.wait(state).unwrap();
        }
    }
    assert_eq!(
        calls.lock().unwrap().last().unwrap(),
        "Slack user U1: safe edited reply"
    );
}

#[test]
fn async_runner_admits_waiting_threads_fairly_at_capacity_one() {
    #[derive(Clone)]
    struct FairProvider {
        calls: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl ConversationProvider for FairProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(contribution.into());
                calls.len() == 1
            };
            if first {
                let (lock, changed) = &*self.gate;
                let mut released = lock.lock().unwrap();
                changed.notify_all();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
            }
            Ok((format!("session-{contribution}"), "reply".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (home, _checkout, config) = setup(true);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config,
        FairProvider {
            calls: calls.clone(),
            gate: gate.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent first")).unwrap();
    while calls.lock().unwrap().is_empty() {
        std::thread::yield_now();
    }
    let mut second = event("E2", None, "@Fluent second");
    second.timestamp = "2.0".into();
    runner.ingest(second).unwrap();
    assert_eq!(&*calls.lock().unwrap(), &["@Fluent first"]);

    {
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
    runner.wait_for_completion().unwrap();
    runner.wait_for_completion().unwrap();
    assert_eq!(
        &*calls.lock().unwrap(),
        &["@Fluent first", "@Fluent second"]
    );
}

#[test]
fn async_runner_admits_parallel_threads_at_configured_capacity() {
    #[derive(Clone)]
    struct ParallelProvider {
        gate: Arc<(Mutex<(usize, bool)>, Condvar)>,
    }
    impl ConversationProvider for ParallelProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            let (lock, changed) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            changed.notify_all();
            while !state.1 {
                state = changed.wait(state).unwrap();
            }
            Ok((format!("session-{}", state.0), "reply".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let gate = Arc::new((Mutex::new((0, false)), Condvar::new()));
    let (home, _checkout, config) = setup(true);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config,
        ParallelProvider { gate: gate.clone() },
        InMemorySlack::default(),
        2,
    );
    runner.ingest(event("E1", None, "@Fluent first")).unwrap();
    let mut second = event("E2", None, "@Fluent second");
    second.timestamp = "2.0".into();
    runner.ingest(second).unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while state.0 < 2 {
            state = changed.wait(state).unwrap();
        }
        state.1 = true;
        changed.notify_all();
    }
    runner.wait_for_completion().unwrap();
    runner.wait_for_completion().unwrap();
}

#[test]
fn fatal_worker_still_launches_the_next_fair_admission() {
    #[derive(Clone)]
    struct FatalFirstProvider {
        calls: Arc<AtomicUsize>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl ConversationProvider for FatalFirstProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let (lock, changed) = &*self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                return Err(ProviderError::internal("simulated fatal worker"));
            }
            Ok(("session".into(), "recovered waiter".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (home, _checkout, config) = setup(true);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config,
        FatalFirstProvider {
            calls: calls.clone(),
            gate: gate.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent first")).unwrap();
    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let mut second = event("E2", None, "@Fluent second");
    second.timestamp = "2.0".into();
    runner.ingest(second).unwrap();
    {
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
    assert!(runner.wait_for_completion().is_err());
    runner.wait_for_completion().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn async_provider_failure_pauses_until_approved_switch() {
    #[derive(Clone)]
    struct PausingProvider {
        selected: String,
        calls: Arc<(Mutex<Vec<String>>, Condvar)>,
    }
    impl ConversationProvider for PausingProvider {
        fn name(&self) -> &str {
            &self.selected
        }
        fn select(&mut self, provider: &str) -> ProviderResult<()> {
            self.selected = provider.into();
            Ok(())
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            let (lock, changed) = &*self.calls;
            lock.lock().unwrap().push(self.selected.clone());
            changed.notify_all();
            if self.selected == "codex" {
                return Err(ProviderError::new(
                    ProviderFailure::Authentication,
                    "provider login required",
                ));
            }
            Ok(("claude-session".into(), "recovered".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }

    let calls = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let (home, _checkout, config) = setup(true);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        PausingProvider {
            selected: "codex".into(),
            calls: calls.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    runner.wait_for_completion().unwrap();

    let (lock, changed) = &*calls;
    let (observed, _) = changed
        .wait_timeout_while(
            lock.lock().unwrap(),
            std::time::Duration::from_millis(100),
            |calls| calls.len() < 2,
        )
        .unwrap();
    assert_eq!(&*observed, &["codex"]);
    drop(observed);
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.pending_turn.unwrap().dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    assert_eq!(
        runner
            .slack()
            .deliveries
            .iter()
            .filter(|delivery| delivery.text.contains("authentication"))
            .count(),
        1
    );

    runner
        .ingest(event("E2", Some("1.0"), "[provider=claude]"))
        .unwrap();
    runner.wait_for_completion().unwrap();
    assert_eq!(&*lock.lock().unwrap(), &["codex", "claude"]);
}

#[test]
fn async_checkout_loss_pauses_until_explicit_reconfirmation() {
    #[derive(Clone)]
    struct CountingProvider(Arc<AtomicUsize>);
    impl ConversationProvider for CountingProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(("session".into(), "initial".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("continued".into())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let (home, checkout, config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        CountingProvider(calls.clone()),
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    runner.wait_for_completion().unwrap();
    let revoked = checkout.path().join(".git-revoked");
    fs::rename(checkout.path().join(".git"), &revoked).unwrap();

    runner.ingest(event("E2", Some("1.0"), "continue")).unwrap();
    runner.wait_for_completion().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.project_label.as_deref(), Some("app"));
    assert_eq!(
        thread.pending_turn.unwrap().dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    assert_eq!(
        runner
            .slack()
            .deliveries
            .iter()
            .filter(|delivery| delivery.text.contains("bound checkout is unavailable"))
            .count(),
        1
    );

    fs::rename(revoked, checkout.path().join(".git")).unwrap();
    runner
        .restore_connection(Vec::new(), "checkout-reconfirmed".into(), false)
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let paused = FileCollaborationStore::new(home.path())
        .load_thread(&config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        paused.pending_turn.unwrap().dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    let rejected = std::process::Command::new(env!("CARGO_BIN_EXE_fluent"))
        .env("HOME", home.path())
        .args([
            "slack",
            "reconfirm-checkout",
            "--project",
            "other",
            "--thread",
            "1.0",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let accepted = std::process::Command::new(env!("CARGO_BIN_EXE_fluent"))
        .env("HOME", home.path())
        .args([
            "slack",
            "reconfirm-checkout",
            "--project",
            "app",
            "--thread",
            "1.0",
        ])
        .output()
        .unwrap();
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let actions_path = state_root(home.path()).join("recovery-actions.json");
    let unavailable_again = checkout.path().join(".git-unavailable-again");
    fs::rename(checkout.path().join(".git"), &unavailable_again).unwrap();
    assert!(
        runner
            .drive_once(std::time::Duration::from_millis(1))
            .is_err()
    );
    let retained: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&fs::read(&actions_path).unwrap()).unwrap();
    assert!(retained.contains_key("1.0:checkout-reconfirmation"));
    fs::rename(unavailable_again, checkout.path().join(".git")).unwrap();
    let mut actions: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&fs::read(&actions_path).unwrap()).unwrap();
    let mut valid = actions.remove("1.0:checkout-reconfirmation").unwrap();
    valid["id"] = serde_json::json!("z-valid");
    actions.insert("z-valid".into(), valid);
    actions.insert(
        "0-stale-project".into(),
        serde_json::json!({
            "id": "0-stale-project",
            "project": "retired-project",
            "thread": "1.0",
            "kind": "CheckoutReconfirmation"
        }),
    );
    actions.insert(
        "1-stale-provider".into(),
        serde_json::json!({
            "id": "1-stale-provider",
            "project": "app",
            "thread": "1.0",
            "kind": {"ProviderLogin": {"provider": "claude"}}
        }),
    );
    fs::write(&actions_path, serde_json::to_vec_pretty(&actions).unwrap()).unwrap();
    runner
        .drive_once(std::time::Duration::from_millis(1))
        .unwrap();
    for _ in 0..100 {
        if calls.load(Ordering::SeqCst) == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    runner.poll_completions().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let remaining: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&fs::read(actions_path).unwrap()).unwrap();
    assert!(remaining.is_empty());
}

#[test]
fn async_provider_login_recovery_rechecks_scope_before_resuming() {
    #[derive(Clone)]
    struct LoginProvider {
        preparations: Arc<AtomicUsize>,
        turns: Arc<AtomicUsize>,
    }
    impl ConversationProvider for LoginProvider {
        fn prepare_session(&mut self, scope: &ProviderScope) -> ProviderResult<Option<String>> {
            assert!(scope.checkout.join(".git").is_dir());
            if self.preparations.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(ProviderError::new(
                    ProviderFailure::Authentication,
                    "provider login required",
                ));
            }
            Ok(Some("authenticated-session".into()))
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            unreachable!()
        }
        fn resume(
            &mut self,
            _: &str,
            session: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            assert_eq!(session, "authenticated-session");
            self.turns.fetch_add(1, Ordering::SeqCst);
            Ok("resumed after login".into())
        }
    }

    let preparations = Arc::new(AtomicUsize::new(0));
    let turns = Arc::new(AtomicUsize::new(0));
    let (home, _checkout, config) = setup(true);
    write_config(home.path(), &config).unwrap();
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        LoginProvider {
            preparations: preparations.clone(),
            turns: turns.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    runner.wait_for_completion().unwrap();
    assert_eq!(preparations.load(Ordering::SeqCst), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 0);

    drop(runner);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        LoginProvider {
            preparations: preparations.clone(),
            turns: turns.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner
        .restore_connection(Vec::new(), "login-recovered".into(), false)
        .unwrap();
    assert_eq!(preparations.load(Ordering::SeqCst), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 0);
    let login = home.path().join("codex-login");
    fs::write(&login, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&login, fs::Permissions::from_mode(0o755)).unwrap();
    let authenticated = std::process::Command::new(env!("CARGO_BIN_EXE_fluent"))
        .env("HOME", home.path())
        .env("FLUENT_CODEX_APP_SERVER", &login)
        .args([
            "slack",
            "provider-login",
            "--project",
            "app",
            "--thread",
            "1.0",
            "--provider",
            "codex",
        ])
        .output()
        .unwrap();
    assert!(
        authenticated.status.success(),
        "{}",
        String::from_utf8_lossy(&authenticated.stderr)
    );
    let mut revoked = config.clone();
    let project = revoked.projects.get_mut("app").unwrap();
    project.default_provider = "claude".into();
    project.enabled_providers = BTreeSet::from(["claude".into()]);
    write_config(home.path(), &revoked).unwrap();
    assert!(
        runner
            .drive_once(std::time::Duration::from_millis(1))
            .unwrap_err()
            .to_string()
            .contains("no longer enabled")
    );
    assert_eq!(preparations.load(Ordering::SeqCst), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 0);
    let paused = FileCollaborationStore::new(home.path())
        .load_thread(&revoked, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        paused.pending_turn.unwrap().dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );

    write_config(home.path(), &config).unwrap();
    runner
        .drive_once(std::time::Duration::from_millis(1))
        .unwrap();
    for _ in 0..100 {
        if turns.load(Ordering::SeqCst) == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    runner.poll_completions().unwrap();
    assert_eq!(preparations.load(Ordering::SeqCst), 2);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[test]
fn provider_failure_classifies_recovery_state() {
    assert_eq!(
        classify_provider_failure("authentication required"),
        ProviderFailure::Authentication
    );
    assert_eq!(
        classify_provider_failure("HTTP 429 rate limit"),
        ProviderFailure::RateLimited {
            retry_after_seconds: 60
        }
    );
    assert_eq!(
        classify_provider_failure("runner host credential access denied"),
        ProviderFailure::HostCredentialDenied
    );
    assert_eq!(
        classify_provider_failure("Permission denied (os error 13)"),
        ProviderFailure::HostCredentialDenied
    );
    assert_eq!(
        classify_provider_failure("incompatible provider protocol version"),
        ProviderFailure::IncompatibleProtocol
    );
}

#[test]
fn provider_failure_preserves_replies_and_reports_recovery() {
    struct FailingProvider(&'static str);
    impl ConversationProvider for FailingProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            Err(ProviderError::from_diagnostic(self.0))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        FailingProvider("authentication required"),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("authentication is required")
    );
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.pending_turn.as_ref().unwrap().contribution,
        "@Fluent hello"
    );
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::Authentication)
    );

    let (home, _checkout, config) = setup(true);
    let mut unavailable = CollaborationRunner::new(
        home.path(),
        config,
        FailingProvider("provider runtime unavailable"),
        InMemorySlack::default(),
    );
    unavailable
        .ingest(event("E1", None, "@Fluent preserve this"))
        .unwrap();
    let recovery = &unavailable.slack().deliveries[0].text;
    assert!(recovery.contains("queued replies are preserved"));
    assert!(recovery.contains("project approver may switch providers"));
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&unavailable.config, "1.0")
            .unwrap()
            .unwrap()
            .provider_failure,
        Some(ProviderFailure::Unavailable)
    );
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&unavailable.config, "1.0")
            .unwrap()
            .unwrap()
            .pending_turn
            .unwrap()
            .contribution,
        "@Fluent preserve this"
    );

    for (diagnostic, safe_message) in [
        (
            "runner host credential access denied",
            "Provider credentials are unavailable to the runner host",
        ),
        (
            "incompatible provider protocol version",
            "Provider protocol is incompatible with this Fluent build",
        ),
    ] {
        let (home, _checkout, config) = setup(true);
        let mut runner = CollaborationRunner::new(
            home.path(),
            config,
            FailingProvider(diagnostic),
            InMemorySlack::default(),
        );
        runner
            .ingest(event("E1", None, "@Fluent preserve typed failure"))
            .unwrap();
        let recovery = &runner.slack().deliveries[0].text;
        assert!(recovery.contains(safe_message));
        assert!(recovery.contains("project approver may switch providers"));
        assert_eq!(
            FileCollaborationStore::new(home.path())
                .load_thread(&runner.config, "1.0")
                .unwrap()
                .unwrap()
                .pending_turn
                .unwrap()
                .contribution,
            "@Fluent preserve typed failure"
        );
    }
}

#[test]
fn provider_setup_failure_preserves_typed_state_and_pending_turn() {
    struct SetupFailureProvider;
    impl ConversationProvider for SetupFailureProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            unreachable!()
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
        fn prepare_session(&mut self, _: &ProviderScope) -> ProviderResult<Option<String>> {
            Err(ProviderError::new(
                ProviderFailure::HostCredentialDenied,
                "secret credential helper diagnostic",
            ))
        }
    }

    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        SetupFailureProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent keep setup turn"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::HostCredentialDenied)
    );
    assert_eq!(
        thread.pending_turn.unwrap().contribution,
        "@Fluent keep setup turn"
    );
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("credentials are unavailable to the runner host")
    );
    assert!(
        !runner.slack().deliveries[0]
            .text
            .contains("secret credential helper diagnostic")
    );
}

#[test]
fn provider_recovery_failure_preserves_typed_state_and_pending_turn() {
    struct RecoveryFailureProvider;
    impl ConversationProvider for RecoveryFailureProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            unreachable!()
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
        fn recover_turn(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
        ) -> ProviderResult<ProviderTurnRecovery> {
            Err(ProviderError::new(
                ProviderFailure::IncompatibleProtocol,
                "secret recovery protocol diagnostic",
            ))
        }
    }

    let (home, _checkout, config) = setup(true);
    let mut first = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    first
        .ingest(event("E1", None, "@Fluent establish session"))
        .unwrap();
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        RecoveryFailureProvider,
        InMemorySlack::default(),
    );
    recovered
        .ingest(event("E2", Some("1.0"), "keep recovery turn"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&recovered.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::IncompatibleProtocol)
    );
    assert_eq!(
        thread.pending_turn.unwrap().contribution,
        "keep recovery turn"
    );
    assert!(
        recovered.slack().deliveries[0]
            .text
            .contains("protocol is incompatible")
    );
    assert!(
        !recovered.slack().deliveries[0]
            .text
            .contains("secret recovery protocol diagnostic")
    );
}

#[test]
fn provider_selection_failure_preserves_typed_state_and_pending_turn() {
    struct SelectionFailureProvider;
    impl ConversationProvider for SelectionFailureProvider {
        fn select(&mut self, _: &str) -> ProviderResult<()> {
            Err(ProviderError::new(
                ProviderFailure::Unavailable,
                "selected adapter disappeared",
            ))
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            unreachable!()
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }

    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        SelectionFailureProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent keep selection turn"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.provider_failure, Some(ProviderFailure::Unavailable));
    assert_eq!(
        thread.pending_turn.unwrap().contribution,
        "@Fluent keep selection turn"
    );
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("queued replies are preserved")
    );
}

#[test]
fn missing_accepted_provider_turn_preserves_incompatible_protocol_state() {
    struct MissingAcceptedTurnProvider;
    impl ConversationProvider for MissingAcceptedTurnProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            unreachable!()
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
        fn recover_turn(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
        ) -> ProviderResult<ProviderTurnRecovery> {
            Ok(ProviderTurnRecovery::Missing)
        }
    }

    let (home, _checkout, config) = setup(true);
    let mut first = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    first
        .ingest(event("E1", None, "@Fluent establish session"))
        .unwrap();
    let store = FileCollaborationStore::new(home.path());
    let mut thread = store.load_thread(&config, "1.0").unwrap().unwrap();
    thread.pending_turn = Some(PendingTurn {
        action_id: "E2".into(),
        source_timestamp: "2.0".into(),
        contribution: "keep accepted turn".into(),
        actor: "U1".into(),
        starts_session: false,
        provider_turn_id: Some("accepted-turn".into()),
        completed_reply: None,
        dispatch_state: PendingTurnDispatchState::Runnable,
        retry_at_unix_seconds: None,
        delivery_attempt: 0,
    });
    store.save_thread(&config, &thread).unwrap();
    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        MissingAcceptedTurnProvider,
        InMemorySlack::default(),
    );
    recovered
        .ingest(event("E2", Some("1.0"), "keep accepted turn"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&recovered.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::IncompatibleProtocol)
    );
    assert_eq!(
        thread.pending_turn.unwrap().contribution,
        "keep accepted turn"
    );
    assert!(
        recovered.slack().deliveries[0]
            .text
            .contains("protocol is incompatible")
    );
}

#[test]
fn provider_reference_failure_preserves_typed_state_and_pending_turn() {
    struct ReferenceFailureProvider;
    impl ConversationProvider for ReferenceFailureProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session".into(),
                serde_json::json!({
                    "type": "message",
                    "text": "reply",
                    "references": [{"path": "../secret", "line": 1}]
                })
                .to_string(),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            Ok("reply".into())
        }
    }

    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        ReferenceFailureProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent inspect code"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::IncompatibleProtocol)
    );
    let pending = thread.pending_turn.unwrap();
    assert_eq!(pending.contribution, "@Fluent inspect code");
    assert!(pending.provider_turn_id.is_some());
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("protocol is incompatible")
    );
}

#[test]
fn unavailable_provider_preserves_reply_and_offers_switch() {
    struct UnavailableProvider;
    impl ConversationProvider for UnavailableProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            Err(ProviderError::new(
                ProviderFailure::Unavailable,
                "provider runtime unavailable",
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        UnavailableProvider,
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent help")).unwrap();
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("queued replies are preserved")
    );
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("project approver may switch providers")
    );
}

#[test]
fn synchronous_rate_limit_rechecks_provider_policy_before_retry() {
    struct RateLimitedProvider {
        attempts: Arc<AtomicUsize>,
    }
    impl ConversationProvider for RateLimitedProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::new(
                ProviderFailure::RateLimited {
                    retry_after_seconds: 17,
                },
                "HTTP 429 rate limit; retry-after=17",
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let now = Arc::new(AtomicU64::new(1_000));
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        RateLimitedProvider {
            attempts: attempts.clone(),
        },
        InMemorySlack::default(),
    );
    let clock = now.clone();
    runner.set_clock_for_test(move || clock.load(Ordering::SeqCst));
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("retrying in 17 seconds")
    );
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .pending_turn
            .is_some()
    );
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .provider_failure,
        Some(ProviderFailure::RateLimited {
            retry_after_seconds: 17
        })
    );
    assert_eq!(
        FileCollaborationStore::new(home.path())
            .load_thread(&runner.config, "1.0")
            .unwrap()
            .unwrap()
            .pending_turn
            .unwrap()
            .dispatch_state,
        PendingTurnDispatchState::RateLimitedUntilDeadline
    );

    let mut revoked = config.clone();
    let project = revoked.projects.get_mut("app").unwrap();
    project.default_provider = "claude".into();
    project.enabled_providers = BTreeSet::from(["claude".into()]);
    write_config(home.path(), &revoked).unwrap();
    now.store(1_017, Ordering::SeqCst);
    assert!(
        runner
            .retry_due_turns()
            .unwrap_err()
            .to_string()
            .contains("no longer enabled")
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let paused = FileCollaborationStore::new(home.path())
        .load_thread(&revoked, "1.0")
        .unwrap()
        .unwrap()
        .pending_turn
        .unwrap();
    assert_eq!(
        paused.dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    assert_eq!(paused.retry_at_unix_seconds, None);
}

#[test]
fn rate_limit_retry_rechecks_provider_policy_before_dispatch() {
    #[derive(Clone)]
    struct RateLimitedProvider {
        attempts: Arc<AtomicUsize>,
    }
    impl ConversationProvider for RateLimitedProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::new(
                ProviderFailure::RateLimited {
                    retry_after_seconds: 17,
                },
                "provider rate limit; retry-after=17",
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let (home, _checkout, config) = setup(true);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config.clone(),
        RateLimitedProvider {
            attempts: attempts.clone(),
        },
        InMemorySlack::default(),
        1,
    );
    runner.set_clock_for_test(|| 1_000);
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    runner.wait_for_completion().unwrap();

    let mut revoked = config.clone();
    let project = revoked.projects.get_mut("app").unwrap();
    project.default_provider = "claude".into();
    project.enabled_providers = BTreeSet::from(["claude".into()]);
    write_config(home.path(), &revoked).unwrap();
    assert!(
        runner
            .wake_due_retries(1_017)
            .unwrap_err()
            .to_string()
            .contains("no longer enabled")
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let paused = FileCollaborationStore::new(home.path())
        .load_thread(&revoked, "1.0")
        .unwrap()
        .unwrap()
        .pending_turn
        .unwrap();
    assert_eq!(
        paused.dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    assert_eq!(paused.retry_at_unix_seconds, None);
}

#[test]
fn rate_limit_waits_recovers_and_offers_provider_switch() {
    struct ClockSlack {
        inner: InMemorySlack,
        now: Arc<AtomicU64>,
    }
    impl SlackTransport for ClockSlack {
        fn acknowledge(&mut self, event_id: &str) -> anyhow::Result<()> {
            self.inner.acknowledge(event_id)
        }
        fn post(&mut self, delivery: &OutboundDelivery) -> anyhow::Result<()> {
            self.inner.post(delivery)
        }
        fn receive_timeout(
            &mut self,
            timeout: std::time::Duration,
        ) -> anyhow::Result<Option<InboundEnvelope>> {
            self.now.fetch_add(timeout.as_secs(), Ordering::SeqCst);
            Ok(None)
        }
    }
    #[derive(Clone)]
    struct RecoveringProvider {
        calls: Arc<Mutex<Vec<String>>>,
        attempts: Arc<AtomicUsize>,
    }
    impl ConversationProvider for RecoveringProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push(contribution.into());
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(ProviderError::new(
                    ProviderFailure::RateLimited {
                        retry_after_seconds: 17,
                    },
                    "provider rate limit; retry-after=17",
                ));
            }
            Ok(("session".into(), "recovered".into()))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            _: &str,
        ) -> ProviderResult<String> {
            self.calls.lock().unwrap().push(contribution.into());
            Ok("follow-up".into())
        }
    }
    let now = Arc::new(AtomicU64::new(1_000));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = RecoveringProvider {
        calls: calls.clone(),
        attempts: Arc::new(AtomicUsize::new(0)),
    };
    let (home, _checkout, config) = setup(true);
    let stored_config = config.clone();
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        config,
        provider.clone(),
        ClockSlack {
            inner: InMemorySlack::default(),
            now: now.clone(),
        },
        1,
    );
    let clock = now.clone();
    runner.set_clock_for_test(move || clock.load(Ordering::SeqCst));

    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    runner.wait_for_completion().unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "first queued"))
        .unwrap();
    runner
        .ingest(event("E3", Some("1.0"), "second queued"))
        .unwrap();
    assert!(runner.slack().inner.deliveries.iter().any(|delivery| {
        delivery
            .text
            .contains("queued for the next conversation turn")
    }));
    assert!(runner.slack().inner.deliveries.iter().any(|delivery| {
        delivery
            .text
            .contains("project approver may switch providers")
    }));
    assert!(
        runner
            .slack()
            .inner
            .deliveries
            .iter()
            .any(|delivery| { delivery.text.contains("retrying in 17 seconds") })
    );

    drop(runner);
    let mut runner = AsyncCollaborationRunner::new(
        home.path(),
        stored_config.clone(),
        provider,
        ClockSlack {
            inner: InMemorySlack::default(),
            now: now.clone(),
        },
        1,
    );
    let clock = now.clone();
    runner.set_clock_for_test(move || clock.load(Ordering::SeqCst));

    runner
        .drive_once(std::time::Duration::from_secs(16))
        .unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    runner
        .drive_once(std::time::Duration::from_secs(60))
        .unwrap();
    runner.wait_for_completion().unwrap();
    assert_eq!(
        &*calls.lock().unwrap(),
        &[
            "@Fluent initial".to_string(),
            "@Fluent initial".to_string(),
            "Slack user U1: first queued\nSlack user U1: second queued".to_string()
        ]
    );
    assert!(
        FileCollaborationStore::new(home.path())
            .load_thread(&stored_config, "1.0")
            .unwrap()
            .unwrap()
            .pending_turn
            .is_none()
    );
}

#[test]
fn approver_switches_provider_without_changing_work() {
    struct SwitchingProvider {
        name: &'static str,
        calls: Arc<Mutex<Vec<(String, String)>>>,
        fail_resume: bool,
    }
    impl ConversationProvider for SwitchingProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            actor: &str,
        ) -> ProviderResult<(String, String)> {
            self.calls.lock().unwrap().push((
                self.name.into(),
                format!("Slack user {actor}: {contribution}"),
            ));
            Ok((
                format!("{}-session", self.name),
                format!("{} prior reply", self.name),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            contribution: &str,
            actor: &str,
        ) -> ProviderResult<String> {
            self.calls.lock().unwrap().push((
                self.name.into(),
                format!("Slack user {actor}: {contribution}"),
            ));
            if self.fail_resume {
                return Err(ProviderError::new(
                    ProviderFailure::Unavailable,
                    "provider runtime unavailable",
                ));
            }
            Ok("continued reply".into())
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = ProviderRegistry::new([
        Box::new(SwitchingProvider {
            name: "codex",
            calls: calls.clone(),
            fail_resume: true,
        }) as Box<dyn ConversationProvider>,
        Box::new(SwitchingProvider {
            name: "claude",
            calls: calls.clone(),
            fail_resume: false,
        }) as Box<dyn ConversationProvider>,
    ])
    .unwrap();
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        registry,
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "continue the plan"))
        .unwrap();

    let mut revoked = config.clone();
    let project = revoked.projects.get_mut("app").unwrap();
    project.enabled_providers = BTreeSet::from(["codex".into()]);
    write_config(home.path(), &revoked).unwrap();
    assert!(
        runner
            .switch_provider("1.0", "U1", "claude")
            .unwrap_err()
            .to_string()
            .contains("not enabled")
    );
    assert_eq!(calls.lock().unwrap().len(), 2);

    write_config(home.path(), &config).unwrap();
    runner
        .ingest(event("E3", Some("1.0"), "@Fluent [provider=claude]"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.provider.as_deref(), Some("claude"));
    assert!(thread.linked_work.is_none());
    assert!(thread.pending_turn.is_none());
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[0],
        ("codex".into(), "Slack user U1: @Fluent hello".into())
    );
    assert_eq!(
        calls[1],
        ("codex".into(), "Slack user U1: continue the plan".into())
    );
    assert_eq!(calls[2].0, "claude");
    assert!(calls[2].1.contains("Slack user U1: @Fluent hello"));
    assert!(calls[2].1.contains("Slack user Fluent: codex prior reply"));
    assert!(calls[2].1.contains("Slack user U1: continue the plan"));
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.text.contains("claude prior reply"))
    );
}

#[test]
fn approver_cannot_switch_a_completed_thread() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    let error = runner.switch_provider("1.0", "U1", "claude").unwrap_err();
    assert!(error.to_string().contains("interrupted Fluent thread"));
}

#[test]
fn committed_code_reference_uses_commit_pinned_github_link() {
    let (_home, checkout, config) = setup(true);
    std::process::Command::new("git")
        .arg("-C")
        .arg(checkout.path())
        .args([
            "remote",
            "add",
            "origin",
            "git@github.com:fluent/example.git",
        ])
        .status()
        .unwrap();
    let reference = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("README"),
        1,
    )
    .unwrap();
    assert!(reference.contains(&format!(
        "/blob/{}/README#L1",
        config.projects["app"].checkout.commit
    )));
    assert!(!reference.contains(&checkout.path().display().to_string()));
}

#[test]
fn local_code_reference_uses_labeled_slack_fallback() {
    let (_home, checkout, config) = setup(true);
    fs::write(checkout.path().join("README"), "dirty local source").unwrap();
    let reference = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("README"),
        1,
    )
    .unwrap();
    assert!(reference.contains("local-only reference"));
    assert!(reference.contains("dirty local source"));
    assert!(!reference.contains(&checkout.path().display().to_string()));
}

#[test]
fn clean_code_reference_without_supported_remote_uses_labeled_slack_fallback() {
    let (_home, checkout, config) = setup(true);
    let reference = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("README"),
        1,
    )
    .unwrap();
    assert!(reference.contains("local-only reference"));
    assert!(reference.contains("fixture"));
    assert!(!reference.contains("github.com"));
    assert!(!reference.contains(&checkout.path().display().to_string()));
}

#[test]
fn local_code_reference_bounds_long_slack_excerpt() {
    let (_home, checkout, config) = setup(true);
    fs::write(checkout.path().join("README"), "x".repeat(500)).unwrap();
    let reference = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("README"),
        1,
    )
    .unwrap();
    assert!(reference.contains("[truncated]"));
    assert!(reference.len() < 400);
}

#[test]
fn local_code_reference_truncates_at_utf8_boundary() {
    let (_home, checkout, config) = setup(true);
    fs::write(checkout.path().join("README"), "🦀".repeat(300)).unwrap();
    let reference = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("README"),
        1,
    )
    .unwrap();
    assert!(reference.contains("… [truncated]"));
    assert!(reference.is_char_boundary(reference.len()));
}

#[test]
fn code_question_uses_bound_checkout_and_relative_references() {
    struct AdapterOutputProvider {
        scopes: Arc<Mutex<Vec<std::path::PathBuf>>>,
    }
    impl ConversationProvider for AdapterOutputProvider {
        fn start(
            &mut self,
            _: &str,
            scope: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            self.scopes.lock().unwrap().push(scope.checkout.clone());
            Ok((
                "session".into(),
                serde_json::to_string(&ProviderOutput::Message {
                    text: "Inspect the README for the binding.".into(),
                    references: vec![CodeReference {
                        path: "README".into(),
                        line: 1,
                    }],
                })
                .unwrap(),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, checkout, config) = setup(true);
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        AdapterOutputProvider {
            scopes: scopes.clone(),
        },
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent where is the binding?"))
        .unwrap();
    assert_eq!(&*scopes.lock().unwrap(), &[checkout.path().to_owned()]);
    let reply = &runner.slack().deliveries[0].text;
    assert!(reply.contains("README:1"));
    assert!(reply.contains("local-only reference"));
    assert!(!reply.contains(&checkout.path().display().to_string()));
}

#[test]
fn code_question_posts_provider_reference_in_slack_thread() {
    struct ReferencingProvider;
    impl ConversationProvider for ReferencingProvider {
        fn start(
            &mut self,
            _action_id: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session".into(),
                serde_json::to_string(&ProviderOutput::Message {
                    text: "The implementation is here.".into(),
                    references: vec![CodeReference {
                        path: "README".into(),
                        line: 1,
                    }],
                })
                .unwrap(),
            ))
        }
        fn resume(
            &mut self,
            _action_id: &str,
            _session: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        ReferencingProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent where is this code?"))
        .unwrap();
    let reply = &runner.slack().deliveries[0].text;
    assert!(reply.contains("README:1"));
    assert!(!reply.contains(&checkout.path().display().to_string()));
}

#[test]
fn malformed_provider_reference_preserves_typed_state_and_pending_turn() {
    struct EscapingProvider;
    impl ConversationProvider for EscapingProvider {
        fn start(
            &mut self,
            _action_id: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session".into(),
                serde_json::json!({
                    "type": "message",
                    "text": "answer",
                    "references": [{"path": "../secret", "line": 1}]
                })
                .to_string(),
            ))
        }
        fn resume(
            &mut self,
            _action_id: &str,
            _session: &str,
            _scope: &ProviderScope,
            _contribution: &str,
            _actor: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        EscapingProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent inspect code"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::IncompatibleProtocol)
    );
    assert_eq!(
        thread.pending_turn.unwrap().contribution,
        "@Fluent inspect code"
    );
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("protocol is incompatible")
    );
}

#[test]
fn unformattable_provider_reference_preserves_typed_state_and_pending_turn() {
    struct MissingReferenceProvider;
    impl ConversationProvider for MissingReferenceProvider {
        fn start(
            &mut self,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session".into(),
                serde_json::json!({
                    "type": "message",
                    "text": "answer",
                    "references": [{"path": "missing.rs", "line": 1}]
                })
                .to_string(),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        MissingReferenceProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent inspect missing code"))
        .unwrap();
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(
        thread.provider_failure,
        Some(ProviderFailure::IncompatibleProtocol)
    );
    let pending = thread.pending_turn.unwrap();
    assert_eq!(pending.contribution, "@Fluent inspect missing code");
    assert!(pending.provider_turn_id.is_some());
    assert!(
        runner.slack().deliveries[0]
            .text
            .contains("protocol is incompatible")
    );
}

#[test]
fn provider_reply_sanitizes_host_paths_before_persistence_and_delivery() {
    struct HostPathProvider;
    impl ConversationProvider for HostPathProvider {
        fn start(
            &mut self,
            _: &str,
            scope: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<(String, String)> {
            Ok((
                "session".into(),
                format!(
                    "Inspect {} and /Users/runner/private/credentials.txt; also See </Users/runner/angle/private.txt>; keep src/module.rs and https://example.test/docs",
                    scope.checkout.join("README:1").display()
                ),
            ))
        }
        fn resume(
            &mut self,
            _: &str,
            _: &str,
            _: &ProviderScope,
            _: &str,
            _: &str,
        ) -> ProviderResult<String> {
            unreachable!()
        }
    }
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        HostPathProvider,
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent inspect paths"))
        .unwrap();

    let delivery = &runner.slack().deliveries[0].text;
    assert!(delivery.contains("README:1"));
    assert!(delivery.contains("[REDACTED_PATH]"));
    assert!(delivery.contains("See <[REDACTED_PATH]>"));
    assert!(delivery.contains("src/module.rs"));
    assert!(delivery.contains("https://example.test/docs"));
    assert!(!delivery.contains(&checkout.path().display().to_string()));
    assert!(!delivery.contains("/Users/runner/private"));
    assert!(!delivery.contains("/Users/runner/angle"));

    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    let persisted = &thread.conversation.last().unwrap().contribution;
    assert!(persisted.contains("README:1"));
    assert!(persisted.contains("[REDACTED_PATH]"));
    assert!(persisted.contains("See <[REDACTED_PATH]>"));
    assert!(persisted.contains("src/module.rs"));
    assert!(persisted.contains("https://example.test/docs"));
    assert!(!persisted.contains(&checkout.path().display().to_string()));
    assert!(!persisted.contains("/Users/runner/private"));
    assert!(!persisted.contains("/Users/runner/angle"));
}

#[test]
fn code_reference_rejects_checkout_symlink_escape() {
    let (_home, checkout, config) = setup(true);
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        checkout.path().join("linked"),
    )
    .unwrap();
    let error = format_code_reference(
        &config.projects["app"].checkout,
        &checkout.path().join("linked"),
        1,
    )
    .unwrap_err();
    assert!(error.to_string().contains("outside the bound checkout"));
}

#[test]
fn code_reference_rejects_directory_targets() {
    let (_home, checkout, config) = setup(true);
    let error =
        format_code_reference(&config.projects["app"].checkout, checkout.path(), 1).unwrap_err();
    assert!(error.to_string().contains("not a regular file"));
}

#[test]
fn code_reference_rejects_missing_target_without_host_path() {
    let (_home, checkout, config) = setup(true);
    let missing = checkout.path().join("missing.rs");
    let error = format_code_reference(&config.projects["app"].checkout, &missing, 1).unwrap_err();
    assert!(error.to_string().contains("canonicalize referenced code"));
    assert!(!error.to_string().contains(&missing.display().to_string()));
}

#[test]
fn unavailable_bound_checkout_fails_closed_without_rebinding() {
    let (home, checkout, mut config) = setup(true);
    let alternative_checkout = TempDir::new().unwrap();
    let (identity, commit) = initialize_checkout(alternative_checkout.path());
    let mut alternative = config.projects["app"].clone();
    alternative.label = "alternative".into();
    alternative.checkout = Checkout {
        label: "alternative-main".into(),
        path: alternative_checkout.path().into(),
        identity,
        branch: "main".into(),
        commit,
    };
    config.projects.insert("alternative".into(), alternative);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent hello")).unwrap();
    fs::remove_dir_all(checkout.path().join(".git")).unwrap();
    runner
        .ingest(event("E2", Some("1.0"), "inspect code"))
        .unwrap();
    assert_eq!(runner.provider().calls.len(), 1);
    assert!(
        runner
            .provider()
            .calls
            .iter()
            .all(|(_, checkout, _, _)| checkout != alternative_checkout.path())
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.text.contains("bound checkout is unavailable"))
    );
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.project_label.as_deref(), Some("app"));
    assert_ne!(
        runner.config.projects["app"].checkout.identity,
        runner.config.projects["alternative"].checkout.identity
    );
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
    ) -> ProviderResult<(String, String)> {
        let mut actions = self.actions.lock().unwrap();
        let result = actions
            .entry(action_id.into())
            .or_insert_with(|| ("session-1".into(), format!("reply: {contribution}")))
            .clone();
        drop(actions);
        let mut fail = self.fail_after_start.lock().unwrap();
        if *fail {
            *fail = false;
            return Err(ProviderError::internal(
                "simulated crash after provider side effect",
            ));
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
    ) -> ProviderResult<String> {
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
    ) -> ProviderResult<(String, String)> {
        Ok(("session-1".into(), format!("reply: {contribution}")))
    }

    fn resume(
        &mut self,
        _action_id: &str,
        _session: &str,
        _scope: &ProviderScope,
        contribution: &str,
        _actor: &str,
    ) -> ProviderResult<String> {
        if contribution == "second" && !*self.failed.lock().unwrap() {
            *self.failed.lock().unwrap() = true;
            return Err(ProviderError::internal(
                "crash midway through Slack recovery",
            ));
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
        change: InboundChange::Message,
        approval_action_id: None,
        approval_artifact_revision: None,
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
                approval_control: None,
                update_timestamp: None,
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
        approval_control: None,
        update_timestamp: None,
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
fn revoked_git_permissions_preserve_binding_without_provider_access() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner.ingest(event("E1", None, "@Fluent initial")).unwrap();
    assert_eq!(runner.provider().calls.len(), 1);

    let git = checkout.path().join(".git");
    let original_permissions = fs::metadata(&git).unwrap().permissions();
    let mut revoked_permissions = original_permissions.clone();
    revoked_permissions.set_mode(0o000);
    fs::set_permissions(&git, revoked_permissions).unwrap();
    let result = runner.ingest(event("E2", Some("1.0"), "continue"));
    fs::set_permissions(&git, original_permissions).unwrap();
    result.unwrap();

    assert_eq!(runner.provider().calls.len(), 1);
    let thread = FileCollaborationStore::new(home.path())
        .load_thread(&config, "1.0")
        .unwrap()
        .unwrap();
    assert_eq!(thread.project_label.as_deref(), Some("app"));
    assert_eq!(
        thread.pending_turn.unwrap().dispatch_state,
        PendingTurnDispatchState::PausedForRecovery
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|delivery| delivery.text.contains("bound checkout is unavailable"))
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
        provider: Some("codex".into()),
        provider_failure: None,
        provider_session: None,
        slice: None,
        planning_stage: None,
        linked_work: None,
        pending_question: None,
        artifact_revisions: Vec::new(),
        pending_artifact_revision: None,
        linked_thread_intent: None,
        conversation_inputs: Vec::new(),
        pending_action: None,
        approvals: Vec::new(),
        processed_events: BTreeSet::from(["E1".into()]),
        last_connection: None,
        first_reply_timestamp: None,
        queued_contributions: Vec::new(),
        conversation: Vec::new(),
        pending_turn: Some(PendingTurn {
            action_id: "E1".into(),
            source_timestamp: "1.0".into(),
            contribution: "private contribution".into(),
            actor: "U1".into(),
            starts_session: true,
            provider_turn_id: None,
            completed_reply: None,
            dispatch_state: PendingTurnDispatchState::Runnable,
            retry_at_unix_seconds: None,
            delivery_attempt: 0,
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
            default_provider: "codex".into(),
            enabled_providers: BTreeSet::from(["codex".into()]),
            project_approvers: BTreeSet::from(["U2".into()]),
            github_remote: None,
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
    WorkControl::new(checkout.path())
        .approve_plan(
            "slice-7",
            "U1",
            &[PlannedWork::new("work-7", "Recover active Work")],
        )
        .unwrap();
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
            .any(|d| d.id.starts_with("1.0:status:") && d.text.contains("last connected at now"))
    );
    assert!(
        runner
            .slack()
            .deliveries
            .iter()
            .any(|d| d.id.starts_with("1.0:status:")
                && d.text.contains("connection was interrupted"))
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
            && SOCKET_MODE_MANIFEST.contains("message.im")
            && SOCKET_MODE_MANIFEST.contains("im:history")
            && !SOCKET_MODE_MANIFEST.contains("request_url")
    );
    let guide = fs::read_to_string("docs/slack-runner.md")
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let scheduler_run = guide.find("fluent scheduler run").unwrap();
    let scheduler_status = guide.find("fluent scheduler status").unwrap();
    let slack_enable = guide.find("fluent slack enable").unwrap();
    assert!(
        guide.contains("Socket Mode")
            && guide.contains("Keychain")
            && guide.contains("LaunchAgent")
            && guide.contains("mkdir -p ~/.config/fluent/slack")
            && guide.contains("Copy member ID")
            && guide.contains("channel ID (`C...`)")
            && guide.contains("direct_message_defaults")
            && guide.contains("DM conversation ID (`D...`)")
            && guide.contains("project_approvers")
            && guide.contains("Every project approver")
            && guide.contains("/invite @Fluent")
            && guide.contains("@Fluent plan the next release")
            && guide.contains("new work <description>")
            && guide.contains("keeps the thread unbound")
            && guide.contains("fluent slack checkout-identity")
            && guide.contains("security add-generic-password -U -a app-token")
            && guide.contains("security add-generic-password -U -a bot-token")
            && guide.contains("keep that process running")
            && guide.contains("register every configured checkout")
            && guide.contains("/absolute/app-checkout")
            && guide.contains("/absolute/api-checkout")
            && guide.contains("scheduler service is already running")
            && guide.contains("Scheduler: running")
            && guide.contains("preflight checks the scheduler request boundary")
            && scheduler_run < scheduler_status
            && scheduler_status < slack_enable
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
            read_http_request(&mut stream);
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

    web_api_check("app_mentions:read,channels:history,im:history,chat:write").unwrap();
    let error = web_api_check("app_mentions:read,chat:write").unwrap_err();
    assert!(error.to_string().contains("channels:history"));
    let error = web_api_check("app_mentions:read,channels:history,chat:write").unwrap_err();
    assert!(error.to_string().contains("im:history"));
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
fn approved_plan_creates_reported_work_without_execution() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .ingest(event(
            "E2",
            Some("1.0"),
            r#"approve plan {"slice_id":"slice-1","planned_work":[{"id":"work-1","title":"Implement control"}]}"#,
        ))
        .unwrap();

    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("Planned Work Items: work-1")
            && delivery.text.contains("Execution has not started")
    }));
    let work = fluent::work_model::WorkModelStore::new(checkout.path())
        .read_work_item("work-1")
        .unwrap();
    assert!(work.authorization.is_proposed());
    assert!(work.attempts.is_empty());
    assert_eq!(work.transitions.len(), 1);
    assert_eq!(work.transitions[0].object_kind, "work-item");
    assert_eq!(work.transitions[0].object_id, "work-1");
    assert_eq!(work.transitions[0].new_state, "planned");
    assert_eq!(
        work.transitions[0].origin,
        fluent::work_model::DurableTransitionOrigin::SlackThread
    );
    let planned_replies = runner
        .slack()
        .deliveries
        .iter()
        .filter(|delivery| {
            delivery.id.contains(":work-transition:work-1:")
                && delivery.text.contains("is now planned")
                && delivery.text.contains("Origin: slack-thread")
        })
        .count();
    assert_eq!(planned_replies, 1);
    assert!(runner.report_thread_work("1.0").unwrap().is_empty());
    assert_eq!(
        runner
            .slack()
            .deliveries
            .iter()
            .filter(|delivery| delivery.id.contains(":work-transition:work-1:"))
            .count(),
        1
    );
}

#[test]
fn plan_approval_resumes_after_partial_materialization() {
    #[derive(Default)]
    struct RecordingScheduler {
        requests: std::cell::RefCell<Vec<String>>,
    }
    impl WorkScheduler for RecordingScheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            self.requests.borrow_mut().push(request.id.clone());
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }

    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let blocked_path = checkout
        .path()
        .join(".fluent/work/transactions/work-retry-second.json");
    fs::create_dir_all(&blocked_path).unwrap();
    let planned_work = [
        PlannedWork::new("work-retry-first", "First"),
        PlannedWork::new("work-retry-second", "Second"),
    ];

    let error = control
        .approve_plan("slice-retry-plan", "U1", &planned_work)
        .unwrap_err();
    assert!(error.to_string().contains("work-retry-second"));

    let staged_path = checkout
        .path()
        .join(".fluent/work/slack/slices/slice-retry-plan.json");
    let staged: serde_json::Value =
        serde_json::from_slice(&fs::read(&staged_path).unwrap()).unwrap();
    assert_eq!(staged["approval_state"], "materializing");
    assert_eq!(
        staged["planned_work"],
        serde_json::to_value(&planned_work).unwrap()
    );
    assert_eq!(
        staged["work_item_ids"],
        serde_json::json!(["work-retry-first", "work-retry-second"])
    );
    let first = store.read_work_item("work-retry-first").unwrap();
    assert_eq!(first.transitions.len(), 1);
    assert_eq!(
        first.transitions[0].evidence_ref.as_deref(),
        Some("slack-slice:slice-retry-plan")
    );
    let first_report = control
        .schedule_durable_transitions("slice-retry-plan", |_| Ok(()))
        .unwrap();
    assert_eq!(first_report.len(), 1);
    assert_eq!(first_report[0].work_item_id, "work-retry-first");
    let scheduler = RecordingScheduler::default();
    let authorization_error = control
        .authorize_execution("slice-retry-plan", "U1", &scheduler)
        .unwrap_err();
    assert!(
        authorization_error
            .to_string()
            .contains("still materializing its planned Work")
    );
    assert!(scheduler.requests.borrow().is_empty());
    let gated_first = store.read_work_item("work-retry-first").unwrap();
    assert!(gated_first.authorization.is_proposed());
    assert_eq!(gated_first.transitions, first.transitions);
    let changed_retry = control
        .approve_plan(
            "slice-retry-plan",
            "U1",
            &[
                PlannedWork::new("work-retry-first", "Changed"),
                PlannedWork::new("work-retry-second", "Second"),
            ],
        )
        .unwrap_err();
    assert!(
        changed_retry
            .to_string()
            .contains("already has a different approved plan")
    );

    fs::remove_dir(&blocked_path).unwrap();
    let report = control
        .approve_plan("slice-retry-plan", "U1", &planned_work)
        .unwrap();
    assert_eq!(
        report.work_item_ids,
        ["work-retry-first", "work-retry-second"]
    );
    let completed: serde_json::Value =
        serde_json::from_slice(&fs::read(&staged_path).unwrap()).unwrap();
    assert_eq!(completed["approval_state"], "complete");

    let transitions = control
        .schedule_durable_transitions("slice-retry-plan", |_| Ok(()))
        .unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].work_item_id, "work-retry-second");
    assert!(transitions.iter().all(|entry| entry.new_state == "planned"));
    control
        .approve_plan("slice-retry-plan", "U1", &planned_work)
        .unwrap();
    assert_eq!(store.list_work_items().unwrap().len(), 2);
    assert!(
        control
            .reconcile_durable_transitions("slice-retry-plan")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn plan_approval_detects_conflicts_before_publishing_work() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    store
        .create_work_item(&fluent::work_model::WorkItem {
            id: "work-existing".into(),
            title: "Existing".into(),
            authorization: fluent::work_model::ExecutionAuthorization::Proposed,
            ..Default::default()
        })
        .unwrap();

    let error = control
        .approve_plan(
            "slice-conflict",
            "U1",
            &[
                PlannedWork::new("work-new", "New"),
                PlannedWork::new("work-existing", "Existing"),
            ],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already exists outside Slice slice-conflict")
    );
    assert!(store.read_work_item("work-new").is_err());
    assert_eq!(store.list_work_items().unwrap().len(), 1);
    let staged = control.slice("slice-conflict").unwrap().unwrap();
    assert_eq!(staged.approval_state, PlanApprovalState::Materializing);
}

#[test]
fn slice_identifiers_stay_inside_the_slice_store() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    let error = control
        .approve_plan("../escape", "U1", &[PlannedWork::new("work-safe", "Work")])
        .unwrap_err();
    assert!(error.to_string().contains("cannot be used as a file name"));
    assert!(
        !checkout
            .path()
            .join(".fluent/work/slack/escape.json")
            .exists()
    );
}

#[test]
fn direct_work_item_writes_append_lifecycle_transitions() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-write",
            "U1",
            &[PlannedWork::new("work-write", "Work")],
        )
        .unwrap();
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let mut item = store.read_work_item("work-write").unwrap();
    item.attempts.push(fluent::work_model::Attempt {
        id: "attempt-write".into(),
        work_item_id: item.id.clone(),
        status: fluent::work_model::AttemptStatus::Executing,
        ..Default::default()
    });
    store.write_work_item(&item).unwrap();
    let saved = store.read_work_item("work-write").unwrap();
    let transition = saved.transitions.last().unwrap();
    assert_eq!(transition.object_kind, "attempt");
    assert_eq!(transition.object_id, "attempt-write");
    assert_eq!(transition.old_state, None);
    assert_eq!(transition.new_state, "executing");
    assert_eq!(
        transition.origin,
        fluent::work_model::DurableTransitionOrigin::Fluent
    );
}

#[test]
fn legacy_empty_ledger_activates_on_first_post_upgrade_mutation() {
    let (_home, checkout, _config) = setup(true);
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let legacy = fluent::work_model::WorkItem {
        id: "legacy-work".into(),
        title: "Legacy work".into(),
        attempts: vec![fluent::work_model::Attempt {
            id: "attempt-legacy".into(),
            work_item_id: "legacy-work".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    store.create_work_item(&legacy).unwrap();
    assert!(
        store
            .read_work_item("legacy-work")
            .unwrap()
            .transitions
            .is_empty(),
        "the compatibility creation path models a pre-ledger aggregate"
    );

    store
        .mutate_work_item("legacy-work", |item| {
            item.attempts[0].status = fluent::work_model::AttemptStatus::Executing;
            Ok(())
        })
        .unwrap();

    let transitions = store.read_work_item("legacy-work").unwrap().transitions;
    assert_eq!(transitions.len(), 3);
    assert_eq!(transitions[0].sequence, 1);
    assert_eq!(transitions[0].object_kind, "work-item");
    assert_eq!(transitions[0].new_state, "authorized");
    assert_eq!(
        transitions[0].origin,
        fluent::work_model::DurableTransitionOrigin::Unknown,
        "the migrated initial state has no attributable historical origin"
    );
    assert_eq!(transitions[1].sequence, 2);
    assert_eq!(transitions[1].object_kind, "attempt");
    assert_eq!(transitions[1].object_id, "attempt-legacy");
    assert_eq!(transitions[1].new_state, "planned");
    assert_eq!(
        transitions[1].origin,
        fluent::work_model::DurableTransitionOrigin::Unknown
    );
    assert_eq!(transitions[2].sequence, 3);
    assert_eq!(transitions[2].object_kind, "attempt");
    assert_eq!(transitions[2].object_id, "attempt-legacy");
    assert_eq!(transitions[2].old_state.as_deref(), Some("planned"));
    assert_eq!(transitions[2].new_state, "executing");
    assert_eq!(
        transitions[2].origin,
        fluent::work_model::DurableTransitionOrigin::Fluent
    );
}

#[test]
fn legacy_empty_ledger_activates_on_first_whole_aggregate_write() {
    let (_home, checkout, _config) = setup(true);
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let legacy = fluent::work_model::WorkItem {
        id: "legacy-whole-write".into(),
        title: "Legacy work".into(),
        ..Default::default()
    };
    store.create_work_item(&legacy).unwrap();

    let mut changed = store.read_work_item("legacy-whole-write").unwrap();
    changed.attempts.push(fluent::work_model::Attempt {
        id: "attempt-whole-write".into(),
        work_item_id: changed.id.clone(),
        status: fluent::work_model::AttemptStatus::Executing,
        ..Default::default()
    });
    store.write_work_item(&changed).unwrap();

    let transitions = store
        .read_work_item("legacy-whole-write")
        .unwrap()
        .transitions;
    assert_eq!(transitions.len(), 2);
    assert_eq!(transitions[0].object_kind, "work-item");
    assert_eq!(
        transitions[0].origin,
        fluent::work_model::DurableTransitionOrigin::Unknown
    );
    assert_eq!(transitions[1].object_kind, "attempt");
    assert_eq!(transitions[1].object_id, "attempt-whole-write");
    assert_eq!(
        transitions[1].origin,
        fluent::work_model::DurableTransitionOrigin::Fluent
    );
}

#[test]
fn legacy_empty_ledger_still_rejects_forged_history() {
    let (_home, checkout, _config) = setup(true);
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let legacy = fluent::work_model::WorkItem {
        id: "legacy-forged".into(),
        title: "Legacy work".into(),
        ..Default::default()
    };
    store.create_work_item(&legacy).unwrap();

    let error = store
        .mutate_work_item("legacy-forged", |item| {
            item.transitions
                .push(fluent::work_model::DurableWorkTransition {
                    sequence: 1,
                    object_kind: "work-item".into(),
                    object_id: item.id.clone(),
                    old_state: None,
                    new_state: "forged".into(),
                    occurred_at: "2026-08-18T00:00:00Z".into(),
                    elapsed_seconds: None,
                    origin: fluent::work_model::DurableTransitionOrigin::External,
                    reason: None,
                    evidence_ref: None,
                });
            Ok(())
        })
        .unwrap_err();

    assert!(error.to_string().contains("append-only and host-owned"));
    assert!(
        store
            .read_work_item("legacy-forged")
            .unwrap()
            .transitions
            .is_empty()
    );
}

#[test]
fn transition_history_is_immutable_and_metadata_is_atomic() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-ledger-protected",
            "U1",
            &[PlannedWork::new("work-protected", "Work")],
        )
        .unwrap();
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let mut rewritten = store.read_work_item("work-protected").unwrap();
    let initial_transitions = rewritten.transitions.clone();
    rewritten.transitions.clear();
    store.write_work_item(&rewritten).unwrap();
    assert_eq!(
        store.read_work_item("work-protected").unwrap().transitions,
        initial_transitions,
        "whole-aggregate compatibility writes cannot remove host-owned transitions"
    );

    let metadata = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some(format!("capacity became available {}", "r".repeat(1_100))),
        evidence_ref: Some(format!("scheduler-event:42-{}", "e".repeat(600))),
    };
    store
        .mutate_work_item_with_transition("work-protected", &metadata, |item| {
            item.authorization = fluent::work_model::ExecutionAuthorization::execution_ready(
                fluent::work_model::ExecutionAuthority::Human,
            );
            Ok(())
        })
        .unwrap();
    let saved = store.read_work_item("work-protected").unwrap();
    let transition = saved.transitions.last().unwrap();
    assert_eq!(
        transition.origin,
        fluent::work_model::DurableTransitionOrigin::Scheduler
    );
    assert!(
        transition.reason.as_ref().unwrap().len()
            <= fluent::work_model::MAX_TRANSITION_REASON_BYTES
    );
    assert!(
        transition.evidence_ref.as_ref().unwrap().len()
            <= fluent::work_model::MAX_TRANSITION_EVIDENCE_BYTES
    );
}

#[test]
fn legacy_transition_without_origin_is_explicitly_unknown() {
    let transition: fluent::work_model::DurableWorkTransition =
        serde_json::from_value(serde_json::json!({
            "sequence": 1,
            "object_kind": "work-item",
            "object_id": "legacy-work",
            "old_state": null,
            "new_state": "planned",
            "occurred_at": "2026-08-18T00:00:00Z"
        }))
        .unwrap();
    assert_eq!(
        transition.origin,
        fluent::work_model::DurableTransitionOrigin::Unknown
    );
}

#[test]
fn execution_approval_queues_each_planned_work_item() {
    #[derive(Default)]
    struct FakeScheduler {
        requests: std::cell::RefCell<Vec<WorkExecutionRequest>>,
    }
    impl WorkScheduler for FakeScheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            self.requests.borrow_mut().push(request.clone());
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }

    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .ingest(event(
            "E2",
            Some("1.0"),
            r#"approve plan {"slice_id":"slice-1","planned_work":[{"id":"work-1","title":"First"},{"id":"work-2","title":"Second"}]}"#,
        ))
        .unwrap();
    let scheduler = FakeScheduler::default();

    runner
        .ingest_with_scheduler(event("E3", Some("1.0"), "authorize execution"), &scheduler)
        .unwrap();

    assert_eq!(scheduler.requests.borrow().len(), 2);
    for work_id in ["work-1", "work-2"] {
        let work = fluent::work_model::WorkModelStore::new(checkout.path())
            .read_work_item(work_id)
            .unwrap();
        assert!(work.authorization.is_execution_ready());
        assert!(work.attempts.is_empty());
        assert!(runner.slack().deliveries.iter().any(|delivery| {
            delivery.thread_timestamp == "1.0"
                && delivery
                    .text
                    .contains(&format!("Work Item {work_id} queue outcome: Queued"))
        }));
    }
    runner
        .ingest_with_scheduler(event("E4", Some("1.0"), "authorize execution"), &scheduler)
        .unwrap();
    assert_eq!(scheduler.requests.borrow().len(), 2);
}

#[test]
fn execution_approval_retries_the_same_request_after_scheduler_failure() {
    struct FlakyScheduler(std::cell::Cell<u8>, std::cell::RefCell<Vec<String>>);
    impl WorkScheduler for FlakyScheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            self.1.borrow_mut().push(request.id.clone());
            let calls = self.0.get();
            self.0.set(calls + 1);
            if calls == 0 {
                anyhow::bail!("scheduler temporarily unavailable");
            }
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }

    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-retry",
            "U1",
            &[PlannedWork::new("work-retry", "Work")],
        )
        .unwrap();
    let scheduler = FlakyScheduler(std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
    assert!(
        control
            .authorize_execution("slice-retry", "U1", &scheduler)
            .is_err()
    );
    let report = control
        .authorize_execution("slice-retry", "U1", &scheduler)
        .unwrap();
    assert_eq!(report.outcomes.len(), 1);
    let requests = scheduler.1.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn durable_work_transition_posts_detailed_thread_reply() {
    struct Scheduler;
    impl WorkScheduler for Scheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-runner",
            &[PlannedWork::new("work-runner", "Runner integration")],
        )
        .unwrap();
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("Planned Work Items: work-runner")
            && delivery.text.contains("Execution has not started")
    }));
    let planned = runner.report_thread_work("1.0").unwrap();
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].object_kind, "work-item");
    assert_eq!(planned[0].object_id, "work-runner");
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("work-item work-runner")
            && delivery.text.contains("is now planned")
            && delivery.text.contains("Origin: slack-thread")
            && delivery.text.contains("Next action: Plan approved by U1")
            && delivery.text.contains("Elapsed: 0s")
            && delivery.text.contains("Evidence: slack-slice:slice-runner")
    }));
    assert!(runner.report_thread_work("1.0").unwrap().is_empty());

    runner
        .authorize_thread_execution("1.0", "U1", &Scheduler)
        .unwrap();
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery
            .text
            .contains("Work Item work-runner queue outcome")
    }));
    let transitions = runner.report_thread_work("1.0").unwrap();
    assert!(transitions.iter().any(|transition| {
        transition.work_item_id == "work-runner"
            && transition.origin == fluent::work_model::DurableTransitionOrigin::SlackThread
    }));
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("Work Item work-runner")
            && delivery.text.contains("Origin: slack-thread")
            && delivery
                .text
                .contains("Next action: Execution approved by U1")
            && delivery.text.contains("Evidence: slack-slice:slice-runner")
    }));
    assert!(runner.report_thread_work("1.0").unwrap().is_empty());

    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let executing = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some("worker accepted execution".into()),
        evidence_ref: Some("scheduler-event:accepted".into()),
    };
    store
        .mutate_work_item_with_transition("work-runner", &executing, |item| {
            item.attempts.push(fluent::work_model::Attempt {
                id: "attempt-runner".into(),
                work_item_id: item.id.clone(),
                status: fluent::work_model::AttemptStatus::Executing,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let needs_user = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some("choose a recovery option".into()),
        evidence_ref: Some("scheduler-event:needs-user".into()),
    };
    store
        .mutate_work_item_with_transition("work-runner", &needs_user, |item| {
            item.attempts[0].status = fluent::work_model::AttemptStatus::NeedsUser;
            Ok(())
        })
        .unwrap();

    runner.report_thread_work("1.0").unwrap();
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("attempt attempt-runner")
            && delivery.text.contains("is now needs-user")
            && delivery.text.contains("Origin: scheduler")
            && delivery
                .text
                .contains("Next action: choose a recovery option")
            && delivery
                .text
                .contains("Evidence: scheduler-event:needs-user")
            && delivery.text.contains("Elapsed:")
            && delivery.text.contains("requested recovery decision")
    }));

    let source = fluent::work_model::WorkspaceRef {
        id: "b4-source".into(),
        path: "/tmp/b4-source".into(),
    };
    let target = fluent::work_model::WorkspaceRef {
        id: "b4-target".into(),
        path: checkout.path().display().to_string(),
    };
    let landed = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Landing,
        reason: Some("candidate passed required gates".into()),
        evidence_ref: Some("candidate/evidence.json".into()),
    };
    store
        .mutate_work_item_with_transition("work-runner", &landed, |item| {
            item.attempts[0].status = fluent::work_model::AttemptStatus::Complete;
            item.attempts[0].review_state = Some(fluent::work_model::AttemptReviewState::Passed);
            item.attempts[0].tasks.push(fluent::work_model::Task {
                id: "task-runner".into(),
                kind: fluent::work_model::TaskKind::Write,
                status: fluent::work_model::TaskStatus::Complete,
                role: "writer".into(),
                instructions: None,
                work_item_id: item.id.clone(),
                attempt_id: Some("attempt-runner".into()),
                workspace_access: fluent::work_model::WorkspaceAccess {
                    reads: vec![target.clone()],
                    writes: vec![source.clone()],
                },
                artifact_area: None,
                review_context: None,
                evidence_review_context: None,
                input_artifacts: Vec::new(),
                depends_on: None,
                output: Some(fluent::work_model::TaskOutput {
                    workspace_id: source.id.clone(),
                    workspace_path: source.path.clone(),
                    source_branch: "main".into(),
                    base_commit: Some("base-b4".into()),
                    commit: "commit-b4".into(),
                    no_change: None,
                    learner_canonicalization: None,
                }),
                created_at: None,
                started_at: None,
                completed_at: None,
            });
            item.merge_candidates
                .push(fluent::work_model::MergeCandidate {
                    id: "candidate-runner".into(),
                    attempt_id: "attempt-runner".into(),
                    source_workspace: source.clone(),
                    target_workspace: target.clone(),
                    source_branch: "main".into(),
                    target_branch: "main".into(),
                    candidate_commit: "commit-b4".into(),
                    merge_review_state: fluent::work_model::MergeReviewState::Passed,
                    merge_state: fluent::work_model::MergeCandidateMergeState::default(),
                    created_at: None,
                    ready_at: None,
                    started_at: None,
                    completed_at: None,
                });
            Ok(())
        })
        .unwrap();

    let before = runner.slack().deliveries.len();
    let transitions = runner.report_thread_work("1.0").unwrap();
    assert!(transitions.iter().any(|transition| {
        transition.object_kind == "task"
            && transition.object_id == "task-runner"
            && transition.new_state == "complete"
    }));
    assert!(transitions.iter().any(|transition| {
        transition.object_kind == "merge-candidate"
            && transition.object_id == "candidate-runner"
            && transition.new_state == "ready"
    }));
    let replies = &runner.slack().deliveries[before..];
    for (kind, id, state) in [
        ("task", "task-runner", "complete"),
        ("merge-candidate", "candidate-runner", "ready"),
    ] {
        assert!(replies.iter().any(|delivery| {
            delivery.text.contains(&format!("{kind} {id}"))
                && delivery.text.contains(&format!("is now {state}"))
                && delivery.text.contains("Work Item work-runner")
                && delivery.text.contains("Origin: landing")
                && delivery
                    .text
                    .contains("Next action: candidate passed required gates")
                && delivery.text.contains("Elapsed: 0s")
                && delivery.text.contains("Evidence: candidate/evidence.json")
        }));
    }
    let delivered = runner.slack().deliveries.len();
    assert!(runner.report_thread_work("1.0").unwrap().is_empty());
    assert_eq!(runner.slack().deliveries.len(), delivered);
}

#[test]
fn allowed_participant_cannot_approve_without_project_authority() {
    struct Scheduler;
    impl WorkScheduler for Scheduler {
        fn submit(&self, _request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            panic!("unauthorized execution must not reach the scheduler")
        }
    }

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
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();

    let mut denied_plan = event(
        "E2",
        Some("1.0"),
        r#"approve plan {"slice_id":"slice-denied","planned_work":[{"id":"work-denied","title":"Denied"}]}"#,
    );
    denied_plan.user = "U2".into();
    let error = runner.ingest(denied_plan).unwrap_err();

    assert!(error.to_string().contains("cannot approve Work"));

    runner
        .ingest(event(
            "E3",
            Some("1.0"),
            r#"approve plan {"slice_id":"slice-approved","planned_work":[{"id":"work-approved","title":"Approved"}]}"#,
        ))
        .unwrap();
    let mut denied_execution = event("E4", Some("1.0"), "authorize execution");
    denied_execution.user = "U2".into();
    let error = runner
        .ingest_with_scheduler(denied_execution, &Scheduler)
        .unwrap_err();
    assert!(error.to_string().contains("cannot authorize Work"));
}

#[test]
fn status_request_reports_authoritative_work_state_and_actions() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-status-request",
            &[PlannedWork::new("work-status-request", "Status")],
        )
        .unwrap();
    let before = runner.slack().deliveries.len();
    runner
        .ingest(event("E2", Some("1.0"), "work status"))
        .unwrap();
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("work-status-request: planned")
            && delivery.text.contains("Await execution approval")
    }));
}

#[test]
fn status_message_tracks_current_slice_state() {
    let (home, _checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    let first_reply = FileCollaborationStore::new(home.path())
        .load_thread(&runner.config, "1.0")
        .unwrap()
        .unwrap()
        .first_reply_timestamp
        .unwrap();
    assert_eq!(first_reply, "E1:reply");

    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-pinned",
            &[PlannedWork::new("work-pinned", "Pinned")],
        )
        .unwrap();

    let update = runner
        .slack()
        .deliveries
        .iter()
        .find(|delivery| delivery.update_timestamp.as_deref() == Some(first_reply.as_str()))
        .unwrap();
    assert!(update.text.contains("Project: app"));
    assert!(update.text.contains("checkout: main"));
    assert!(update.text.contains("Slice: Active"));
    assert!(update.text.contains("provider: connected"));
    assert!(update.text.contains("work-pinned: planned"));
    assert!(update.text.contains("Await execution approval"));
}

#[test]
fn durable_transition_ledger_replays_once_per_slack_cursor() {
    struct Scheduler;
    impl WorkScheduler for Scheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }

    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-ledger",
            "U1",
            &[PlannedWork::new("work-ledger", "Work")],
        )
        .unwrap();
    assert_eq!(
        control
            .schedule_durable_transitions("slice-ledger", |_| Ok(()))
            .unwrap()[0]
            .new_state,
        "planned"
    );
    assert!(
        control
            .reconcile_durable_transitions("slice-ledger")
            .unwrap()
            .is_empty()
    );
    control
        .authorize_execution("slice-ledger", "U1", &Scheduler)
        .unwrap();
    let transitions = control
        .schedule_durable_transitions("slice-ledger", |_| Ok(()))
        .unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].work_item_id, "work-ledger");
    assert_eq!(transitions[0].new_state, "authorized");
    assert_eq!(
        transitions[0].origin,
        fluent::work_model::DurableTransitionOrigin::SlackThread
    );
    assert!(
        control
            .reconcile_durable_transitions("slice-ledger")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn read_only_transition_reconciliation_does_not_acknowledge_delivery() {
    let (_home, checkout, _config) = setup(true);
    let control = planned_control(&checkout, "slice-read-only", "work-read-only");

    let first = control
        .reconcile_durable_transitions("slice-read-only")
        .unwrap();
    let retry = control
        .reconcile_durable_transitions("slice-read-only")
        .unwrap();
    assert_eq!(retry, first);

    let failure = control
        .schedule_durable_transitions("slice-read-only", |_| {
            Err(anyhow::anyhow!("simulated outbox failure"))
        })
        .unwrap_err();
    assert!(failure.to_string().contains("simulated outbox failure"));
    assert_eq!(
        control
            .reconcile_durable_transitions("slice-read-only")
            .unwrap(),
        first
    );

    let scheduled = control
        .schedule_durable_transitions("slice-read-only", |_| Ok(()))
        .unwrap();
    assert_eq!(scheduled, first);
    assert!(
        control
            .reconcile_durable_transitions("slice-read-only")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn legacy_empty_ledger_replays_initial_and_current_transition_once() {
    struct Scheduler;
    impl WorkScheduler for Scheduler {
        fn submit(&self, request: &WorkExecutionRequest) -> anyhow::Result<WorkQueueOutcome> {
            Ok(WorkQueueOutcome::Queued {
                request_id: request.id.clone(),
            })
        }
    }

    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-legacy-ledger",
            "U1",
            &[PlannedWork::new("work-legacy-ledger", "Legacy Work")],
        )
        .unwrap();
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    let item_path = store.work_item_path("work-legacy-ledger").unwrap();
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&item_path).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("transitions");
    fs::write(&item_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    control
        .authorize_execution("slice-legacy-ledger", "U1", &Scheduler)
        .unwrap();
    let transitions = control
        .schedule_durable_transitions("slice-legacy-ledger", |_| Ok(()))
        .unwrap();
    assert_eq!(
        transitions
            .iter()
            .map(|transition| transition.new_state.as_str())
            .collect::<Vec<_>>(),
        vec!["planned", "authorized"]
    );
    assert_eq!(
        transitions[0].origin,
        fluent::work_model::DurableTransitionOrigin::Unknown
    );
    assert_eq!(
        transitions[1].origin,
        fluent::work_model::DurableTransitionOrigin::SlackThread
    );
    assert!(
        control
            .reconcile_durable_transitions("slice-legacy-ledger")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn status_message_uses_current_durable_slice_projection() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-status",
            "U1",
            &[PlannedWork::new("work-status", "Work")],
        )
        .unwrap();
    let message = control
        .render_status_message("slice-status", "app", "app-checkout", "idle")
        .unwrap();
    assert!(message.contains("Project: app"));
    assert!(message.contains("checkout: app-checkout"));
    assert!(message.contains("Slice: Active"));
    assert!(message.contains("work-status: planned"));
    assert!(message.contains("Await execution approval"));
}

#[test]
fn actionable_work_replies_offer_recovery_and_review_without_github_mutation() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    let transition = WorkTransition {
        sequence: 7,
        work_item_id: "work-7".into(),
        object_id: "candidate-7".into(),
        old_state: Some("reviewing".into()),
        new_state: "ready".into(),
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some("Review candidate".into()),
        evidence_ref: Some("tests/output/candidate.log".into()),
        object_kind: "merge-candidate".into(),
        occurred_at: Some("2026-08-18T00:00:00Z".into()),
        elapsed_seconds: Some(12),
        candidate_commit: Some("abc123".into()),
        comparison_base: Some("main-base".into()),
        completed_tests: vec!["tester passed".into()],
        completed_reviews: vec!["architecture passed".into()],
    };
    let reply = control.render_actionable_transition(
        &transition,
        &["tests/output/candidate.log".into()],
        Some("https://github.com/acme/app"),
    );
    assert!(reply.contains("Candidate is ready for review"));
    assert!(reply.contains("Commit: abc123"));
    assert!(reply.contains("Tests: tester passed"));
    assert!(reply.contains("Reviews: architecture passed"));
    assert!(reply.contains("https://github.com/acme/app/commit/abc123"));
    assert!(reply.contains("https://github.com/acme/app/compare/main-base...abc123"));
    assert!(reply.contains("Optionally create a pull request"));
    assert!(reply.contains("Elapsed: 12s"));

    let needs_user = WorkTransition {
        new_state: "needs-user".into(),
        ..transition.clone()
    };
    assert!(
        control
            .render_actionable_transition(&needs_user, &[], None)
            .contains("requested recovery decision")
    );
    let failed = WorkTransition {
        new_state: "failed".into(),
        ..transition
    };
    assert!(
        control
            .render_actionable_transition(&failed, &["failure.log".into()], None)
            .contains("permitted recovery action")
    );
}

#[test]
fn completed_slice_stays_complete_for_observation_and_offers_linked_work() {
    let (_home, checkout, _config) = setup(true);
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(
            "slice-complete",
            "U1",
            &[PlannedWork::new("work-complete", "Work")],
        )
        .unwrap();
    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    store
        .mutate_work_item("work-complete", |item| {
            item.attempts.push(fluent::work_model::Attempt {
                id: "attempt-complete".into(),
                work_item_id: item.id.clone(),
                status: fluent::work_model::AttemptStatus::Complete,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(
        control.slice_stage("slice-complete").unwrap(),
        SliceStage::Complete
    );
    assert!(
        control
            .render_optional_observation("slice-complete", "observation-1")
            .unwrap()
            .contains("remains complete")
    );
    assert!(
        control
            .linked_slice_offer("slice-complete", "Add another feature")
            .unwrap()
            .contains("Start linked Slice")
    );
    store
        .mutate_work_item("work-complete", |item| {
            item.attempts[0].status = fluent::work_model::AttemptStatus::NeedsUser;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        control.slice_stage("slice-complete").unwrap(),
        SliceStage::Active
    );
}

fn planned_control<'a>(checkout: &'a TempDir, slice: &str, work: &str) -> WorkControl {
    let control = WorkControl::new(checkout.path());
    control
        .approve_plan(slice, "U1", &[PlannedWork::new(work, "Work")])
        .unwrap();
    control
}

#[test]
fn transition_renderer_includes_detailed_work_identity() {
    let (_home, checkout, _config) = setup(true);
    let control = planned_control(&checkout, "slice-detail", "work-detail");
    let transition = control
        .reconcile_durable_transitions("slice-detail")
        .unwrap()
        .pop()
        .unwrap();
    let reply = render_transition_reply(&transition, &["work.json".into()]);
    assert!(reply.contains("Work Item work-detail"));
    assert!(reply.contains("is now planned"));
    assert!(reply.contains("Evidence: work.json"));
}

#[test]
fn external_work_transition_updates_linked_thread() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-external",
            &[PlannedWork::new("work-external", "External")],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();
    let delivered_before_idle_tick = runner.slack().deliveries.len();
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_from_external(
            "work-external",
            "external controller approved execution",
            Some("external:request-42".into()),
            |item| {
                item.authorization = fluent::work_model::ExecutionAuthorization::execution_ready(
                    fluent::work_model::ExecutionAuthority::Human,
                );
                Ok(())
            },
        )
        .unwrap();
    runner.poll_linked_work().unwrap();
    let delivery = runner.slack().deliveries[delivered_before_idle_tick..]
        .iter()
        .find(|delivery| delivery.id.contains(":work-transition:"))
        .unwrap_or_else(|| {
            panic!(
                "idle deliveries: {:?}",
                &runner.slack().deliveries[delivered_before_idle_tick..]
            )
        });
    assert!(delivery.text.contains("Work Item work-external"));
    assert!(delivery.text.contains("Origin: external"));
    assert!(
        delivery
            .text
            .contains("Next action: external controller approved execution")
    );
    assert!(delivery.text.contains("Evidence: external:request-42"));
}

#[test]
fn typed_lifecycle_origins_replay_to_the_linked_slack_thread() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    let owners = [
        ("work-scheduler", "Scheduler"),
        ("work-attempt", "AttemptExecutor"),
        ("work-task", "TaskExecutor"),
        ("work-landing", "Landing"),
        ("work-post-land", "PostLand"),
    ];
    let planned = owners
        .iter()
        .map(|(id, _)| PlannedWork::new(*id, *id))
        .collect::<Vec<_>>();
    runner
        .approve_thread_plan("1.0", "U1", "slice-origins", &planned)
        .unwrap();
    runner.report_thread_work("1.0").unwrap();

    let store = fluent::work_model::WorkModelStore::new(checkout.path());
    for (work_item_id, variant) in owners {
        let origin = match variant {
            "Scheduler" => fluent::work_model::DurableTransitionOrigin::Scheduler,
            "AttemptExecutor" => fluent::work_model::DurableTransitionOrigin::AttemptExecutor,
            "TaskExecutor" => fluent::work_model::DurableTransitionOrigin::TaskExecutor,
            "Landing" => fluent::work_model::DurableTransitionOrigin::Landing,
            "PostLand" => fluent::work_model::DurableTransitionOrigin::PostLand,
            _ => unreachable!(),
        };
        store
            .mutate_work_item_with_transition(
                work_item_id,
                &fluent::work_model::DurableTransitionMetadata {
                    origin,
                    reason: Some(format!("{variant} changed durable state")),
                    evidence_ref: Some(format!("owner:{work_item_id}")),
                },
                |item| {
                    item.authorization =
                        fluent::work_model::ExecutionAuthorization::execution_ready(
                            fluent::work_model::ExecutionAuthority::Human,
                        );
                    Ok(())
                },
            )
            .unwrap();
    }

    runner.poll_linked_work().unwrap();
    for (work_item_id, variant) in owners {
        let rendered =
            variant
                .chars()
                .enumerate()
                .fold(String::new(), |mut value, (index, character)| {
                    if character.is_ascii_uppercase() && index > 0 {
                        value.push('-');
                    }
                    value.push(character.to_ascii_lowercase());
                    value
                });
        assert!(runner.slack().deliveries.iter().any(|delivery| {
            delivery.text.contains(work_item_id)
                && delivery.text.contains(&format!("Origin: {rendered}"))
                && delivery.text.contains(&format!("owner:{work_item_id}"))
        }));
    }
}

#[test]
fn executor_origin_replays_the_object_specific_pause_reason() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-pause-origin",
            &[PlannedWork::new("work-pause-origin", "Paused Work")],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();

    let metadata = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::AttemptExecutor,
        reason: None,
        evidence_ref: Some("attempt:evidence".into()),
    };
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_with_transition("work-pause-origin", &metadata, |item| {
            item.attempts.push(fluent::work_model::Attempt {
                id: "attempt-pause-origin".into(),
                work_item_id: item.id.clone(),
                status: fluent::work_model::AttemptStatus::NeedsUser,
                pause_kind: Some(fluent::work_model::PauseKind::ProviderUnavailable),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();

    runner.poll_linked_work().unwrap();
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("attempt attempt-pause-origin")
            && delivery.text.contains("Origin: attempt-executor")
            && delivery.text.contains("Next action: provider-unavailable")
            && delivery.text.contains("Evidence: attempt:evidence")
    }));
}

#[test]
fn status_renderer_tracks_current_slice_state() {
    let (_home, checkout, _config) = setup(true);
    let control = planned_control(&checkout, "slice-status-direct", "work-status-direct");
    let status = control
        .render_status_message("slice-status-direct", "app", "main", "connected")
        .unwrap();
    assert!(status.contains("Slice: Active"));
    assert!(status.contains("work-status-direct: planned"));
    assert!(status.contains("Await execution approval"));
}

fn append_attempt_state(
    checkout: &TempDir,
    work: &str,
    state: fluent::work_model::AttemptStatus,
    reason: &str,
) {
    let metadata = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some(reason.into()),
        evidence_ref: Some(format!("evidence:{work}")),
    };
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_with_transition(work, &metadata, |item| {
            item.authorization = fluent::work_model::ExecutionAuthorization::execution_ready(
                fluent::work_model::ExecutionAuthority::Human,
            );
            item.attempts.push(fluent::work_model::Attempt {
                id: format!("attempt-{work}"),
                work_item_id: item.id.clone(),
                status: state,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
}

#[test]
fn needs_user_transition_posts_actionable_request() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-question",
            &[PlannedWork::new("work-question", "Question")],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();
    let before = runner.slack().deliveries.len();
    append_attempt_state(
        &checkout,
        "work-question",
        fluent::work_model::AttemptStatus::NeedsUser,
        "choose retry or stop",
    );
    runner.report_thread_work("1.0").unwrap();
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("work-question")
            && delivery.text.contains("choose retry or stop")
            && delivery.text.contains("requested recovery decision")
    }));
}

fn ready_transition() -> WorkTransition {
    WorkTransition {
        sequence: 1,
        work_item_id: "work-ready".into(),
        object_id: "candidate-ready".into(),
        old_state: Some("reviewing".into()),
        new_state: "ready".into(),
        origin: fluent::work_model::DurableTransitionOrigin::Fluent,
        reason: Some("review candidate".into()),
        evidence_ref: Some("candidate.json".into()),
        object_kind: "merge-candidate".into(),
        occurred_at: Some("2026-08-18T00:00:00Z".into()),
        elapsed_seconds: Some(4),
        candidate_commit: Some("abc123".into()),
        comparison_base: Some("base123".into()),
        completed_tests: vec!["tester passed".into()],
        completed_reviews: vec!["reviews passed".into()],
    }
}

#[test]
fn ready_candidate_renderer_includes_reviewable_evidence_summary() {
    let (_home, checkout, _config) = setup(true);
    let reply = WorkControl::new(checkout.path()).render_actionable_transition(
        &ready_transition(),
        &["candidate.json".into()],
        None,
    );
    assert!(reply.contains("Commit: abc123"));
    assert!(reply.contains("Tests: tester passed"));
    assert!(reply.contains("Reviews: reviews passed"));
}

#[test]
fn ready_candidate_renderer_offers_github_review_without_creating_pr() {
    let (_home, checkout, _config) = setup(true);
    let reply = WorkControl::new(checkout.path()).render_actionable_transition(
        &ready_transition(),
        &[],
        Some("https://github.com/acme/app"),
    );
    assert!(reply.contains("/commit/abc123"));
    assert!(reply.contains("/compare/base123...abc123"));
    assert!(reply.contains("Optionally create a pull request"));
}

fn assert_runner_posts_durable_ready_candidate_evidence_and_links() {
    use fluent::work_model::{
        ArtifactRef, Attempt, AttemptReviewState, AttemptStatus, MergeCandidate,
        MergeCandidateMergeState, MergeCandidateMergeStatus, MergeReviewState, Task, TaskKind,
        TaskOutput, TaskStatus, WorkspaceAccess, WorkspaceRef,
    };

    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-ready-runner",
            &[PlannedWork::new("work-ready-runner", "Ready")],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();

    let workspace = WorkspaceRef {
        id: "workspace-ready".into(),
        path: "/tmp/workspace-ready".into(),
    };
    let target = WorkspaceRef {
        id: "target-ready".into(),
        path: checkout.path().display().to_string(),
    };
    let metadata = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Fluent,
        reason: Some("candidate passed required gates".into()),
        evidence_ref: Some("candidate/evidence.json".into()),
    };
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_with_transition("work-ready-runner", &metadata, |item| {
            item.authorization = fluent::work_model::ExecutionAuthorization::execution_ready(
                fluent::work_model::ExecutionAuthority::Human,
            );
            item.attempts.push(Attempt {
                id: "attempt-ready".into(),
                work_item_id: item.id.clone(),
                status: AttemptStatus::Complete,
                review_state: Some(AttemptReviewState::Passed),
                tasks: vec![Task {
                    id: "write-ready".into(),
                    kind: TaskKind::Write,
                    status: TaskStatus::Complete,
                    role: "writer".into(),
                    instructions: None,
                    work_item_id: item.id.clone(),
                    attempt_id: Some("attempt-ready".into()),
                    workspace_access: WorkspaceAccess {
                        reads: vec![target.clone()],
                        writes: vec![workspace.clone()],
                    },
                    artifact_area: None,
                    review_context: None,
                    evidence_review_context: None,
                    input_artifacts: Vec::new(),
                    depends_on: None,
                    output: Some(TaskOutput {
                        workspace_id: workspace.id.clone(),
                        workspace_path: workspace.path.clone(),
                        source_branch: "main".into(),
                        base_commit: Some("base123".into()),
                        commit: "abc123".into(),
                        no_change: None,
                        learner_canonicalization: None,
                    }),
                    created_at: None,
                    started_at: None,
                    completed_at: None,
                }],
                ..Default::default()
            });
            item.merge_candidates.push(MergeCandidate {
                id: "candidate-ready-runner".into(),
                attempt_id: "attempt-ready".into(),
                source_workspace: workspace.clone(),
                target_workspace: target.clone(),
                source_branch: "main".into(),
                target_branch: "main".into(),
                candidate_commit: "abc123".into(),
                merge_review_state: MergeReviewState::Passed,
                merge_state: MergeCandidateMergeState {
                    status: MergeCandidateMergeStatus::Pending,
                    check_artifacts: vec![ArtifactRef {
                        producer_id: "tester".into(),
                        path: "tester/result.json".into(),
                    }],
                    review_artifacts: vec![ArtifactRef {
                        producer_id: "reviews".into(),
                        path: "reviews/result.json".into(),
                    }],
                    ..Default::default()
                },
                created_at: None,
                ready_at: None,
                started_at: None,
                completed_at: None,
            });
            Ok(())
        })
        .unwrap();

    runner.report_thread_work("1.0").unwrap();
    let reply = runner
        .slack()
        .deliveries
        .iter()
        .find(|delivery| delivery.text.contains("candidate-ready-runner"))
        .unwrap();
    assert!(reply.text.contains("is now ready"));
    assert!(reply.text.contains("Commit: abc123"));
    assert!(reply.text.contains("Tests: tester/result.json"));
    assert!(reply.text.contains("Reviews: reviews/result.json"));
    assert!(reply.text.contains("Evidence: candidate/evidence.json"));
    assert!(
        reply
            .text
            .contains("https://github.com/acme/app/commit/abc123")
    );
    assert!(reply.text.contains("/compare/main...abc123"));
    assert!(reply.text.contains("Optionally create a pull request"));

    let control = WorkControl::new(checkout.path());
    assert_eq!(
        control.slice_stage("slice-ready-runner").unwrap(),
        SliceStage::Active,
        "an unlanded candidate keeps the Slice active"
    );
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item("work-ready-runner", |item| {
            item.merge_candidates[0].merge_state.status = MergeCandidateMergeStatus::Merged;
            item.merge_candidates[0].merge_state.follow_up_failure =
                Some(fluent::work_model::FollowUpProcessingFailure {
                    stage: "observation".into(),
                    message: "post-land processing failed".into(),
                    next_action: "retry post-land processing".into(),
                });
            Ok(())
        })
        .unwrap();
    assert_eq!(
        control.slice_stage("slice-ready-runner").unwrap(),
        SliceStage::Active,
        "a post-land failure keeps the Slice active"
    );
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item("work-ready-runner", |item| {
            item.merge_candidates[0].merge_state.follow_up_failure = None;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        control.slice_stage("slice-ready-runner").unwrap(),
        SliceStage::Complete
    );
}

#[test]
fn ready_candidate_posts_reviewable_evidence_summary() {
    assert_runner_posts_durable_ready_candidate_evidence_and_links();
}

#[test]
fn ready_candidate_offers_github_review_without_creating_pr() {
    assert_runner_posts_durable_ready_candidate_evidence_and_links();
}

#[test]
fn failed_work_posts_evidence_and_permitted_recovery() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-failed",
            &[PlannedWork::new("work-failed", "Failed")],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();
    let before = runner.slack().deliveries.len();
    append_attempt_state(
        &checkout,
        "work-failed",
        fluent::work_model::AttemptStatus::Failed,
        "retry after fixing credentials",
    );
    runner.report_thread_work("1.0").unwrap();
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("work-failed")
            && delivery.text.contains("retry after fixing credentials")
            && delivery.text.contains("Evidence: evidence:work-failed")
            && delivery.text.contains("permitted recovery action")
    }));
}

#[test]
fn parallel_work_transitions_remain_attributed() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-parallel-runner",
            &[
                PlannedWork::new("work-left", "Left"),
                PlannedWork::new("work-right", "Right"),
            ],
        )
        .unwrap();
    runner.report_thread_work("1.0").unwrap();
    append_attempt_state(
        &checkout,
        "work-left",
        fluent::work_model::AttemptStatus::Executing,
        "left started",
    );
    append_attempt_state(
        &checkout,
        "work-right",
        fluent::work_model::AttemptStatus::NeedsUser,
        "right needs input",
    );
    let before = runner.slack().deliveries.len();
    runner.report_thread_work("1.0").unwrap();
    let replies = &runner.slack().deliveries[before..];
    assert!(replies.iter().any(|reply| {
        reply.thread_timestamp == "1.0"
            && reply.text.contains("Work Item work-left")
            && reply.text.contains("is now executing")
    }));
    assert!(replies.iter().any(|reply| {
        reply.thread_timestamp == "1.0"
            && reply.text.contains("Work Item work-right")
            && reply.text.contains("is now needs-user")
    }));
}

#[test]
fn reconnect_reconciles_work_transitions_exactly_once() {
    let (home, checkout, config) = setup(true);
    let mut first = CollaborationRunner::new(
        home.path(),
        config.clone(),
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    first
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    first
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-reconnect-runner",
            &[
                PlannedWork::new("work-reconnect-runner", "Reconnect"),
                PlannedWork::new("work-reconnect-candidate", "Candidate"),
            ],
        )
        .unwrap();
    first.report_thread_work("1.0").unwrap();
    drop(first);

    append_attempt_state(
        &checkout,
        "work-reconnect-runner",
        fluent::work_model::AttemptStatus::Executing,
        "worker started",
    );
    let metadata = fluent::work_model::DurableTransitionMetadata {
        origin: fluent::work_model::DurableTransitionOrigin::Scheduler,
        reason: Some("operator decision required".into()),
        evidence_ref: Some("recovery.json".into()),
    };
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_with_transition("work-reconnect-runner", &metadata, |item| {
            item.attempts[0].status = fluent::work_model::AttemptStatus::NeedsUser;
            Ok(())
        })
        .unwrap();

    let source = fluent::work_model::WorkspaceRef {
        id: "reconnect-source".into(),
        path: "/tmp/reconnect-source".into(),
    };
    let target = fluent::work_model::WorkspaceRef {
        id: "reconnect-target".into(),
        path: checkout.path().display().to_string(),
    };
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item_with_transition("work-reconnect-candidate", &metadata, |item| {
            item.authorization = fluent::work_model::ExecutionAuthorization::execution_ready(
                fluent::work_model::ExecutionAuthority::Human,
            );
            item.attempts.push(fluent::work_model::Attempt {
                id: "attempt-reconnect-candidate".into(),
                work_item_id: item.id.clone(),
                status: fluent::work_model::AttemptStatus::Complete,
                review_state: Some(fluent::work_model::AttemptReviewState::Passed),
                tasks: vec![fluent::work_model::Task {
                    id: "task-reconnect-write".into(),
                    kind: fluent::work_model::TaskKind::Write,
                    status: fluent::work_model::TaskStatus::Complete,
                    role: "writer".into(),
                    instructions: None,
                    work_item_id: item.id.clone(),
                    attempt_id: Some("attempt-reconnect-candidate".into()),
                    workspace_access: fluent::work_model::WorkspaceAccess {
                        reads: vec![target.clone()],
                        writes: vec![source.clone()],
                    },
                    artifact_area: None,
                    review_context: None,
                    evidence_review_context: None,
                    input_artifacts: Vec::new(),
                    depends_on: None,
                    output: Some(fluent::work_model::TaskOutput {
                        workspace_id: source.id.clone(),
                        workspace_path: source.path.clone(),
                        source_branch: "main".into(),
                        base_commit: Some("base-reconnect".into()),
                        commit: "commit-reconnect".into(),
                        no_change: None,
                        learner_canonicalization: None,
                    }),
                    created_at: None,
                    started_at: None,
                    completed_at: None,
                }],
                ..Default::default()
            });
            item.merge_candidates
                .push(fluent::work_model::MergeCandidate {
                    id: "candidate-reconnect".into(),
                    attempt_id: "attempt-reconnect-candidate".into(),
                    source_workspace: source.clone(),
                    target_workspace: target.clone(),
                    source_branch: "main".into(),
                    target_branch: "main".into(),
                    candidate_commit: "commit-reconnect".into(),
                    merge_review_state: fluent::work_model::MergeReviewState::Passed,
                    merge_state: fluent::work_model::MergeCandidateMergeState::default(),
                    created_at: None,
                    ready_at: None,
                    started_at: None,
                    completed_at: None,
                });
            Ok(())
        })
        .unwrap();

    let mut recovered = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    recovered.reconnect([], "reconnected".into()).unwrap();
    let transition_deliveries = recovered
        .slack()
        .deliveries
        .iter()
        .filter(|delivery| delivery.id.contains(":work-transition:"))
        .collect::<Vec<_>>();
    assert_eq!(transition_deliveries.len(), 7);
    assert!(transition_deliveries[0].text.contains("is now authorized"));
    assert!(transition_deliveries[1].text.contains("is now executing"));
    assert!(transition_deliveries[2].text.contains("is now needs-user"));
    assert!(
        transition_deliveries[2]
            .text
            .contains("operator decision required")
    );
    let task_position = transition_deliveries
        .iter()
        .position(|delivery| delivery.text.contains("task-reconnect-write"))
        .unwrap();
    let candidate_position = transition_deliveries
        .iter()
        .position(|delivery| delivery.text.contains("candidate-reconnect"))
        .unwrap();
    assert!(task_position < candidate_position);
    assert!(
        transition_deliveries[task_position]
            .text
            .contains("Work Item work-reconnect-candidate")
    );
    assert!(
        transition_deliveries[candidate_position]
            .text
            .contains("is now ready")
    );

    let delivered = recovered.slack().deliveries.len();
    recovered.reconnect([], "reconnected-again".into()).unwrap();
    assert_eq!(
        recovered.slack().deliveries[delivered..]
            .iter()
            .filter(|delivery| delivery.id.contains(":work-transition:"))
            .count(),
        0
    );
}

#[test]
fn finished_planned_work_marks_slice_complete() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-finished",
            &[
                PlannedWork::new("work-finished", "First"),
                PlannedWork::new("work-sibling", "Sibling"),
            ],
        )
        .unwrap();
    append_attempt_state(
        &checkout,
        "work-finished",
        fluent::work_model::AttemptStatus::Complete,
        "completed",
    );
    runner.report_thread_work("1.0").unwrap();
    let control = WorkControl::new(checkout.path());
    assert_eq!(
        control.slice_stage("slice-finished").unwrap(),
        SliceStage::Active
    );
    append_attempt_state(
        &checkout,
        "work-sibling",
        fluent::work_model::AttemptStatus::Complete,
        "completed",
    );
    let before = runner.slack().deliveries.len();
    runner.report_thread_work("1.0").unwrap();
    assert_eq!(
        control.slice_stage("slice-finished").unwrap(),
        SliceStage::Complete
    );
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.update_timestamp.is_some()
            && delivery.text.contains("Slice: Complete")
            && delivery.text.contains("work-finished: complete")
            && delivery.text.contains("work-sibling: complete")
    }));
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item("work-finished", |item| {
            item.attempts.push(fluent::work_model::Attempt {
                id: "attempt-latest-required".into(),
                work_item_id: item.id.clone(),
                status: fluent::work_model::AttemptStatus::Failed,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let before = runner.slack().deliveries.len();
    runner.report_thread_work("1.0").unwrap();
    assert_eq!(
        control.slice_stage("slice-finished").unwrap(),
        SliceStage::Active,
        "a failed latest required Attempt keeps the Slice active"
    );
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.update_timestamp.is_some()
            && delivery.text.contains("Slice: Active")
            && delivery.text.contains("work-finished: failed")
    }));
    fluent::work_model::WorkModelStore::new(checkout.path())
        .mutate_work_item("work-finished", |item| {
            item.attempts.last_mut().unwrap().status = fluent::work_model::AttemptStatus::NeedsUser;
            Ok(())
        })
        .unwrap();
    let before = runner.slack().deliveries.len();
    runner.report_thread_work("1.0").unwrap();
    assert_eq!(
        control.slice_stage("slice-finished").unwrap(),
        SliceStage::Active,
        "a needs-user latest required Attempt keeps the Slice active"
    );
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.update_timestamp.is_some()
            && delivery.text.contains("Slice: Active")
            && delivery.text.contains("work-finished: needs-user")
    }));
}

#[test]
fn completed_thread_still_answers_slice_status_questions() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-answer",
            &[PlannedWork::new("work-answer", "Answer")],
        )
        .unwrap();
    append_attempt_state(
        &checkout,
        "work-answer",
        fluent::work_model::AttemptStatus::Complete,
        "completed",
    );
    runner.report_thread_work("1.0").unwrap();
    let before = runner.slack().deliveries.len();
    runner
        .ingest(event("E2", Some("1.0"), "work status"))
        .unwrap();
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("Slice: Complete")
            && delivery.text.contains("work-answer: complete")
    }));
}

#[test]
fn new_work_after_completion_offers_linked_slice_thread() {
    let (home, checkout, config) = setup(true);
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-linked",
            &[PlannedWork::new("work-linked", "Linked")],
        )
        .unwrap();
    append_attempt_state(
        &checkout,
        "work-linked",
        fluent::work_model::AttemptStatus::Complete,
        "completed",
    );
    runner.report_thread_work("1.0").unwrap();
    let before = runner.slack().deliveries.len();
    let provider_calls = runner.provider().calls.len();
    runner
        .ingest(event("E2", Some("1.0"), "new work add product analytics"))
        .unwrap();
    assert_eq!(runner.provider().calls.len(), provider_calls);
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery
                .text
                .contains("Slice slice-linked remains complete")
            && delivery.text.contains("Start linked Slice")
    }));
    assert_eq!(
        WorkControl::new(checkout.path())
            .slice_stage("slice-linked")
            .unwrap(),
        SliceStage::Complete
    );
}

fn commit_follow_up_authority(checkout: &TempDir) -> (String, fluent::follow_up::AuthorityLocator) {
    let anchor = "Correct post-land regressions through focused Work.";
    fs::write(checkout.path().join("AGENTS.md"), format!("{anchor}\n")).unwrap();
    for args in [
        vec!["add", "AGENTS.md"],
        vec!["commit", "-m", "Add follow-up authority"],
    ] {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(checkout.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let authority = fluent::follow_up::AuthorityLocator {
        kind: fluent::follow_up::AuthorityKind::AgentsInstruction,
        path: "AGENTS.md".into(),
        anchor: anchor.into(),
        digest: fluent::follow_up::content_digest(anchor.as_bytes()),
    };
    (commit, authority)
}

fn post_land_batch(
    work_item_id: &str,
    merged_commit: &str,
    follow_up: fluent::follow_up::FollowUpDraftV1,
) -> fluent::follow_up::NormalizedFollowUpBatchV1 {
    fluent::follow_up::NormalizedFollowUpBatchV1 {
        schema_version: fluent::follow_up::NormalizedFollowUpBatchV1::SCHEMA_VERSION,
        source: fluent::follow_up::FollowUpSource::Learner,
        origin: fluent::follow_up::PostLandOrigin {
            work_item_id: work_item_id.into(),
            attempt_id: format!("attempt-{work_item_id}"),
            merge_candidate_id: format!("candidate-{work_item_id}"),
            merged_commit: merged_commit.into(),
        },
        learning_summary: "post-land policy result".into(),
        follow_ups: vec![follow_up],
    }
}

#[test]
fn corrective_follow_up_reactivates_original_slice() {
    let (home, checkout, mut config) = setup(true);
    let (merged_commit, authority) = commit_follow_up_authority(&checkout);
    config.projects.get_mut("app").unwrap().checkout.commit = merged_commit.clone();
    write_config(home.path(), &config).unwrap();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-corrective-runner",
            &[PlannedWork::new("work-original", "Original")],
        )
        .unwrap();
    append_attempt_state(
        &checkout,
        "work-original",
        fluent::work_model::AttemptStatus::Complete,
        "landed",
    );
    let control = WorkControl::new(checkout.path());
    assert_eq!(
        control.slice_stage("slice-corrective-runner").unwrap(),
        SliceStage::Complete
    );
    let batch = post_land_batch(
        "work-original",
        &merged_commit,
        fluent::follow_up::FollowUpDraftV1 {
            id: "correct-regression".into(),
            summary: "Correct the post-land regression".into(),
            corrective: true,
            corrective_context: Some(fluent::work_model::CorrectiveContext {
                objective: "Correct the post-land regression".into(),
                requirement: authority.anchor.clone(),
                evidence: "The landed behavior regressed".into(),
                included_scope: "README".into(),
                excluded_scope: "Unrelated behavior".into(),
                verification: "cargo test focused_post_land".into(),
            }),
            target_paths: vec!["README".into()],
            expected_result: "The regression is corrected".into(),
            authority: Some(authority),
            ..Default::default()
        },
    );
    let outcome = fluent::follow_up::process_landed_batch(checkout.path(), &batch, None).unwrap();
    assert_eq!(outcome.work_items_created, 1);

    runner.poll_linked_work().unwrap();

    assert_eq!(
        control.slice_stage("slice-corrective-runner").unwrap(),
        SliceStage::Active
    );
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery
            .text
            .contains("Linked corrective Work Item derived-")
    }));
    assert!(runner.slack().deliveries.iter().any(|delivery| {
        delivery.text.contains("Work Item derived-") && delivery.text.contains("is now planned")
    }));
    let delivered = runner.slack().deliveries.len();
    runner.poll_linked_work().unwrap();
    assert_eq!(runner.slack().deliveries.len(), delivered);
}

#[test]
fn optional_observation_does_not_reopen_completed_slice() {
    let (home, checkout, config) = setup(true);
    let merged_commit = config.projects["app"].checkout.commit.clone();
    let mut runner = CollaborationRunner::new(
        home.path(),
        config,
        InMemoryProvider::default(),
        InMemorySlack::default(),
    );
    runner
        .ingest(event("E1", None, "@Fluent plan work"))
        .unwrap();
    runner
        .approve_thread_plan(
            "1.0",
            "U1",
            "slice-observation",
            &[PlannedWork::new("work-observation", "Observation")],
        )
        .unwrap();
    append_attempt_state(
        &checkout,
        "work-observation",
        fluent::work_model::AttemptStatus::Complete,
        "completed",
    );
    runner.report_thread_work("1.0").unwrap();
    let before = runner.slack().deliveries.len();
    let batch = post_land_batch(
        "work-observation",
        &merged_commit,
        fluent::follow_up::FollowUpDraftV1 {
            id: "optional-note".into(),
            summary: "Remember an optional improvement".into(),
            ..Default::default()
        },
    );
    let outcome = fluent::follow_up::process_landed_batch(checkout.path(), &batch, None).unwrap();
    assert_eq!(outcome.observations_created, 1);

    runner.poll_linked_work().unwrap();
    assert!(runner.slack().deliveries[before..].iter().any(|delivery| {
        delivery.thread_timestamp == "1.0"
            && delivery.text.contains("remains complete")
            && delivery.text.contains("Observation: followup-")
    }));
    let control = WorkControl::new(checkout.path());
    assert_eq!(
        control.slice_stage("slice-observation").unwrap(),
        SliceStage::Complete
    );
    let delivered = runner.slack().deliveries.len();
    runner.poll_linked_work().unwrap();
    assert_eq!(runner.slack().deliveries.len(), delivered);
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
        ) -> ProviderResult<(String, String)> {
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
        ) -> ProviderResult<String> {
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
