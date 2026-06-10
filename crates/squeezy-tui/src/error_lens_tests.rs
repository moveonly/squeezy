//! Unit tests for the Error Lenses model (§12.5.6). Pure: no terminal, no
//! rendering — they exercise the per-class detectors, `file:line[:col]`
//! extraction (Unix and Windows shapes), severity, the per-entry cap, the
//! staleness fast path, navigation, and the summary.

use super::*;

fn cand(id: u64, text: &str) -> ErrorCandidate {
    ErrorCandidate {
        id,
        revision: 0,
        text: text.to_string(),
    }
}

#[test]
fn detects_rustc_diagnostic_and_location() {
    let text = "error[E0277]: the trait bound is not satisfied\n  --> src/lib.rs:10:5\n   |";
    let lenses = detect_in_text(7, text);
    assert!(!lenses.is_empty());
    let first = &lenses[0];
    assert_eq!(first.entry_id, 7);
    assert_eq!(first.class, ErrorClass::Rustc);
    assert_eq!(first.severity, ErrorSeverity::Error);
    assert!(first.message.contains("trait bound"));
    // The `-->` continuation line carries the location.
    let loc = lenses
        .iter()
        .find_map(|l| l.location.as_ref())
        .expect("a location was extracted");
    assert_eq!(loc.path, "src/lib.rs");
    assert_eq!(loc.line, 10);
    assert_eq!(loc.col, Some(5));
    assert_eq!(loc.display(), "src/lib.rs:10:5");
}

#[test]
fn detects_cargo_could_not_compile() {
    let lenses = detect_in_text(1, "error: could not compile `squeezy-tui` due to 3 errors");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Cargo);
}

#[test]
fn detects_test_failure() {
    let text = "test foo::bar ... FAILED\nthread panic? no\nassertion failed: left == right";
    let lenses = detect_in_text(2, text);
    let classes: Vec<_> = lenses.iter().map(|l| l.class).collect();
    assert!(classes.contains(&ErrorClass::TestFailure), "{classes:?}");
}

#[test]
fn detects_panic_over_permission() {
    // A panic line that mentions "permission denied" classifies as a panic,
    // because the panic detector runs first (more specific).
    let lenses = detect_in_text(
        3,
        "thread 'main' panicked at 'permission denied': src/x.rs:4",
    );
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Panic);
}

#[test]
fn detects_permission_error() {
    let lenses = detect_in_text(4, "cp: cannot create '/etc/x': Permission denied");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Permission);
}

#[test]
fn detects_network_error() {
    let lenses = detect_in_text(5, "curl: (7) Failed to connect: Connection refused");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Network);
}

#[test]
fn detects_sandbox_denial() {
    let lenses = detect_in_text(6, "sandbox: operation not permitted (denied by policy)");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Sandbox);
}

#[test]
fn warning_lines_are_warnings() {
    let lenses = detect_in_text(8, "warning: unused variable `x`");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Rustc);
    assert_eq!(lenses[0].severity, ErrorSeverity::Warning);
}

#[test]
fn extracts_unix_path_line_only() {
    // A `path:line` (no column) compiler prefix.
    let lenses = detect_in_text(9, "error: src/main.rs:42: something broke");
    let loc = lenses[0].location.as_ref().expect("location");
    assert_eq!(loc.path, "src/main.rs");
    assert_eq!(loc.line, 42);
    assert_eq!(loc.col, None);
    assert_eq!(loc.display(), "src/main.rs:42");
}

#[test]
fn extracts_windows_drive_path() {
    // A Windows `C:\dir\file.rs:10:5` location must keep the drive colon in the
    // path (split-from-the-right) and only take the trailing line:col.
    let lenses = detect_in_text(10, r"error: C:\proj\src\main.rs:10:5: bad");
    let loc = lenses[0].location.as_ref().expect("location");
    assert_eq!(loc.path, r"C:\proj\src\main.rs");
    assert_eq!(loc.line, 10);
    assert_eq!(loc.col, Some(5));
}

#[test]
fn rejects_non_path_colon_pairs() {
    // `error: status:500` has no path-shaped token, so no location is extracted
    // (avoids a false positive from a `key:value` pair).
    let lenses = detect_in_text(11, "error: status:500 returned");
    assert_eq!(lenses.len(), 1);
    assert!(lenses[0].location.is_none(), "no false-positive location");
}

#[test]
fn non_error_lines_produce_no_lenses() {
    let lenses = detect_in_text(12, "compiling squeezy-tui\nfinished in 3.2s\nall good");
    assert!(lenses.is_empty());
}

#[test]
fn ansi_color_is_stripped_before_detection() {
    let lenses = detect_in_text(13, "\x1b[1;31merror[E0382]\x1b[0m: borrow of moved value");
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0].class, ErrorClass::Rustc);
    // The message is the stripped text, with no escape bytes.
    assert!(!lenses[0].message.contains('\u{1b}'));
    assert!(lenses[0].message.contains("E0382"));
}

