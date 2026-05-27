//! Cache plumbing tests. The render-side end-to-end tests live next to
//! each renderer (`markdown_tests.rs`, `highlight_tests.rs`,
//! `diff_tests.rs`) and exercise the cached APIs implicitly.
//!
//! Each test uses content/path unique to the test so it never collides
//! with sibling render tests in the same `cargo test` binary, which keeps
//! these checks order-independent without forcing serial execution.

use super::*;
use ratatui::text::Line;
use std::sync::atomic::{AtomicUsize, Ordering};

fn unique(tag: &str, i: usize) -> String {
    format!("__cache_test::{tag}::{i}")
}

#[test]
fn markdown_hits_on_repeat() {
    let calls = AtomicUsize::new(0);
    let key = unique("md_hits", 0);

    let first = get_or_compute_markdown(&key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("first")]
    });
    assert_eq!(line_text(&first[0]), "first");

    let second = get_or_compute_markdown(&key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("MISS")]
    });
    assert_eq!(
        line_text(&second[0]),
        "first",
        "second call should return cached value"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second call should not invoke the compute closure"
    );
}

#[test]
fn highlight_keys_on_language() {
    let calls = AtomicUsize::new(0);
    let src = unique("hi_lang", 0);

    let _ = get_or_compute_highlight(&src, "rust", || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("rust")]
    });
    let _ = get_or_compute_highlight(&src, "go", || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("go")]
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "different languages on the same source must compute separately"
    );

    let third = get_or_compute_highlight(&src, "rust", || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("MISS")]
    });
    assert_eq!(line_text(&third[0]), "rust");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "repeat of (source, rust) must hit cache"
    );
}

#[test]
fn diff_keys_on_path_and_content() {
    let calls = AtomicUsize::new(0);
    let patch = format!("@@ -1 +1 @@\n-{}\n+{}", unique("diff_path", 0), "y");
    let path_a = format!("{}/a.rs", unique("diff_path", 0));
    let path_b = format!("{}/b.rs", unique("diff_path", 0));

    let _ = get_or_compute_diff(&path_a, &patch, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("a")]
    });
    let _ = get_or_compute_diff(&path_b, &patch, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("b")]
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "same patch on different paths must produce separate compute calls"
    );

    let _ = get_or_compute_diff(&path_a, &patch, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("MISS")]
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "second call to (path_a, same patch) must hit cache"
    );
}

#[test]
fn markdown_evicts_at_capacity() {
    // Don't share the global cache with sibling tests; just verify the
    // LRU cap is honoured by counting how many unique inputs are retained.
    let probe = unique("md_evict", 0);

    for i in 0..=super::MARKDOWN_CAPACITY {
        let key = format!("{probe}::{i}");
        let _ = get_or_compute_markdown(&key, || vec![Line::from(key.clone())]);
    }
    // Cap on the global cache (other tests may add entries too) bounds the
    // total size. This is a loose invariant — the strict guarantee is that
    // a single test workload above the cap can't blow past it.
    assert!(
        super::markdown_len() <= super::MARKDOWN_CAPACITY,
        "markdown LRU must never exceed MARKDOWN_CAPACITY ({})",
        super::MARKDOWN_CAPACITY
    );
}

#[test]
fn hash_content_changes_with_input() {
    assert_ne!(hash_content(b"alpha"), hash_content(b"beta"));
    assert_eq!(hash_content(b"alpha"), hash_content(b"alpha"));
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
