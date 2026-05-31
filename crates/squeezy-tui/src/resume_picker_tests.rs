use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use squeezy_store::{EventBranchTip, SessionMetadata, SessionStatus};

use super::*;

fn meta(id: &str, cwd: &str, started_at_ms: u64, resume_available: bool) -> SessionMetadata {
    SessionMetadata {
        session_id: id.to_string(),
        cwd: cwd.to_string(),
        started_at_ms,
        resume_available,
        first_user_task: Some(format!("task for {id}")),
        status: SessionStatus::Completed,
        ..SessionMetadata::default()
    }
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

#[test]
fn filter_excludes_other_cwds() {
    let cwd = PathBuf::from("/work/repo");
    let now = 1_000_000;
    let sessions = vec![
        meta("a", "/work/repo", now - 1_000, true),
        meta("b", "/other/path", now - 2_000, true),
    ];
    let out = filter_candidates(&sessions, &cwd, now);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].session_id, "a");
}

#[test]
fn filter_excludes_old_sessions() {
    let cwd = PathBuf::from("/work/repo");
    let now = 30 * 24 * 60 * 60 * 1_000;
    let sessions = vec![
        meta("recent", "/work/repo", now - 1_000, true),
        meta(
            "stale",
            "/work/repo",
            now - 30 * 24 * 60 * 60 * 1_000 + 1,
            true,
        ),
    ];
    let out = filter_candidates(&sessions, &cwd, now);
    assert_eq!(
        out.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recent"]
    );
}

#[test]
fn filter_excludes_unresumable() {
    let cwd = PathBuf::from("/work/repo");
    let now = 1_000_000;
    let sessions = vec![
        meta("ok", "/work/repo", now - 1_000, true),
        meta("blocked", "/work/repo", now - 2_000, false),
    ];
    let out = filter_candidates(&sessions, &cwd, now);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].session_id, "ok");
}

#[test]
fn filter_orders_newest_first_and_caps_at_max() {
    let cwd = PathBuf::from("/work/repo");
    let now = 1_000_000;
    let sessions: Vec<SessionMetadata> = (0..10)
        .map(|i| {
            meta(
                &format!("s{i}"),
                "/work/repo",
                now - (i as u64 * 1_000),
                true,
            )
        })
        .collect();
    let out = filter_candidates(&sessions, &cwd, now);
    assert_eq!(out.len(), MAX_PICKER_ENTRIES);
    let ids: Vec<&str> = out.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, vec!["s0", "s1", "s2", "s3", "s4"]);
}

fn summary(id: &str) -> SessionSummary {
    summary_at(id, "/work/repo")
}

fn summary_at(id: &str, cwd: &str) -> SessionSummary {
    SessionSummary {
        session_id: id.to_string(),
        started_at_ms: 0,
        first_user_task: Some(format!("task for {id}")),
        latest_summary: None,
        turn_count: 0,
        cwd: cwd.to_string(),
        repo_root: None,
        display_name: None,
        labels: Vec::new(),
        branches: Vec::new(),
    }
}

fn cwd() -> PathBuf {
    PathBuf::from("/work/repo")
}

fn render_state_to_text(state: &ResumePickerState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_picker(frame, state))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

#[test]
fn picker_opens_with_start_fresh_selected() {
    let state = ResumePickerState::new(vec![summary("first"), summary("second")], cwd());
    // Start fresh sits at row 0 so the safe default is pre-selected.
    assert_eq!(state.cursor, 0);
    assert_eq!(state.start_fresh_index(), 0);
}

#[test]
fn picker_enter_on_start_fresh_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")], cwd());
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::StartFresh {
            suppress: ResumePickerSuppress::default(),
        })
    );
}

#[test]
fn picker_enter_on_candidate_resumes_that_session() {
    let mut state = ResumePickerState::new(vec![summary("first"), summary("second")], cwd());
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // first candidate (row 1)
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 2); // second candidate (row 2)
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::Resume {
            session_id: "second".to_string(),
            branch_tip: None,
        })
    );
}

