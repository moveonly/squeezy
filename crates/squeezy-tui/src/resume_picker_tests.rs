use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use squeezy_store::{GlobalSessionIndexEntry, SessionMetadata, SessionStatus};

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
fn filter_excludes_empty_sessions_without_resume_content() {
    let cwd = PathBuf::from("/work/repo");
    let now = 1_000_000;
    let mut empty = meta("empty", "/work/repo", now - 1_000, true);
    empty.first_user_task = None;
    empty.latest_summary = None;
    empty.display_name = None;
    empty.metrics.turns = 0;
    empty.event_count = 2;
    let mut failed_turn = meta("failed-turn", "/work/repo", now - 2_000, true);
    failed_turn.metrics.turns = 0;

    let out = filter_candidates(&[empty, failed_turn], &cwd, now);

    assert_eq!(
        out.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["failed-turn"]
    );
}

#[test]
fn merge_candidates_excludes_empty_global_index_entries() {
    let now = 1_000_000;
    let empty = GlobalSessionIndexEntry {
        session_id: "empty".to_string(),
        cwd: "/work/repo".to_string(),
        workspace_root: "/work/repo".to_string(),
        repo_root: None,
        title: None,
        display_name: None,
        started_at_ms: now - 1_000,
        last_event_at_ms: now - 1_000,
        turn_count: 0,
        resume_available: true,
    };
    let useful = GlobalSessionIndexEntry {
        session_id: "useful".to_string(),
        title: Some("debug failure".to_string()),
        started_at_ms: now - 2_000,
        last_event_at_ms: now - 2_000,
        ..empty.clone()
    };

    let out = merge_candidates_for_picker(&[], &[empty, useful], now);

    assert_eq!(
        out.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["useful"]
    );
}

#[test]
fn filter_orders_newest_first_and_caps_at_max() {
    let cwd = PathBuf::from("/work/repo");
    let now = 1_000_000;
    let total = MAX_PICKER_ENTRIES + 5;
    let sessions: Vec<SessionMetadata> = (0..total)
        .map(|i| {
            meta(
                &format!("s{i:02}"),
                "/work/repo",
                now - (i as u64 * 1_000),
                true,
            )
        })
        .collect();
    let out = filter_candidates(&sessions, &cwd, now);
    assert_eq!(out.len(), MAX_PICKER_ENTRIES);
    // Newest-first, capped to the most-recent MAX_PICKER_ENTRIES.
    let ids: Vec<&str> = out.iter().map(|s| s.session_id.as_str()).collect();
    let expected: Vec<String> = (0..MAX_PICKER_ENTRIES)
        .map(|i| format!("s{i:02}"))
        .collect();
    assert_eq!(ids, expected);
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
fn picker_scrolls_to_keep_cursor_visible_when_list_overflows() {
    let summaries: Vec<SessionSummary> = (0..MAX_PICKER_ENTRIES)
        .map(|i| summary(&format!("s{i:02}")))
        .collect();
    let mut state = ResumePickerState::new(summaries, cwd());
    let last_candidate = state.candidates.len();
    for _ in 0..last_candidate {
        state.dispatch(press(KeyCode::Down));
    }
    assert_eq!(state.cursor, last_candidate, "cursor on the last candidate");

    // A short terminal can't fit the whole list, so the viewport must scroll:
    // the selected (last) row stays on screen and an early one scrolls off.
    let last_label = format!("task for s{:02}", state.candidates.len() - 1);
    let text = render_state_to_text(&state, 80, 16);
    assert!(
        text.contains(&last_label),
        "selected row must stay visible:\n{text}"
    );
    assert!(
        !text.contains("task for s00"),
        "early rows must scroll off the top:\n{text}"
    );
}

#[test]
fn picker_signals_cross_project_switch() {
    let here = summary_at("here", "/work/repo");
    let there = summary_at("there", "/other/place");
    let mut state = ResumePickerState::new(vec![here, there], cwd());
    state.dispatch(press(KeyCode::Tab)); // show sessions from all projects
    // Move the cursor onto the cross-project candidate.
    state.dispatch(press(KeyCode::Down));
    state.dispatch(press(KeyCode::Down));

    let text = render_state_to_text(&state, 90, 18);
    assert!(
        text.contains('↪'),
        "cross-project rows carry the ↪ marker:\n{text}"
    );
    assert!(
        text.contains("switches to /other/place"),
        "the highlighted cross-project row explains the directory switch:\n{text}"
    );
}

#[test]
fn picker_teaches_tab_when_scoped_view_is_empty_but_other_projects_exist() {
    // No session for the current cwd, but one in a sibling repo: the scoped
    // view has no rows, yet the picker must still surface the Tab toggle.
    let elsewhere = summary_at("elsewhere", "/other/place");
    let state = ResumePickerState::new(vec![elsewhere], cwd());
    assert!(
        state.candidates.is_empty(),
        "scoped view hides the cross-project session by default",
    );
    let text = render_state_to_text(&state, 90, 18);
    assert!(
        text.contains("session in other projects"),
        "the empty scoped view advertises the Tab toggle:\n{text}"
    );
}

#[test]
fn picker_footer_warns_that_cross_project_rows_change_cwd() {
    let here = summary_at("here", "/work/repo");
    let there = summary_at("there", "/other/place");
    let mut state = ResumePickerState::new(vec![here, there], cwd());
    state.dispatch(press(KeyCode::Tab)); // cross-project view
    let text = render_state_to_text(&state, 100, 18);
    assert!(
        text.contains("change your working directory"),
        "the cross-project footer spells out the cwd consequence:\n{text}"
    );
}

#[test]
fn picker_ellipsises_long_labels_instead_of_hard_cropping() {
    let mut long = summary("long");
    long.first_user_task = Some("verylongsessiontitlewithnospaces".repeat(12));
    let mut state = ResumePickerState::new(vec![long], cwd());
    state.dispatch(press(KeyCode::Down)); // cursor onto the candidate

    let text = render_state_to_text(&state, 70, 16);
    assert!(
        text.contains('…'),
        "an over-long label must be ellipsised, not hard-cropped:\n{text}"
    );
}

#[test]
fn picker_enter_on_start_fresh_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")], cwd());
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::StartFresh)
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
        })
    );
}

