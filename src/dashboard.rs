mod app;
mod layout;
mod render;
mod snapshot;
mod transitions;

use anyhow::Result;
use app::{App, Effect};
use crossterm::event::{self, Event as CEvent};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::work_status;

const DATA_POLL_INTERVAL: Duration = Duration::from_secs(2);

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    cleanup: TerminalCleanup,
}

struct TerminalCleanup {
    raw: bool,
    alternate: bool,
    mouse: bool,
}
impl TerminalCleanup {
    fn restore(&mut self) {
        let mut stdout = io::stdout();
        if self.mouse {
            let _ = crossterm::execute!(stdout, crossterm::event::DisableMouseCapture);
            self.mouse = false;
        }
        if self.alternate {
            let _ = crossterm::execute!(stdout, LeaveAlternateScreen);
            self.alternate = false;
        }
        if self.raw {
            let _ = disable_raw_mode();
            self.raw = false;
        }
    }
}
impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        self.restore();
    }
}

impl TerminalSession {
    fn open() -> Result<Self> {
        let mut cleanup = TerminalCleanup {
            raw: false,
            alternate: false,
            mouse: false,
        };
        enable_raw_mode()?;
        cleanup.raw = true;
        let mut stdout = io::stdout();
        cleanup.alternate = true;
        cleanup.mouse = true;
        if let Err(error) = crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        ) {
            return Err(error.into());
        }
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
            cleanup,
        })
    }

    fn set_mouse_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.cleanup.mouse != enabled {
            if enabled {
                crossterm::execute!(
                    self.terminal.backend_mut(),
                    crossterm::event::EnableMouseCapture
                )?;
            } else {
                crossterm::execute!(
                    self.terminal.backend_mut(),
                    crossterm::event::DisableMouseCapture
                )?;
            }
            self.cleanup.mouse = enabled;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
    }
}