#[test]
fn picker_esc_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")], cwd());
    state.dispatch(press(KeyCode::Down)); // cursor on candidate
    assert_eq!(
        state.dispatch(press(KeyCode::Esc)),
        Some(ResumeChoice::StartFresh {
            suppress: ResumePickerSuppress::default(),
        })
    );
}

#[test]
fn picker_q_quits() {
    let mut state = ResumePickerState::new(vec![summary("first")], cwd());
    assert_eq!(
        state.dispatch(press(KeyCode::Char('q'))),
        Some(ResumeChoice::Quit)
    );
}

#[test]
fn picker_arrow_wraps_through_start_fresh_at_top() {
    // [start_fresh, candidate, project checkbox, user checkbox] — 4 rows total.
    let mut state = ResumePickerState::new(vec![summary("only")], cwd());
    assert_eq!(state.cursor, 0); // opens on start_fresh
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // candidate
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 2); // project checkbox
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 3); // user checkbox
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 0); // wraps back to start_fresh
    state.dispatch(press(KeyCode::Up));
    assert_eq!(state.cursor, 3); // wraps up to last row (user checkbox)
}

#[test]
fn user_never_ask_checkbox_implies_project_checkbox() {
    let mut state = ResumePickerState::new(vec![summary("only")], cwd());
    state.cursor = state.user_checkbox_index();

    state.dispatch(press(KeyCode::Char(' ')));

    assert!(state.never_user);
    assert!(state.never_project);
    state.cursor = state.start_fresh_index();
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::StartFresh {
            suppress: ResumePickerSuppress {
                project: true,
                user: true,
            },
        })
    );
}

#[test]
fn clearing_project_checkbox_also_clears_user_checkbox() {
    let mut state = ResumePickerState::new(vec![summary("only")], cwd());
    state.cursor = state.user_checkbox_index();
    state.dispatch(press(KeyCode::Char(' ')));
    state.cursor = state.project_checkbox_index();

    state.dispatch(press(KeyCode::Char(' ')));

    assert!(!state.never_project);
    assert!(!state.never_user);
}

#[test]
fn turn_indicator_renders_singular_and_plural_correctly() {
    let mut s = summary("x");
    s.turn_count = 0;
    assert_eq!(s.turn_indicator(), "new");
    s.turn_count = 1;
    assert_eq!(s.turn_indicator(), "1 prompt");
    s.turn_count = 7;
    assert_eq!(s.turn_indicator(), "7 prompts");
}

#[test]
fn session_summary_label_keeps_long_prompts_for_wrapping() {
    let summary = SessionSummary {
        session_id: "x".to_string(),
        started_at_ms: 0,
        first_user_task: Some("a".repeat(200)),
        latest_summary: None,
        turn_count: 0,
        cwd: "/work/repo".to_string(),
        repo_root: None,
        display_name: None,
        labels: Vec::new(),
        branches: Vec::new(),
    };
    let label = summary.label();
    assert_eq!(label.chars().count(), 200);
    assert!(!label.contains('…'), "label should wrap instead: {label}");
}

#[test]
fn session_summary_label_prefers_display_name_when_set() {
    // The user-set display name beats the inferred first-user-task label
    // so memorable sessions stay easy to spot in the picker.
    let mut summary = summary("x");
    summary.display_name = Some("payments-refactor".to_string());
    summary.first_user_task = Some("debug why /checkout 500s".to_string());
    assert_eq!(summary.label(), "payments-refactor");
}

#[test]
fn session_summary_label_falls_back_to_task_when_display_name_is_blank() {
    // A whitespace-only display name (the user typed `/session rename
    // "   "`) must not silently blank the row label — fall back to the
    // inferred task instead.
    let mut summary = summary("x");
    summary.display_name = Some("   ".to_string());
    summary.first_user_task = Some("inferred prompt".to_string());
    assert_eq!(summary.label(), "inferred prompt");
}

