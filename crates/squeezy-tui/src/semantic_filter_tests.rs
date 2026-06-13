//! Unit tests for the main-view Semantic Filter model (§12.5.2). These cover the
//! pure category logic — projection onto `OverlayFilter`, cycle order, wrap-around
//! stepping, stale-index recovery, and labels — without a terminal. The
//! integration of the filter into the real `render()` (keyboard + mouse + resize
//! + empty case) lives in `lib_tests.rs`.

use super::*;
use crate::OverlayFilter;

#[test]
fn all_is_the_default_and_inactive() {
    assert_eq!(SemanticCategory::default(), SemanticCategory::All);
    assert!(!SemanticCategory::All.is_active());
    assert!(SemanticCategory::Errors.is_active());
    assert!(SemanticCategory::ToolCalls.is_active());
    assert!(SemanticCategory::Tool(0).is_active());
    assert!(SemanticCategory::UserTurns.is_active());
    assert!(SemanticCategory::Assistant.is_active());
}

#[test]
fn projects_onto_overlay_filter_where_a_twin_exists() {
    assert_eq!(
        SemanticCategory::All.to_overlay_filter(),
        Some(OverlayFilter::All)
    );
    assert_eq!(
        SemanticCategory::Errors.to_overlay_filter(),
        Some(OverlayFilter::Errors)
    );
    assert_eq!(
        SemanticCategory::ToolCalls.to_overlay_filter(),
        Some(OverlayFilter::ToolCalls)
    );
    assert_eq!(
        SemanticCategory::Tool(2).to_overlay_filter(),
        Some(OverlayFilter::Tool(2))
    );
    // The two role categories have no single overlay twin (the overlay folds
    // both into `Conversation`), so they project to `None` and the caller
    // applies the role test directly.
    assert_eq!(SemanticCategory::UserTurns.to_overlay_filter(), None);
    assert_eq!(SemanticCategory::Assistant.to_overlay_filter(), None);
}

#[test]
fn cycle_omits_per_tool_entries_with_at_most_one_tool() {
    // No tools / one tool: only the always-present categories appear (a single
    // tool already has `ToolCalls`, so a per-tool entry would be redundant).
    let none = cycle(&[]);
    assert_eq!(
        none,
        vec![
            SemanticCategory::All,
            SemanticCategory::UserTurns,
            SemanticCategory::Assistant,
            SemanticCategory::ToolCalls,
            SemanticCategory::Errors,
        ]
    );
    let one = cycle(&["shell".to_string()]);
    assert_eq!(one, none);
    assert!(!one.iter().any(|c| matches!(c, SemanticCategory::Tool(_))));
}

#[test]
fn cycle_appends_one_entry_per_tool_when_several_appear() {
    let names = vec!["shell".to_string(), "edit".to_string(), "read".to_string()];
    let cats = cycle(&names);
    assert_eq!(
        &cats[..5],
        &[
            SemanticCategory::All,
            SemanticCategory::UserTurns,
            SemanticCategory::Assistant,
            SemanticCategory::ToolCalls,
            SemanticCategory::Errors,
        ]
    );
    assert_eq!(
        &cats[5..],
        &[
            SemanticCategory::Tool(0),
            SemanticCategory::Tool(1),
            SemanticCategory::Tool(2),
        ]
    );
}

#[test]
fn step_walks_forward_and_wraps_back_to_all() {
    let names: Vec<String> = vec![];
    let mut cat = SemanticCategory::All;
    let order = [
        SemanticCategory::UserTurns,
        SemanticCategory::Assistant,
        SemanticCategory::ToolCalls,
        SemanticCategory::Errors,
        SemanticCategory::All, // wraps
    ];
    for expected in order {
        cat = step(cat, &names, false);
        assert_eq!(cat, expected);
    }
}

#[test]
fn step_walks_backward_and_wraps() {
    let names: Vec<String> = vec![];
    // Backward from the resting `All` lands on the LAST real filter (`Errors`).
    let cat = step(SemanticCategory::All, &names, true);
    assert_eq!(cat, SemanticCategory::Errors);
    // ...and forward from there returns to `All`.
    assert_eq!(step(cat, &names, false), SemanticCategory::All);
}

#[test]
fn step_recovers_from_a_stale_tool_index() {
    // The user was on `Tool(5)` but the tool list shrank so it is no longer in
    // the cycle. Forward steps to the first real filter; backward to the last —
    // never stuck on a filter the transcript can no longer satisfy.
    let names = vec!["shell".to_string(), "edit".to_string()];
    let forward = step(SemanticCategory::Tool(5), &names, false);
    assert_eq!(forward, cycle(&names)[1]);
    assert_eq!(forward, SemanticCategory::UserTurns);
    let backward = step(SemanticCategory::Tool(5), &names, true);
    assert_eq!(backward, *cycle(&names).last().unwrap());
}

#[test]
fn step_through_per_tool_entries_round_trips() {
    let names = vec!["shell".to_string(), "edit".to_string()];
    // From `Errors` (last always-present category) forward into the per-tool
    // entries, then wrap back to `All`.
    let mut cat = SemanticCategory::Errors;
    cat = step(cat, &names, false);
    assert_eq!(cat, SemanticCategory::Tool(0));
    cat = step(cat, &names, false);
    assert_eq!(cat, SemanticCategory::Tool(1));
    cat = step(cat, &names, false);
    assert_eq!(cat, SemanticCategory::All);
}

#[test]
fn label_names_the_active_tool_and_degrades_when_out_of_range() {
    let names = vec!["shell".to_string(), "edit".to_string()];
    assert_eq!(SemanticCategory::All.label(&names), "all");
    assert_eq!(SemanticCategory::Errors.label(&names), "errors");
    assert_eq!(SemanticCategory::ToolCalls.label(&names), "tool calls");
    assert_eq!(SemanticCategory::UserTurns.label(&names), "user turns");
    assert_eq!(SemanticCategory::Assistant.label(&names), "assistant");
    assert_eq!(SemanticCategory::Tool(1).label(&names), "tool: edit");
    // Out-of-range index (list shrank): degrade to a bare `tool`, never panic.
    assert_eq!(SemanticCategory::Tool(9).label(&names), "tool");
}
