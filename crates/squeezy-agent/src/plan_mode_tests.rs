use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use squeezy_core::SessionMode;

use super::{PLAN_DIR, PLAN_MODE_INSTRUCTIONS, instructions_for_mode, latest_plan_path};

fn empty_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy_plan_mode_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("mkdir workspace");
    root
}

#[test]
fn plan_mode_appends_overlay() {
    let root = empty_workspace("appends_overlay");
    let out = instructions_for_mode("base", SessionMode::Plan, &root);
    assert!(out.starts_with("base"));
    assert!(out.contains(PLAN_MODE_INSTRUCTIONS));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn build_mode_returns_base_verbatim() {
    let root = empty_workspace("build_verbatim");
    let out = instructions_for_mode("base instructions", SessionMode::Build, &root);
    assert_eq!(out, "base instructions");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plan_mode_instructions_are_concise() {
    assert!(
        PLAN_MODE_INSTRUCTIONS.len() <= 700,
        "PLAN_MODE_INSTRUCTIONS length {} > 700",
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
fn refinement_hint_added_when_plan_file_present() {
    let root = empty_workspace("hint_present");
    let plans_dir = root.join(PLAN_DIR);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_file = plans_dir.join("plan-abc123.md");
    fs::write(&plan_file, "step 1\n").expect("write plan");

    let out = instructions_for_mode("base", SessionMode::Plan, &root);
    assert!(out.contains("Active plan file:"));
    assert!(out.contains(plan_file.to_str().expect("utf-8")));
    assert!(out.contains("read this file first with read_file"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn refinement_hint_absent_when_no_plan_files() {
    let root = empty_workspace("hint_absent");
    let out = instructions_for_mode("base", SessionMode::Plan, &root);
    assert!(!out.contains("Active plan file:"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_picks_newest_by_mtime() {
    let root = empty_workspace("latest_by_mtime");
    let plans_dir = root.join(PLAN_DIR);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let older = plans_dir.join("plan-old.md");
    let newer = plans_dir.join("plan-new.md");
    fs::write(&older, "old").expect("write older");
    fs::write(&newer, "new").expect("write newer");
    // Drive mtimes explicitly so the test does not rely on the OS clock's
    // resolution between two write calls (some filesystems quantise to a
    // second or more).
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
    let picked = latest_plan_path(&root).expect("latest exists");
    assert_eq!(picked, newer);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn latest_plan_path_ignores_non_markdown_files() {
    let root = empty_workspace("ignore_non_md");
    let plans_dir = root.join(PLAN_DIR);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    fs::write(plans_dir.join("README"), "not a plan").expect("write readme");
    fs::write(plans_dir.join("notes.txt"), "not a plan").expect("write notes");
    assert!(latest_plan_path(&root).is_none());
    let _ = fs::remove_dir_all(&root);
}