#[test]
fn session_summary_label_hint_renders_labels_as_hashtags() {
    let mut summary = summary("x");
    summary.labels = vec!["bugfix".to_string(), "payments".to_string()];
    assert_eq!(summary.label_hint(), "#bugfix #payments");
}

#[test]
fn session_summary_label_hint_is_empty_when_no_labels() {
    let summary = summary("x");
    assert!(summary.label_hint().is_empty());
}

#[test]
fn toggle_all_projects_includes_cross_cwd_sessions() {
    // One session in cwd, one in a sibling repo. The default scoped view
    // hides the sibling; Tab flips to include both.
    let all = vec![
        summary_at("scoped", "/work/repo"),
        summary_at("sibling", "/work/other"),
    ];
    let mut state = ResumePickerState::new(all, cwd());
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|entry| entry.session_id())
            .collect::<Vec<_>>(),
        vec!["scoped"]
    );
    assert!(!state.show_all_projects);

    state.dispatch(press(KeyCode::Tab));
    assert!(state.show_all_projects);
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|entry| entry.session_id())
            .collect::<Vec<_>>(),
        vec!["scoped", "sibling"]
    );
    // Cursor must reset so the user does not act on a row that moved.
    assert_eq!(state.cursor, 0);

    state.dispatch(press(KeyCode::Tab));
    assert!(!state.show_all_projects);
    assert_eq!(state.candidates.len(), 1);
}

#[test]
fn enter_on_cross_project_row_returns_cross_project_choice() {
    let all = vec![summary_at("sibling", "/work/other")];
    let mut state = ResumePickerState::new(all, cwd());
    // Default scoped view hides the sibling, so toggle first.
    state.dispatch(press(KeyCode::Tab));
    state.dispatch(press(KeyCode::Down)); // cursor on sibling row
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::CrossProject {
            session_id: "sibling".to_string(),
            target_cwd: "/work/other".to_string(),
        })
    );
}

#[test]
fn project_hint_prefers_repo_root_basename() {
    let s = SessionSummary {
        session_id: "x".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
        turn_count: 0,
        cwd: "/work/other/src/deep".to_string(),
        repo_root: Some("/work/other".to_string()),
        display_name: None,
        labels: Vec::new(),
        branches: Vec::new(),
    };
    assert_eq!(s.project_hint(), "other");
}

#[test]
fn project_hint_falls_back_to_cwd_tail() {
    let s = SessionSummary {
        session_id: "x".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
        turn_count: 0,
        cwd: "/work/sibling".to_string(),
        repo_root: None,
        display_name: None,
        labels: Vec::new(),
        branches: Vec::new(),
    };
    assert_eq!(s.project_hint(), "sibling");
}

#[test]
fn filter_all_projects_keeps_cross_cwd_entries() {
    let now = 1_000_000;
    let sessions = vec![
        meta("scoped", "/work/repo", now - 1_000, true),
        meta("sibling", "/work/other", now - 2_000, true),
    ];
    let out = filter_candidates_all_projects(&sessions, now);
    assert_eq!(
        out.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["scoped", "sibling"]
    );
}

fn tip(tip_sequence: u64, branched_from: u64, ts: u64, message: &str) -> EventBranchTip {
    EventBranchTip {
        tip_sequence,
        branched_from_sequence: branched_from,
        tip_ts_unix_ms: ts,
        first_message_after_branch: Some(message.to_string()),
    }
}

