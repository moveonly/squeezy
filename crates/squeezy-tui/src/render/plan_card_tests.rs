use super::{PlanCardData, render_plan_card, render_plan_diff};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proposed_plan::{self, PlanMeta};
use crate::render::palette;

const TEST_SESSION: &str = "card-tests";

fn fresh_workspace(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy_card_{label}_{nonce}"));
    std::fs::create_dir_all(&root).expect("mkdir workspace");
    root
}

fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

#[test]
fn render_plan_card_shows_id_path_and_step_count() {
    let root = fresh_workspace("header");
    let body = "Context: doc tweak.\n\n1. Edit README\n2. Verify links\n";
    let (plan_id, path) =
        proposed_plan::persist_plan(&root, TEST_SESSION, body, &PlanMeta::default())
            .expect("persist plan");
    let data = PlanCardData {
        plan_id: plan_id.clone(),
        path,
        parent_plan_id: None,
    };
    let lines = render_plan_card(&data);
    assert!(!lines.is_empty());
    let header = line_text(&lines[0]);
    assert!(
        header.contains(&plan_id),
        "header must include id: {header}"
    );
    assert!(
        header.contains("· 2 steps"),
        "header must include step count: {header}"
    );
    // Path line is second.
    let path_line = line_text(&lines[1]);
    assert!(path_line.contains(TEST_SESSION));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn render_plan_card_uses_amber_box_not_full_amber_body() {
    let root = fresh_workspace("amber_box");
    let body = "Context\n\n1. Edit README\n";
    let (plan_id, path) =
        proposed_plan::persist_plan(&root, TEST_SESSION, body, &PlanMeta::default())
            .expect("persist plan");
    let data = PlanCardData {
        plan_id,
        path,
        parent_plan_id: None,
    };

    let lines = render_plan_card(&data);
    let top = line_text(&lines[0]);
    let path = line_text(&lines[1]);
    let body = lines
        .iter()
        .find(|line| line_text(line).contains("Context"))
        .expect("body line");

    assert!(top.starts_with("╭─ Plan "), "{top}");
    assert_eq!(lines[0].spans[0].style.fg, Some(palette::AMBER));
    assert!(path.starts_with("│ "), "{path}");
    assert!(
        body.spans
            .iter()
            .any(|span| span.content.contains("Context") && span.style.fg != Some(palette::AMBER)),
        "body text should not be painted amber: {body:?}"
    );
    assert!(
        line_text(lines.last().expect("bottom border")).starts_with('╰'),
        "bottom border missing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn render_plan_card_emits_diff_when_parent_exists() {
    let root = fresh_workspace("diff_parent");
    let (parent_id, _) = proposed_plan::persist_plan(
        &root,
        TEST_SESSION,
        "step one\nstep two\n",
        &PlanMeta::default(),
    )
    .expect("persist parent");
    let (child_id, child_path) = proposed_plan::persist_plan(
        &root,
        TEST_SESSION,
        "step one\nstep TWO\nstep three\n",
        &PlanMeta {
            parent_plan_id: Some(parent_id.clone()),
            model: None,
        },
    )
    .expect("persist child");
    let data = PlanCardData {
        plan_id: child_id,
        path: child_path,
        parent_plan_id: Some(parent_id.clone()),
    };
    let rendered: Vec<String> = render_plan_card(&data).iter().map(line_text).collect();
    let joined = rendered.join("\n");
    assert!(
        joined.contains(&format!("diff vs {parent_id}")),
        "diff header should reference parent: {joined}"
    );
    assert!(
        joined.contains("+ step three"),
        "diff should show the added line: {joined}"
    );
    assert!(
        joined.contains("- step two"),
        "diff should show the removed line: {joined}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn render_plan_card_handles_missing_file_gracefully() {
    let root = fresh_workspace("missing");
    let phantom = root.join("nope.md");
    let data = PlanCardData {
        plan_id: "plan-phantom".to_string(),
        path: phantom,
        parent_plan_id: None,
    };
    let lines = render_plan_card(&data);
    assert!(line_text(&lines[0]).contains("file missing"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn render_plan_diff_marks_additions_and_deletions() {
    let lines = render_plan_diff("alpha\nbeta\n", "alpha\ngamma\n");
    let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("+ gamma"));
    assert!(joined.contains("- beta"));
    assert!(
        joined.contains("  alpha"),
        "context line preserved: {joined}"
    );
}
