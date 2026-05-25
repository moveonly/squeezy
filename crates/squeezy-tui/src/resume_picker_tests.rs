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

#[test]
fn picker_enter_resumes_highlighted_session() {
    let candidates = vec![
        SessionSummary {
            session_id: "first".to_string(),
            started_at_ms: 0,
            first_user_task: Some("a".to_string()),
            latest_summary: None,
        },
        SessionSummary {
            session_id: "second".to_string(),
            started_at_ms: 0,
            first_user_task: Some("b".to_string()),
            latest_summary: None,
        },
    ];
    let mut state = ResumePickerState::new(candidates);
    assert!(state.dispatch(press(KeyCode::Down)).is_none());
    assert_eq!(state.cursor, 1);
    let outcome = state.dispatch(press(KeyCode::Enter));
    assert_eq!(outcome, Some(ResumeChoice::Resume("second".to_string())));
}

#[test]
fn picker_esc_starts_fresh() {
    let mut state = ResumePickerState::new(vec![SessionSummary {
        session_id: "first".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
    }]);
    assert_eq!(
        state.dispatch(press(KeyCode::Esc)),
        Some(ResumeChoice::StartFresh)
    );
}

#[test]
fn picker_q_quits() {
    let mut state = ResumePickerState::new(vec![SessionSummary {
        session_id: "first".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
    }]);
    assert_eq!(
        state.dispatch(press(KeyCode::Char('q'))),
        Some(ResumeChoice::Quit)
    );
}

#[test]
fn picker_arrow_wraps_through_start_fresh() {
    // With one candidate the list has 2 rows: [candidate, start_fresh].
    let mut state = ResumePickerState::new(vec![SessionSummary {
        session_id: "only".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
    }]);
    assert_eq!(state.cursor, 0);
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 1); // moved to start_fresh
    state.dispatch(press(KeyCode::Down));
    assert_eq!(state.cursor, 0); // wrapped back to first candidate
    state.dispatch(press(KeyCode::Up));
    assert_eq!(state.cursor, 1); // wrapped up to start_fresh
}

#[test]
fn picker_enter_on_start_fresh_row_starts_fresh() {
    let mut state = ResumePickerState::new(vec![SessionSummary {
        session_id: "first".to_string(),
        started_at_ms: 0,
        first_user_task: None,
        latest_summary: None,
    }]);
    state.dispatch(press(KeyCode::Down)); // cursor on start_fresh
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(ResumeChoice::StartFresh)
    );
}

#[test]
fn session_summary_label_truncates_long_prompts() {
    let summary = SessionSummary {
        session_id: "x".to_string(),
        started_at_ms: 0,
        first_user_task: Some("a".repeat(200)),
        latest_summary: None,
    };
    let label = summary.label();
    assert!(label.chars().count() <= 80, "label too long: {label}");
    assert!(label.ends_with('…'), "expected ellipsis: {label}");
}
