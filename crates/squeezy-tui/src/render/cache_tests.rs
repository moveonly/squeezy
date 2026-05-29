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

#[test]
fn entry_hits_on_same_revision_palette_and_context() {
    let calls = AtomicUsize::new(0);
    let session = next_session_id();

    let first = get_or_compute_entry(session, 0, 7, 3, 0xabcd, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("entry-body")]
    });
    assert_eq!(line_text(&first[0]), "entry-body");

    let second = get_or_compute_entry(session, 0, 7, 3, 0xabcd, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("MISS")]
    });
    assert_eq!(
        line_text(&second[0]),
        "entry-body",
        "second call with matching revision/palette/context must return cached lines",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "compute closure must run exactly once across two matching lookups",
    );
}

#[test]
fn entry_misses_on_bumped_revision() {
    let calls = AtomicUsize::new(0);
    let session = next_session_id();

    let _ = get_or_compute_entry(session, 0, 0, 5, 0xfeed, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("rev-0")]
    });
    let after_bump = get_or_compute_entry(session, 0, 1, 5, 0xfeed, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("rev-1")]
    });

    assert_eq!(
        line_text(&after_bump[0]),
        "rev-1",
        "a bumped entry_revision must invalidate the cached lines",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "compute must run again when entry_revision moves forward",
    );

    // After the rev-1 insert, a re-lookup at rev=1 must hit again so the
    // post-mutation steady state still amortises across redraws.
    let steady = get_or_compute_entry(session, 0, 1, 5, 0xfeed, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("MISS")]
    });
    assert_eq!(line_text(&steady[0]), "rev-1");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "steady-state lookups at the new revision must hit cache",
    );
}

#[test]
fn entry_misses_on_palette_change() {
    let calls = AtomicUsize::new(0);
    let session = next_session_id();

    let _ = get_or_compute_entry(session, 0, 4, 10, 0x1234, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("gen-10")]
    });
    let after_theme = get_or_compute_entry(session, 0, 4, 11, 0x1234, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("gen-11")]
    });

    assert_eq!(
        line_text(&after_theme[0]),
        "gen-11",
        "a moved palette_generation must invalidate the cached lines",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "compute must run again on a palette generation bump",
    );
}

#[test]
fn entry_misses_on_context_change() {
    // Context (selected, width, verbosity, …) is folded into a single
    // opaque hash by the caller. The cache must invalidate when that
    // hash moves even if revision and palette are unchanged — otherwise
    // a selection move would surface the prior entry's selection marker.
    let calls = AtomicUsize::new(0);
    let session = next_session_id();

    let _ = get_or_compute_entry(session, 0, 0, 0, 0xa1, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("ctx-a")]
    });
    let after_ctx = get_or_compute_entry(session, 0, 0, 0, 0xa2, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("ctx-b")]
    });

    assert_eq!(line_text(&after_ctx[0]), "ctx-b");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a context_hash change must force a recompute",
    );
}

#[test]
fn entry_sessions_isolate_identical_ids() {
    // Two independent sessions reusing entry_id=0 (every TuiApp starts
    // at 0) must not collide through the global cache. Otherwise the
    // second session would render the first session's stale entry.
    let calls = AtomicUsize::new(0);
    let s1 = next_session_id();
    let s2 = next_session_id();

    let first = get_or_compute_entry(s1, 0, 0, 0, 0, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("session-one")]
    });
    let second = get_or_compute_entry(s2, 0, 0, 0, 0, || {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("session-two")]
    });
    assert_eq!(line_text(&first[0]), "session-one");
    assert_eq!(line_text(&second[0]), "session-two");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "distinct session_ids must NOT share cache slots even when entry_id matches",
    );
}

#[test]
fn entry_evicts_at_capacity() {
    // Saturate the entry LRU with unique ids and verify the cap holds.
    // Like the markdown variant, we can't claim *only* these entries
    // are in the cache (sibling tests may have populated it), so we
    // assert the loose invariant that the count never exceeds the cap.
    let session = next_session_id();
    for id in 0..=(super::ENTRY_CAPACITY as u64) {
        let _ = get_or_compute_entry(session, id, 0, 0, 0, || vec![Line::from(id.to_string())]);
    }
    assert!(
        super::entry_len() <= super::ENTRY_CAPACITY,
        "entry LRU must never exceed ENTRY_CAPACITY ({})",
        super::ENTRY_CAPACITY,
    );
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