/// Launch the read-only Work operator console.
pub fn run_dashboard(search_root: &Path) -> Result<()> {
    let snapshot = load_snapshot(search_root)?;
    let mut session = TerminalSession::open()?;
    let mut app = App::new(snapshot);
    let size = session.terminal.size()?;
    app.resize(size.width, size.height);
    let mut next_poll = Instant::now() + DATA_POLL_INTERVAL;

    loop {
        if app.take_dirty() {
            session.terminal.draw(|frame| render::draw(frame, &app))?;
        }
        let timeout = next_poll.saturating_duration_since(Instant::now());
        if event::poll(timeout)? {
            match event::read()? {
                CEvent::Key(key) => {
                    let effect = app.handle_key(key.code, key.modifiers);
                    if let Effect::ToggleMouse(enabled) = effect {
                        session.set_mouse_enabled(enabled)?;
                    }
                    if matches!(effect, Effect::Refresh) {
                        refresh(search_root, &mut app);
                        next_poll = Instant::now() + DATA_POLL_INTERVAL;
                    }
                }
                CEvent::Resize(width, height) => app.resize(width, height),
                _ => {}
            }
        }
        if poll_due(Instant::now(), next_poll) {
            refresh(search_root, &mut app);
            next_poll = Instant::now() + DATA_POLL_INTERVAL;
        }
        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

fn poll_due(now: Instant, deadline: Instant) -> bool {
    now >= deadline
}

fn refresh(root: &Path, app: &mut App) {
    match load_snapshot(root) {
        Ok(snapshot) => app.refresh(snapshot),
        Err(error) => app.refresh_failed(error.to_string()),
    }
}

fn load_snapshot(root: &Path) -> Result<snapshot::DashboardSnapshot> {
    let work = crate::work_model::WorkModelStore::new(root).snapshot_work_items();
    let transitions = transitions::latest_transitions(&work.items);
    let status = work_status::summarize_work_snapshot(work, root);
    Ok(snapshot::DashboardSnapshot::from_status_and_transitions(
        status,
        transitions,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_status::WorkItemStatus;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    #[test]
    fn dashboard_snapshot_is_nonmutating() {
        fn tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
            fn visit(
                root: &Path,
                path: &Path,
                out: &mut std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>>,
            ) {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    if path.is_dir() {
                        None
                    } else {
                        Some(std::fs::read(path).unwrap())
                    },
                );
                if path.is_dir() {
                    for entry in std::fs::read_dir(path).unwrap() {
                        visit(root, &entry.unwrap().path(), out);
                    }
                }
            }
            let mut out = Default::default();
            visit(root, &root.join(".fluent/work"), &mut out);
            out
        }
        let root = tempfile::tempdir().unwrap();
        let store = crate::work_model::WorkModelStore::new(root.path());
        store
            .create_work_item_with_transition(
                &crate::work_model::WorkItem::planned("visible", "Published"),
                &crate::work_model::DurableTransitionMetadata {
                    origin: crate::work_model::DurableTransitionOrigin::Fluent,
                    reason: None,
                    evidence_ref: None,
                },
            )
            .unwrap();
        std::fs::write(
            root.path().join(".fluent/work/transactions/visible.json"),
            "unfinished journal",
        )
        .unwrap();
        std::fs::remove_dir_all(root.path().join(".fluent/work/locks")).unwrap();
        let before = tree(root.path());
        let snapshot = load_snapshot(root.path()).unwrap();
        assert_eq!(tree(root.path()), before);
        assert_eq!(snapshot.rows[0].status.title, "Published");
        assert!(snapshot.rows[0].latest_transition.is_some());
        assert!(
            snapshot
                .errors
                .iter()
                .any(|d| d.contains("host-owned recovery is pending"))
        );
    }

    fn row(id: &str, action: &str) -> WorkItemStatus {
        WorkItemStatus {
            id: id.into(),
            title: format!("{id} title"),
            attempt: "attempt [running]".into(),
            task: "write [running]".into(),
            review: "pending".into(),
            merge_candidate: "candidate".into(),
            merge: "pending".into(),
            action: action.into(),
            next_action: None,
            metrics: Default::default(),
            compatibility_warnings: vec![],
            release: None,
        }
    }
    fn app(rows: Vec<WorkItemStatus>) -> App {
        App::new(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus {
                rows,
                errors: vec![],
            },
        ))
    }
    fn app_with_errors(rows: Vec<WorkItemStatus>, errors: Vec<String>) -> App {
        App::new(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus { rows, errors },
        ))
    }
    fn text(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render::draw(f, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }
    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dashboard_opens_on_current_work() {
        let app = app(vec![row("ready", "task-ready"), row("done", "complete")]);
        assert!(text(&app, 100, 24).contains("Ready (1)"));
        assert!(!text(&app, 100, 24).contains("done title"));
    }

    #[test]
    fn dashboard_projects_latest_published_transition_on_every_load() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::work_model::WorkModelStore::new(root.path());
        let metadata = crate::work_model::DurableTransitionMetadata {
            origin: crate::work_model::DurableTransitionOrigin::SlackThread,
            reason: Some("plan approved".into()),
            evidence_ref: Some("slack-slice:dashboard".into()),
        };
        store
            .create_work_item_with_transition(
                &crate::work_model::WorkItem {
                    id: "work-dashboard".into(),
                    title: "Dashboard transitions".into(),
                    authorization: crate::work_model::ExecutionAuthorization::Proposed,
                    ..Default::default()
                },
                &metadata,
            )
            .unwrap();

        let first = load_snapshot(root.path()).unwrap();
        let transition = first.rows[0].latest_transition.as_ref().unwrap();
        assert_eq!(transition.work_item_id, "work-dashboard");
        assert_eq!(transition.transition.new_state, "planned");
        assert_eq!(transition.transition.sequence, 1);
        assert_eq!(load_snapshot(root.path()).unwrap(), first);
        assert!(!root.path().join(".fluent/work/dashboard").exists());
        assert!(!root.path().join(".fluent/slack").exists());

        store
            .mutate_work_item_with_transition("work-dashboard", &metadata, |item| {
                item.authorization = crate::work_model::ExecutionAuthorization::execution_ready(
                    crate::work_model::ExecutionAuthority::Human,
                );
                Ok(())
            })
            .unwrap();
        let advanced = load_snapshot(root.path()).unwrap();
        let transition = advanced.rows[0].latest_transition.as_ref().unwrap();
        assert_eq!(transition.transition.sequence, 2);
        assert_eq!(transition.transition.new_state, "authorized");

        let snapshot = load_snapshot(root.path()).unwrap();
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.status.id == "work-dashboard")
            .unwrap();
        assert!(
            layout::detail_values(row)
                .iter()
                .any(|line| line.contains("Latest transition: work-item work-dashboard"))
        );
    }
    #[test]
    fn current_work_is_grouped_with_counts() {
        let app = app(vec![
            row("need", "needs-user"),
            row("run", "executing"),
            row("ready", "planned"),
        ]);
        let rendered = text(&app, 100, 24);
        assert!(rendered.contains("Needs you (1)"));
        assert!(rendered.contains("Running (1)"));
        assert!(rendered.contains("Ready (1)"));
        assert!(
            rendered.find("Needs you (1)") < rendered.find("Running (1)")
                && rendered.find("Running (1)") < rendered.find("Ready (1)")
        );
    }
    #[test]
    fn empty_project_has_no_selection() {
        let app = app(vec![]);
        assert!(app.selected_id().is_none());
        assert!(text(&app, 100, 24).contains("No Work Items found"));
    }
    #[test]
    fn unknown_current_action_remains_visible() {
        let app = app(vec![row("new", "future-action")]);
        assert!(text(&app, 100, 24).contains("future-action"));
    }
    #[test]
    fn initial_selection_uses_first_non_empty_section() {
        let app = app(vec![row("ready", "planned"), row("need", "failed")]);
        assert_eq!(app.selected_id(), Some("need"));
    }
    #[test]
    fn wide_layout_shows_list_and_detail() {
        let app = app(vec![row("work", "task-ready")]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render::draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer_text(buffer);
        assert!(rendered.contains("Current Work"));
        assert!(rendered.contains("Work Item detail"));
        assert!(rendered.contains("Attempt: attempt [running]"));
        assert!(buffer[(42, 4)].symbol().contains('│'));
    }
    #[test]
    fn detail_shows_canonical_next_action() {
        let mut status = row("work", "task-ready");
        status.next_action = Some("fluent attempt run work".into());
        let app = app(vec![status]);
        assert!(text(&app, 100, 24).contains("fluent attempt run work"));
    }
    #[test]
    fn detail_does_not_invent_next_action() {
        let app = app(vec![row("work", "executing")]);
        assert!(text(&app, 100, 24).contains("No operator action"));
    }
    #[test]
    fn selection_moves_across_groups_and_scrolls() {
        let mut app = app(vec![row("need", "failed"), row("run", "executing")]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected_id(), Some("run"));
        assert_eq!(app.list_scroll, 0);
    }
    #[test]
    fn selection_survives_refresh_reorder_and_filter() {
        let mut app = app(vec![row("first", "planned"), row("selected", "planned")]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.refresh(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus {
                rows: vec![row("selected", "executing"), row("first", "planned")],
                errors: vec![],
            },
        ));
        assert_eq!(app.selected_id(), Some("selected"));
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(app.selected_id(), Some("selected"));
    }
    #[test]
    fn removed_selection_uses_nearest_remaining_row() {
        let mut app = app(vec![
            row("first", "planned"),
            row("selected", "planned"),
            row("last", "planned"),
        ]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.refresh(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus {
                rows: vec![row("first", "planned"), row("last", "planned")],
                errors: vec![],
            },
        ));
        assert_eq!(app.selected_id(), Some("last"));
    }
    #[test]
    fn narrow_layout_switches_between_list_and_detail() {
        let mut app = app(vec![row("work", "planned")]);
        app.resize(80, 24);
        assert!(text(&app, 80, 24).contains("Work Items"));
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(text(&app, 80, 24).contains("Work Item detail"));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(text(&app, 80, 24).contains("Work Items"));
    }
    #[test]
    fn undersized_terminal_shows_resize_message() {
        let app = app(vec![row("work", "planned")]);
        assert!(text(&app, 59, 14).contains("Resize terminal"));
    }
    #[test]
    fn overflow_detail_remains_navigable() {
        let mut status = row("work", "planned");
        status.compatibility_warnings = (0..30).map(|n| format!("warning {n}")).collect();
        let mut app = app(vec![status]);
        app.resize(80, 15);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..40 {
            app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        }
        assert!(app.detail_scroll > 0);
        assert!(text(&app, 80, 15).contains("Warning: warning 29"));
    }
    #[test]
    fn refresh_key_requests_immediate_poll() {
        let mut app = app(vec![row("work", "planned")]);
        assert_eq!(
            app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE),
            Effect::Refresh
        );
    }
    #[test]
    fn failed_poll_keeps_snapshot_and_marks_it_stale() {
        let mut app = app(vec![row("work", "planned")]);
        app.refresh_failed("cannot read Work model".into());
        let rendered = text(&app, 100, 24);
        assert!(rendered.contains("STALE: cannot read Work model"));
        assert!(rendered.contains("work title"));
        app.refresh(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus {
                rows: vec![row("fresh", "planned")],
                errors: vec![],
            },
        ));
        assert!(!text(&app, 100, 24).contains("STALE:"));
    }
    #[test]
    fn help_lists_contextual_controls_and_closes() {
        let mut app = app(vec![row("work", "planned")]);
        app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(text(&app, 100, 24).contains("j/k or arrows select"));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.help);
    }
    #[test]
    fn dashboard_controls_leave_work_state_unchanged() {
        let mut app = app(vec![row("work", "planned")]);
        let before = app.snapshot.rows[0].status.clone();
        for key in ['j', 'a', '?', 'r', 'c'] {
            app.handle_key(KeyCode::Char(key), KeyModifiers::NONE);
        }
        assert_eq!(app.snapshot.rows[0].status, before);
    }
    #[test]
    fn overflow_rows_remain_navigable() {
        let mut app = app((0..24)
            .map(|n| row(&format!("work-{n}"), "planned"))
            .collect());
        app.resize(80, 15);
        for _ in 0..23 {
            app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        }
        assert_eq!(app.selected_id(), Some("work-23"));
        assert!(app.list_scroll > 0);
        assert!(text(&app, 80, 15).contains("work-23"));
    }
    #[test]
    fn list_navigation_moves_selection_when_scrolled() {
        let mut app = app((0..24)
            .map(|n| row(&format!("work-{n}"), "planned"))
            .collect());
        app.resize(80, 15);
        for _ in 0..23 {
            app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        }
        assert!(app.list_scroll > 0);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected_id(), Some("work-22"));
    }
    #[test]
    fn all_work_adds_terminal_group() {
        let mut app = app(vec![
            row("current", "planned"),
            row("complete", "complete"),
            row("merged", "merged"),
            row("abandoned", "abandoned"),
        ]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        let rendered = text(&app, 100, 24);
        assert!(rendered.contains("Terminal (3)"));
        for (id, title) in [
            ("complete", "complete title"),
            ("merged", "merged title"),
            ("abandoned", "abandoned title"),
        ] {
            app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
            assert_eq!(app.selected_id(), Some(id));
            assert!(text(&app, 100, 24).contains(title), "missing {title}");
        }
    }
    #[test]
    fn current_empty_state_points_to_all_work() {
        assert!(
            text(&app(vec![row("done", "complete")]), 100, 24)
                .contains("No Current Work. Press a for All Work.")
        );
    }
    #[test]
    fn detail_shows_selected_work_state() {
        let mut status = row("work", "planned");
        status.compatibility_warnings = vec!["legacy state".into()];
        status.metrics.input_tokens = 42;
        status.release = Some(Default::default());
        let rendered = text(&app(vec![status]), 100, 24);
        for value in [
            "work title",
            "ID: work",
            "Action: planned",
            "Attempt: attempt [running]",
            "Task: write [running]",
            "Review: pending",
            "Merge Candidate: candidate",
            "Merge: pending",
            "Metrics:",
            "Warning: legacy state",
            "Release:",
        ] {
            assert!(rendered.contains(value), "missing {value}");
        }
    }
    #[test]
    fn long_and_wide_character_content_stays_within_regions() {
        let mut status = row("識別子", "planned");
        status.title = "e\u{301}識別子 with a very long title that must truncate".into();
        let app = app(vec![status]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render::draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer_text(buffer);
        assert!(rendered.contains("…"));
        assert!(rendered.contains("[planned]"));
        for y in 4..22 {
            assert!(buffer[(42, y)].symbol().contains('│'));
        }
    }
    #[test]
    fn resize_preserves_selection_and_changes_layout() {
        let mut app = app(vec![row("first", "planned"), row("selected", "planned")]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.resize(80, 24);
        assert!(text(&app, 80, 24).contains("Work Items"));
        app.resize(100, 24);
        assert_eq!(app.selected_id(), Some("selected"));
        assert!(text(&app, 100, 24).contains("Work Item detail"));
    }
    #[test]
    fn all_key_toggles_current_and_all_work() {
        let mut app = app(vec![row("work", "planned")]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.all_work());
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!app.all_work());
    }
    #[test]
    fn successful_poll_replaces_dashboard_snapshot() {
        let mut app = app(vec![row("old", "planned")]);
        app.take_dirty();
        app.refresh(snapshot::DashboardSnapshot::from_status(
            crate::work_status::WorkStatus {
                rows: vec![row("fresh", "planned")],
                errors: vec![],
            },
        ));
        assert!(app.take_dirty());
        assert_eq!(app.selected_id(), Some("fresh"));
    }
    #[test]
    fn successful_poll_clears_stale_state() {
        let mut app = app(vec![row("work", "planned")]);
        app.refresh_failed("read failed".into());
        app.refresh(app.snapshot.clone());
        assert!(app.stale_error.is_none());
    }
    #[test]
    fn work_read_errors_remain_available_with_overflow() {
        let mut app = app_with_errors(
            vec![row("work", "planned")],
            (0..20).map(|n| format!("error {n}")).collect(),
        );
        app.resize(80, 15);
        for _ in 0..22 {
            app.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        }
        assert_eq!(
            app.list_scroll as usize,
            app.scroll_bounds().list_max_scroll as usize
        );
        assert!(text(&app, 80, 15).contains("error 19"));
    }
    #[test]
    fn idle_dashboard_does_not_request_repaint() {
        let mut app = app(vec![row("work", "planned")]);
        app.take_dirty();
        app.refresh(app.snapshot.clone());
        assert!(!app.take_dirty());
    }
    #[test]
    fn help_bar_tracks_current_view() {
        let mut app = app(vec![row("work", "planned")]);
        assert!(text(&app, 100, 24).contains("j/k select"));
        app.resize(80, 24);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(text(&app, 80, 24).contains("Esc list  j/k scroll"));
    }
    #[test]
    fn scheduled_poll_waits_for_its_deadline() {
        let now = Instant::now();
        assert!(!poll_due(now, now + DATA_POLL_INTERVAL));
        assert!(poll_due(now + DATA_POLL_INTERVAL, now + DATA_POLL_INTERVAL));
    }
}
