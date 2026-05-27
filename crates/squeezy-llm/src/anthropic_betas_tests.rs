use std::sync::Arc;

use super::{anthropic_header_value, bedrock_extra_body_betas, dedup_preserving_order};

fn arcs(values: &[&str]) -> Vec<Arc<str>> {
    values.iter().map(|s| Arc::from(*s)).collect()
}

#[test]
fn dedup_keeps_first_occurrence() {
    let input = arcs(&["a", "b", "a", "c", "b"]);
    let out = dedup_preserving_order(&input);
    let out_strs: Vec<&str> = out.iter().map(|s| s.as_ref()).collect();
    assert_eq!(out_strs, vec!["a", "b", "c"]);
}

#[test]
fn header_value_joins_with_commas() {
    let betas = arcs(&["context-1m-2025-08-07", "interleaved-thinking-2025-05-14"]);
    assert_eq!(
        anthropic_header_value(&betas).as_deref(),
        Some("context-1m-2025-08-07,interleaved-thinking-2025-05-14"),
    );
}

#[test]
fn header_value_dedupes_repeated_entries() {
    let betas = arcs(&["context-1m-2025-08-07", "context-1m-2025-08-07"]);
    assert_eq!(
        anthropic_header_value(&betas).as_deref(),
        Some("context-1m-2025-08-07"),
    );
}

#[test]
fn header_value_is_none_when_empty() {
    let betas: Vec<Arc<str>> = Vec::new();
    assert!(anthropic_header_value(&betas).is_none());
}

#[test]
fn bedrock_subset_retains_body_param_betas() {
    let betas = arcs(&[
        "context-1m-2025-08-07",
        "claude-code-20250219",
        "tool-search-tool-2025-10-19",
        "advanced-tool-use-2025-11-20",
    ]);
    let out = bedrock_extra_body_betas(&betas);
    let out_strs: Vec<&str> = out.iter().map(|s| s.as_ref()).collect();
    assert_eq!(
        out_strs,
        vec!["context-1m-2025-08-07", "tool-search-tool-2025-10-19"],
        "header-only betas (claude-code-*, advanced-tool-use-*) must be dropped on Bedrock",
    );
}

#[test]
fn bedrock_subset_empty_when_no_body_param_betas() {
    let betas = arcs(&["claude-code-20250219", "advanced-tool-use-2025-11-20"]);
    let out = bedrock_extra_body_betas(&betas);
    assert!(out.is_empty());
}

#[test]
fn bedrock_subset_dedupes() {
    let betas = arcs(&["context-1m-2025-08-07", "context-1m-2025-08-07"]);
    let out = bedrock_extra_body_betas(&betas);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].as_ref(), "context-1m-2025-08-07");
}
