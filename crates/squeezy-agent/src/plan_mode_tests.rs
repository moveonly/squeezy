use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use squeezy_core::SessionMode;

use super::{
    CURRENT_POINTER_FILE, PLAN_DIR, PLAN_MODE_INSTRUCTIONS, instructions_for_mode,
    latest_plan_path, strip_proposed_plan_blocks,
};

const TEST_SESSION_ID: &str = "test-sess-abc";

fn empty_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy_plan_mode_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("mkdir workspace");
    root
}

fn session_plans_dir(root: &Path, sid: &str) -> PathBuf {
    let dir = root.join(PLAN_DIR).join(sid);
    fs::create_dir_all(&dir).expect("mkdir session plans");
    dir
}

#[test]
fn plan_mode_appends_overlay() {
    let root = empty_workspace("appends_overlay");
    let out = instructions_for_mode("base", SessionMode::Plan, &root, Some(TEST_SESSION_ID));
    assert!(out.starts_with("base"));
    assert!(out.contains(PLAN_MODE_INSTRUCTIONS));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn build_mode_returns_base_verbatim() {
    let root = empty_workspace("build_verbatim");
    let out = instructions_for_mode(
        "base instructions",
        SessionMode::Build,
        &root,
        Some(TEST_SESSION_ID),
    );
    assert_eq!(out, "base instructions");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plan_mode_instructions_within_budget() {
    // The 3-phase prompt is intentionally heavier than v2's one-liner, but
    // still well under Codex's 4.5KB plan.md. 3500 chars is the cap.
    assert!(
        PLAN_MODE_INSTRUCTIONS.len() <= 3500,
        "PLAN_MODE_INSTRUCTIONS length {} > 3500",
        PLAN_MODE_INSTRUCTIONS.len()
    );
}

#[test]
fn plan_mode_mentions_proposed_plan_contract() {
    assert!(PLAN_MODE_INSTRUCTIONS.contains("<proposed_plan>"));
    assert!(PLAN_MODE_INSTRUCTIONS.contains("request_user_input"));
}

#[test]
fn plan_mode_tells_user_how_to_execute() {
    assert!(
        PLAN_MODE_INSTRUCTIONS.contains("Shift+Tab"),
        "PLAN_MODE_INSTRUCTIONS must reference Shift+Tab to unblock execute requests"
    );
}

#[test]
fn plan_mode_uses_three_phase_structure() {
    for phase in ["PHASE 1", "PHASE 2", "PHASE 3"] {
        assert!(
            PLAN_MODE_INSTRUCTIONS.contains(phase),
            "PLAN_MODE_INSTRUCTIONS missing structural label `{phase}`"
        );
    }
}

#[test]
fn plan_mode_instructs_exploration_before_questions() {
    // Codex's failure mode at v2: model asks before reading anything.
    // The prompt must explicitly steer toward exploration first.
    assert!(
        PLAN_MODE_INSTRUCTIONS.contains("Read/Search"),
        "PLAN_MODE_INSTRUCTIONS must mention the Read/Search exploration pass"
    );
}

#[test]
fn refinement_hint_added_when_plan_file_present() {
    let root = empty_workspace("hint_present");
    let plans_dir = session_plans_dir(&root, TEST_SESSION_ID);
    let plan_file = plans_dir.join("plan-abc123.md");
    fs::write(&plan_file, "step 1\n").expect("write plan");

    let out = instructions_for_mode("base", SessionMode::Plan, &root, Some(TEST_SESSION_ID));
    assert!(out.contains("Active plan file:"));
    assert!(out.contains(plan_file.to_str().expect("utf-8")));
    assert!(out.contains("read this file first with read_file"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn refinement_hint_absent_when_no_plan_files() {
    let root = empty_workspace("hint_absent");
    let out = instructions_for_mode("base", SessionMode::Plan, &root, Some(TEST_SESSION_ID));
    assert!(!out.contains("Active plan file:"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_prefers_current_pointer_over_mtime() {
    let root = empty_workspace("latest_pointer_first");
    let plans_dir = session_plans_dir(&root, TEST_SESSION_ID);
    let older = plans_dir.join("plan-pointed.md");
    let newer = plans_dir.join("plan-newer.md");
    fs::write(&older, "pointed body").expect("write older");
    fs::write(&newer, "newer body").expect("write newer");

    // Pointer aims at the OLDER file even though NEWER has the newer mtime.
    fs::write(plans_dir.join(CURRENT_POINTER_FILE), "plan-pointed\n").expect("write pointer");

    let picked = latest_plan_path(&root, Some(TEST_SESSION_ID)).expect("latest exists");
    assert_eq!(picked, older, "pointer must win over mtime fallback");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_falls_back_to_mtime_when_pointer_missing() {
    let root = empty_workspace("latest_by_mtime_fallback");
    let plans_dir = session_plans_dir(&root, TEST_SESSION_ID);
    let older = plans_dir.join("plan-old.md");
    let newer = plans_dir.join("plan-new.md");
    fs::write(&older, "old").expect("write older");
    fs::write(&newer, "new").expect("write newer");
    let now = SystemTime::now();
    fs::File::options()
        .write(true)
        .open(&older)
        .expect("open older")
        .set_modified(now - Duration::from_secs(60))
        .expect("set older mtime");
    fs::File::options()
        .write(true)
        .open(&newer)
        .expect("open newer")
        .set_modified(now)
        .expect("set newer mtime");
    let picked = latest_plan_path(&root, Some(TEST_SESSION_ID)).expect("latest exists");
    assert_eq!(picked, newer);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_ignores_non_markdown_files() {
    let root = empty_workspace("ignore_non_md");
    let plans_dir = session_plans_dir(&root, TEST_SESSION_ID);
    fs::write(plans_dir.join("README"), "not a plan").expect("write readme");
    fs::write(plans_dir.join("notes.txt"), "not a plan").expect("write notes");
    assert!(latest_plan_path(&root, Some(TEST_SESSION_ID)).is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_isolates_sessions() {
    let root = empty_workspace("session_isolation");
    let sid_a = "sess-a";
    let sid_b = "sess-b";
    let plans_a = session_plans_dir(&root, sid_a);
    let _ = session_plans_dir(&root, sid_b);
    fs::write(plans_a.join("plan-a.md"), "from a").expect("write a");
    // sid_b has nothing; latest_plan_path(b) must be None even though
    // sid_a has a plan file.
    assert!(latest_plan_path(&root, Some(sid_a)).is_some());
    assert!(latest_plan_path(&root, Some(sid_b)).is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn strip_proposed_plan_blocks_removes_closed_block() {
    let input =
        "intro paragraph\n\n<proposed_plan>\nstep 1\nstep 2\n</proposed_plan>\n\ntail paragraph";
    let out = strip_proposed_plan_blocks(input);
    assert!(!out.contains("<proposed_plan>"));
    assert!(!out.contains("step 1"));
    assert!(out.contains("intro paragraph"));
    assert!(out.contains("tail paragraph"));
}

#[test]
fn strip_proposed_plan_blocks_drops_unterminated_tail() {
    let input = "intro\n<proposed_plan>\nstep 1\nstep 2\n(no close tag)";
    let out = strip_proposed_plan_blocks(input);
    assert!(!out.contains("<proposed_plan>"));
    assert!(!out.contains("step 1"));
    assert_eq!(out.trim(), "intro");
}

#[test]
fn strip_proposed_plan_blocks_removes_multiple_blocks() {
    let input = "a <proposed_plan>plan A</proposed_plan> b <proposed_plan>plan B</proposed_plan> c";
    let out = strip_proposed_plan_blocks(input);
    assert!(!out.contains("plan A"));
    assert!(!out.contains("plan B"));
    assert!(out.contains("a "));
    assert!(out.contains("c"));
}

#[test]
fn strip_proposed_plan_blocks_collapses_blank_line_run() {
    let input = "intro\n\n\n<proposed_plan>body</proposed_plan>\n\n\ntail";
    let out = strip_proposed_plan_blocks(input);
    let blank_runs = out
        .split('\n')
        .collect::<Vec<_>>()
        .windows(3)
        .filter(|w| w.iter().all(|line| line.trim().is_empty()))
        .count();
    assert_eq!(
        blank_runs, 0,
        "should not contain 3+ consecutive blank lines: {out:?}"
    );
}

#[test]
fn strip_proposed_plan_blocks_returns_empty_when_only_block() {
    let out = strip_proposed_plan_blocks("<proposed_plan>plan body</proposed_plan>");
    assert!(out.is_empty(), "expected empty, got {out:?}");
}

#[test]
fn latest_plan_path_returns_none_without_session_id() {
    let root = empty_workspace("no_session_id");
    let plans_dir = session_plans_dir(&root, TEST_SESSION_ID);
    fs::write(plans_dir.join("plan-z.md"), "body").expect("write");
    assert!(latest_plan_path(&root, None).is_none());
    let _ = fs::remove_dir_all(&root);
}