#[test]
fn picker_esc_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")], cwd());
    state.dispatch(press(KeyCode::Down)); // cursor on candidate
    assert_eq!(
        state.dispatch(press(KeyCode::Esc)),
        Some(ResumeChoice::StartFresh)
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
    // [start_fresh, candidate] — 2 rows total now that the suppression
    // checkboxes are gone (the picker is opt-in via --resume).
    let mut state = ResumePickerState::new(vec![summary("only")], cwd());
    assert_eq!(state.cursor, 0); // opens on start_fresh
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // candidate
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 0); // wraps back to start_fresh
    state.dispatch(press(KeyCode::Up));
    assert_eq!(state.cursor, 1); // wraps up to the last row (candidate)
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

#[test]
fn picker_keeps_linear_session_as_single_row() {
    let linear = summary("linear");
    let state = ResumePickerState::new(vec![linear], cwd());
    assert_eq!(state.candidates.len(), 1);
}

#[test]
fn picker_select_treats_trailing_slash_mismatch_as_same_project() {
    // A session stored with a trailing separator and a cwd without one
    // should be treated as the same directory. select_at_cursor must use
    // paths_same (not ==) so the session resolves as Resume, not CrossProject.
    let same_dir_with_slash = summary_at("s", "/work/repo/");
    let mut state = ResumePickerState::new(vec![same_dir_with_slash], cwd());
    // By default the scoped view should show this session because
    // paths_same("/work/repo/", "/work/repo") == true.
    assert_eq!(
        state.candidates.len(),
        1,
        "session should be in scoped view"
    );
    state.dispatch(press(KeyCode::Down));
    let choice = state.dispatch(press(KeyCode::Enter));
    assert!(
        matches!(choice, Some(ResumeChoice::Resume { .. })),
        "trailing-slash mismatch must not dispatch as CrossProject; got: {choice:?}"
    );
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
