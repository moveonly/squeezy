use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    }
}

fn cwd() -> PathBuf {
    PathBuf::from("/work/repo")
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
        Some(ResumeChoice::Resume("second".to_string()))
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
    // [start_fresh, candidate] — 2 rows total.
    let mut state = ResumePickerState::new(vec![summary("only")], cwd());
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
        cwd: "/work/repo".to_string(),
        repo_root: None,
    };
    let label = summary.label();
    assert!(label.chars().count() <= 80, "label too long: {label}");
    assert!(label.ends_with('…'), "expected ellipsis: {label}");
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
            .map(|s| s.session_id.as_str())
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
            .map(|s| s.session_id.as_str())
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

fn global_entry(
    id: &str,
    cwd: &str,
    started_at_ms: u64,
    resume_available: bool,
) -> GlobalSessionIndexEntry {
    GlobalSessionIndexEntry {
        session_id: id.to_string(),
        cwd: cwd.to_string(),
        workspace_root: cwd.to_string(),
        repo_root: None,
        title: Some(format!("global task for {id}")),
        started_at_ms,
        last_event_at_ms: started_at_ms,
        turn_count: 0,
        resume_available,
    }
}

#[test]
fn merge_surfaces_cross_project_sessions_from_global_index() {
    // Per-project store only contains the local session; the cross-project
    // index supplies the sibling-repo entry. The picker needs both so the
    // Tab toggle can flip into a cross-project view.
    let now = 5_000_000;
    let local = vec![meta("local", "/work/repo", now - 1_000, true)];
    let global = vec![global_entry("sibling", "/work/other", now - 2_000, true)];

    let out = merge_candidates_for_picker(&local, &global, now);
    let ids: Vec<&str> = out.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["local", "sibling"],
        "merge must surface both local + global entries newest-first",
    );
    // The sibling row keeps the cwd we put in the global index so the
    // picker can render the "(other)" hint.
    let sibling = out
        .iter()
        .find(|s| s.session_id == "sibling")
        .expect("sibling present");
    assert_eq!(sibling.cwd, "/work/other");
    assert_eq!(
        sibling.first_user_task.as_deref(),
        Some("global task for sibling"),
    );
}

#[test]
fn merge_prefers_local_metadata_when_session_id_overlaps() {
    // The local SessionMetadata carries the richer title; the global
    // index entry for the same session_id must lose so the picker shows
    // the finalized state, not the stale snapshot.
    let now = 5_000_000;
    let mut local_meta = meta("shared", "/work/repo", now - 1_000, true);
    local_meta.first_user_task = Some("local prompt wins".to_string());
    let local = vec![local_meta];
    let global = vec![GlobalSessionIndexEntry {
        title: Some("global prompt loses".to_string()),
        ..global_entry("shared", "/work/repo", now - 1_000, true)
    }];

    let out = merge_candidates_for_picker(&local, &global, now);
    assert_eq!(
        out.len(),
        1,
        "duplicate session_ids must collapse to one row"
    );
    assert_eq!(
        out[0].first_user_task.as_deref(),
        Some("local prompt wins"),
        "local SessionMetadata must win over the global index snapshot",
    );
}

#[test]
fn merge_drops_unresumable_global_entries() {
    let now = 5_000_000;
    let global = vec![global_entry("done", "/work/other", now - 1_000, false)];
    let out = merge_candidates_for_picker(&[], &global, now);
    assert!(
        out.is_empty(),
        "resume_available=false in the index must hide the row from the picker",
    );
}

#[test]
fn merge_drops_stale_global_entries() {
    let now = 30 * 24 * 60 * 60 * 1_000;
    let global = vec![global_entry(
        "ancient",
        "/work/other",
        now - 30 * 24 * 60 * 60 * 1_000 + 1,
        true,
    )];
    let out = merge_candidates_for_picker(&[], &global, now);
    assert!(
        out.is_empty(),
        "entries older than the recency window must be dropped"
    );
}