#[test]
fn per_entry_lens_count_is_capped() {
    // 20 error lines, but only LENSES_PER_ENTRY_CAP are retained.
    let text: String = (0..20)
        .map(|i| format!("error: failure number {i}\n"))
        .collect();
    let lenses = detect_in_text(14, &text);
    assert_eq!(lenses.len(), LENSES_PER_ENTRY_CAP);
}

#[test]
fn long_message_is_capped_with_ellipsis() {
    let long = format!("error: {}", "x".repeat(500));
    let lenses = detect_in_text(15, &long);
    assert!(lenses[0].message.chars().count() <= MESSAGE_CAP + 1);
    assert!(lenses[0].message.ends_with('\u{2026}'));
}

#[test]
fn model_builds_and_lists_across_entries() {
    let mut model = ErrorLenses::new();
    let cands = vec![
        cand(1, "error[E0277]: bad\n --> a.rs:1:1"),
        cand(2, "Connection refused"),
        cand(3, "compiling...\nfine"),
    ];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    assert!(model.rebuild_if_stale(fp, &cands));
    // Entry 3 contributes nothing. Entry 1 contributes the rustc error (and its
    // location line), entry 2 the network error.
    assert!(!model.is_empty());
    assert_eq!(model.count_of(ErrorClass::Network), 1);
    assert!(model.count_of(ErrorClass::Rustc) >= 1);
    // Lenses are in entry order, then line order within an entry.
    assert_eq!(model.lenses()[0].entry_id, 1);
    assert_eq!(model.lenses().last().unwrap().entry_id, 2);
}

#[test]
fn rebuild_is_skipped_when_fingerprint_unchanged() {
    let mut model = ErrorLenses::new();
    let cands = vec![cand(1, "error: boom")];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    assert!(model.rebuild_if_stale(fp, &cands)); // first build runs
    assert!(!model.rebuild_if_stale(fp, &cands)); // unchanged -> fast path
    assert_eq!(model.fingerprint(), fp);

    // A revision bump moves the fingerprint and forces a rebuild.
    let bumped = vec![ErrorCandidate {
        id: 1,
        revision: 1,
        text: "error: boom".to_string(),
    }];
    let fp2 = ErrorLenses::fingerprint_of(bumped.iter());
    assert_ne!(fp, fp2);
    assert!(model.rebuild_if_stale(fp2, &bumped));
}

#[test]
fn empty_transcript_builds_without_repeated_rebuild() {
    let mut model = ErrorLenses::new();
    let cands: Vec<ErrorCandidate> = vec![];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    assert!(model.rebuild_if_stale(fp, &cands)); // first build
    assert!(!model.rebuild_if_stale(fp, &cands)); // empty stays cached
    assert!(model.is_empty());
    assert_eq!(model.summary(), "");
}

#[test]
fn navigation_walks_and_wraps() {
    let mut model = ErrorLenses::new();
    let cands = vec![
        cand(1, "error: a"),
        cand(2, "error: b"),
        cand(3, "error: c"),
    ];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    model.rebuild_if_stale(fp, &cands);
    assert_eq!(model.len(), 3);

    assert_eq!(model.next_index(None), Some(0));
    assert_eq!(model.next_index(Some(0)), Some(1));
    assert_eq!(model.next_index(Some(2)), Some(0)); // wrap
    assert_eq!(model.next_index(Some(99)), Some(0)); // out of range -> first

    assert_eq!(model.prev_index(None), Some(2));
    assert_eq!(model.prev_index(Some(0)), Some(2)); // wrap
    assert_eq!(model.prev_index(Some(2)), Some(1));
}

#[test]
fn navigation_empty_is_none() {
    let model = ErrorLenses::new();
    assert_eq!(model.next_index(None), None);
    assert_eq!(model.prev_index(None), None);
}

#[test]
fn summary_reads_naturally() {
    let mut model = ErrorLenses::new();
    let cands = vec![
        cand(1, "error[E0277]: a"),
        cand(2, "Permission denied"),
        cand(3, "Connection refused"),
    ];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    model.rebuild_if_stale(fp, &cands);
    let summary = model.summary();
    assert!(summary.starts_with("3 errors"), "{summary}");
    assert!(summary.contains("1 rustc"), "{summary}");
    assert!(summary.contains("1 permission"), "{summary}");
    assert!(summary.contains("1 network"), "{summary}");
}

#[test]
fn summary_singular_form() {
    let mut model = ErrorLenses::new();
    let cands = vec![cand(1, "error: boom")];
    let fp = ErrorLenses::fingerprint_of(cands.iter());
    model.rebuild_if_stale(fp, &cands);
    assert!(model.summary().starts_with("1 error \u{00b7}"));
}
