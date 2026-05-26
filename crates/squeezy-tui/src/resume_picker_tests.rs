use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use squeezy_store::{SessionMetadata, SessionStatus};

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
    SessionSummary {
        session_id: id.to_string(),
        started_at_ms: 0,
        first_user_task: Some(format!("task for {id}")),
        latest_summary: None,
        turn_count: 0,
    }
}

#[test]
fn picker_opens_with_start_fresh_selected() {
    let state = ResumePickerState::new(vec![summary("first"), summary("second")]);
    // Start fresh sits at row 0 so the safe default is pre-selected.
    assert_eq!(state.cursor, 0);
    assert_eq!(state.start_fresh_index(), 0);
}

#[test]
fn picker_enter_on_start_fresh_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")]);
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::StartFresh)
    );
}

#[test]
fn picker_enter_on_candidate_resumes_that_session() {
    let mut state = ResumePickerState::new(vec![summary("first"), summary("second")]);
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // first candidate (row 1)
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 2); // second candidate (row 2)
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::Resume("second".to_string()))
    );
}

#[test]
fn picker_esc_starts_fresh() {
    let mut state = ResumePickerState::new(vec![summary("first")]);
    state.dispatch(press(KeyCode::Down)); // cursor on candidate
    assert_eq!(
        state.dispatch(press(KeyCode::Esc)),
        Some(ResumeChoice::StartFresh)
    );
}

#[test]
fn picker_q_quits() {
    let mut state = ResumePickerState::new(vec![summary("first")]);
    assert_eq!(
        state.dispatch(press(KeyCode::Char('q'))),
        Some(ResumeChoice::Quit)
    );
}

#[test]
fn picker_arrow_wraps_through_start_fresh_at_top() {
    // [start_fresh, candidate] — 2 rows total.
    let mut state = ResumePickerState::new(vec![summary("only")]);
    assert_eq!(state.cursor, 0); // opens on start_fresh
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // candidate
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 0); // wraps back to start_fresh
    state.dispatch(press(KeyCode::Up));
    assert_eq!(state.cursor, 1); // wraps up to last row (candidate)
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
fn session_summary_label_truncates_long_prompts() {
    let summary = SessionSummary {
        session_id: "x".to_string(),
        started_at_ms: 0,
        first_user_task: Some("a".repeat(200)),
        latest_summary: None,
        turn_count: 0,
    };
    let label = summary.label();
    assert!(label.chars().count() <= 80, "label too long: {label}");
    assert!(label.ends_with('…'), "expected ellipsis: {label}");
}