#[test]
fn picker_expands_branched_sessions_into_one_row_per_branch_tip() {
    // Synthesised summary: a single session with two branches (the user
    // re-prompted from an earlier turn). The picker should surface both
    // paths as independent rows so each is selectable.
    let mut branched = summary("branched");
    branched.branches = vec![
        tip(5, 1, 200, "path B prompt"),
        tip(3, 1, 100, "path A prompt"),
    ];
    let state = ResumePickerState::new(vec![branched.clone()], cwd());
    assert_eq!(state.candidates.len(), 2);
    let session_ids: Vec<&str> = state
        .candidates
        .iter()
        .map(|entry| entry.session_id())
        .collect();
    assert_eq!(session_ids, vec!["branched", "branched"]);
    let branch_tips: Vec<Option<u64>> = state
        .candidates
        .iter()
        .map(|entry| entry.branch_tip.as_ref().map(|t| t.tip_sequence))
        .collect();
    assert_eq!(branch_tips, vec![Some(5), Some(3)]);
}

#[test]
fn picker_enter_on_branch_row_returns_branch_tip_in_resume_choice() {
    let mut branched = summary("branched");
    branched.branches = vec![tip(5, 1, 200, "path B"), tip(3, 1, 100, "path A")];
    let mut state = ResumePickerState::new(vec![branched], cwd());
    // [start_fresh, branch tip 5, branch tip 3]. Down twice lands on tip 3.
    state.dispatch(press(KeyCode::Down));
    state.dispatch(press(KeyCode::Down));
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::Resume {
            session_id: "branched".to_string(),
            branch_tip: Some(3),
        })
    );
}

#[test]
fn picker_keeps_linear_session_as_single_row() {
    let linear = summary("linear");
    let state = ResumePickerState::new(vec![linear], cwd());
    assert_eq!(state.candidates.len(), 1);
    assert!(state.candidates[0].branch_tip.is_none());
}

#[test]
fn picker_handles_single_branch_tip_as_linear() {
    // detect_branches refuses to report only one tip, but if a caller ever
    // hands us a summary with a single tip we still want to render one row
    // rather than dropping the session entirely.
    let mut summary = summary("one-tip");
    summary.branches = vec![tip(2, 0, 50, "only path")];
    let state = ResumePickerState::new(vec![summary], cwd());
    assert_eq!(state.candidates.len(), 1);
    assert!(state.candidates[0].branch_tip.is_none());
}

#[test]
fn picker_expands_branches_after_tab_toggle() {
    let mut sibling = summary_at("sibling", "/work/other");
    sibling.branches = vec![tip(4, 1, 90, "branch B"), tip(2, 1, 70, "branch A")];
    let state = ResumePickerState::new(vec![sibling], cwd());
    assert!(
        state.candidates.is_empty(),
        "scoped view hides cross-project branches by default",
    );
    let mut state = state;
    state.dispatch(press(KeyCode::Tab));
    assert_eq!(state.candidates.len(), 2);
    // Cross-project branched rows must still surface as CrossProject so
    // the user gets the chdir hint rather than an in-process resume that
    // would silently jump cwds.
    state.dispatch(press(KeyCode::Down));
    let choice = state.dispatch(press(KeyCode::Enter));
    assert!(matches!(choice, Some(ResumeChoice::CrossProject { .. })));
}

#[test]
fn setup_resume_picker_left_goes_back_to_setup() {
    let mut state =
        ResumePickerState::with_setup_progress(vec![summary("first")], cwd(), Some((5, 5)));

    assert_eq!(
        state.dispatch(press(KeyCode::Left)),
        Some(ResumeChoice::Back)
    );
}

#[test]
fn setup_resume_picker_uses_setup_chrome_and_full_footer() {
    let state = ResumePickerState::with_setup_progress(vec![summary("first")], cwd(), Some((5, 5)));

    let rendered = render_state_to_text(&state, 86, 22);

    assert!(rendered.contains("first run setup"), "{rendered}");
    assert!(
        rendered.contains("Question 5/5 Resume a session"),
        "{rendered}"
    );
    assert!(rendered.contains("← back"), "{rendered}");
    assert!(rendered.contains("Q quit"), "{rendered}");
}
