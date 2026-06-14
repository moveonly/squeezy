use ratatui::style::Color;

use super::*;

/// A representative spread of geometries: the classic 80x24, a wide layout, and
/// a deliberately tiny one that still has to paint a composer. The gate must
/// hold across every one (the spec's resize axis).
const SIZES: &[(u16, u16)] = &[(80, 24), (160, 48), (40, 10)];

// ---------------------------------------------------------------------------
// Metadata — slugs/labels are stable and unique
// ---------------------------------------------------------------------------

#[test]
fn surface_slugs_are_unique_and_stable() {
    let mut slugs: Vec<&str> = Surface::ALL.iter().map(|s| s.slug()).collect();
    let count = slugs.len();
    slugs.sort_unstable();
    slugs.dedup();
    assert_eq!(slugs.len(), count, "surface slugs must be unique");
    assert_eq!(count, 4);
}

#[test]
fn profile_slugs_cover_the_required_platform_matrix() {
    // The spec requires at least one macOS, one Linux/tmux, and one Windows
    // Terminal profile.
    let slugs: Vec<&str> = TerminalProfile::ALL.iter().map(|p| p.slug()).collect();
    assert!(slugs.contains(&"macos_dark"));
    assert!(slugs.contains(&"linux_tmux_dark"));
    assert!(slugs.contains(&"windows_terminal_dark"));
    assert_eq!(slugs.len(), 3);
}

#[test]
fn gate_kind_labels_are_unique() {
    let kinds = [
        GateKind::Contrast,
        GateKind::ScreenReaderText,
        GateKind::MinimalGlyph,
        GateKind::KeyboardReachability,
    ];
    let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    let count = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), count, "gate labels must be unique");
}

#[test]
fn every_gate_carries_a_distinct_prose_explanation() {
    let kinds = [
        GateKind::Contrast,
        GateKind::ScreenReaderText,
        GateKind::MinimalGlyph,
        GateKind::KeyboardReachability,
    ];
    let mut explanations: Vec<&str> = kinds.iter().map(|k| k.explanation()).collect();
    for &why in &explanations {
        assert!(!why.is_empty(), "every gate explains why it matters");
    }
    let count = explanations.len();
    explanations.sort_unstable();
    explanations.dedup();
    assert_eq!(
        explanations.len(),
        count,
        "each gate's explanation is distinct"
    );

    // The enriched Debug form surfaces the label + the explainer alongside the
    // detail, so a CI failure report carries the reasoning, not just the symptom.
    let rendered = format!(
        "{:?}",
        Violation {
            gate: GateKind::ScreenReaderText,
            detail: "required content X not extractable".to_string(),
        }
    );
    assert!(rendered.contains("screen_reader_text"), "{rendered}");
    assert!(
        rendered.contains("screen readers extract text only"),
        "{rendered}"
    );
    assert!(rendered.contains("required content X"), "{rendered}");
}

// ---------------------------------------------------------------------------
// Capture — the integration path: drives the real render() over a TestBackend
// ---------------------------------------------------------------------------

#[test]
fn capture_drives_real_render_into_a_full_grid() {
    // The integration test of record: a surface captured through the real
    // `render()` fills exactly the viewport with real cells.
    let captured = AuditSurface::capture(Surface::ShortChat, TerminalProfile::MacosDark, 80, 24);
    assert_eq!(captured.cells.len(), 80 * 24);
    assert_eq!(captured.surface, Surface::ShortChat);
    assert_eq!(captured.profile, TerminalProfile::MacosDark);
    // The short chat's prose must actually be painted somewhere on screen.
    let text = captured.screen_reader_text();
    assert!(
        text.contains("Clamp the offset"),
        "rendered surface must extract the assistant prose:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// The gate itself: every built-in surface passes every gate at every size,
// under every required terminal profile. This is the assertion that "fails when
// a rendered surface violates the gate".
// ---------------------------------------------------------------------------

#[test]
fn every_surface_passes_the_full_gate_across_sizes_and_profiles() {
    for &surface in &Surface::ALL {
        for &profile in &TerminalProfile::ALL {
            for &(w, h) in SIZES {
                let report = audit(surface, profile, w, h);
                assert!(
                    report.passed(),
                    "surface {} on profile {} at {w}x{h} violated the a11y gate: {:#?}",
                    surface.slug(),
                    profile.slug(),
                    report.violations,
                );
            }
        }
    }
}

#[test]
fn empty_surface_is_chrome_only_but_still_passes() {
    // Edge case: an empty transcript paints only chrome + caret. It has no
    // required content text, so the screen-reader gate is a no-op, and the
    // remaining gates must still pass.
    let report = audit(Surface::Empty, TerminalProfile::LinuxTmuxDark, 80, 24);
    assert!(
        report.passed(),
        "empty surface must pass the gate: {:#?}",
        report.violations
    );
    assert!(
        Surface::Empty.required_text().is_empty(),
        "empty surface declares no required content"
    );
}

#[test]
fn tiny_terminal_still_passes_the_gate() {
    // Resize-to-tiny edge: a 20x6 surface that still must paint a composer. At
    // this extreme size a long answer is legitimately truncated, so the
    // *content* (screen-reader) gate is exercised on the chrome-only Empty
    // surface — what we assert here is that the contrast / minimal-glyph /
    // keyboard gates hold even at a viewport this small. (The 40x10 case in the
    // full-matrix sweep already proves content stays extractable on a small but
    // usable terminal.)
    let report = audit(Surface::Empty, TerminalProfile::WindowsTerminalDark, 20, 6);
    assert!(
        report.passed(),
        "tiny surface violated the gate: {:#?}",
        report.violations,
    );
}

#[test]
fn content_stays_extractable_across_a_resize() {
    // Resize where the gate paints: the same app captured wide and then narrow
    // must keep its tail content screen-reader extractable through both. The
    // narrow capture is the post-resize frame; the gate must still pass.
    let app = {
        let mut a = new_audit_app();
        a.push_transcript_item(TranscriptItem::user("a question"));
        a.push_transcript_item(TranscriptItem::assistant("the fix is local"));
        a
    };
    for &(w, h) in &[(160u16, 48u16), (60, 18)] {
        let report = audit_app(&app, Surface::LongSession, TerminalProfile::MacosDark, w, h);
        // LongSession's required tail content ("the fix is local") is exactly
        // this app's latest answer, so it must stay extractable at both sizes.
        assert!(
            report.passed(),
            "resize to {w}x{h} broke the a11y gate: {:#?}",
            report.violations,
        );
    }
}

// ---------------------------------------------------------------------------
// Contrast gate — formula correctness, pass, and a staged failure
// ---------------------------------------------------------------------------

#[test]
fn contrast_ratio_matches_wcag_reference_values() {
    // Black on white is the canonical 21:1.
    let bw = contrast_ratio((0, 0, 0), (255, 255, 255));
    assert!(
        (bw - 21.0).abs() < 0.01,
        "black/white must be 21:1, got {bw}"
    );
    // A color against itself is exactly 1:1.
    let same = contrast_ratio((30, 30, 30), (30, 30, 30));
    assert!(
        (same - 1.0).abs() < 1e-9,
        "identical colors are 1:1, got {same}"
    );
    // Symmetric: order of arguments does not matter.
    let a = contrast_ratio((10, 80, 200), (240, 240, 240));
    let b = contrast_ratio((240, 240, 240), (10, 80, 200));
    assert!((a - b).abs() < 1e-9, "contrast ratio must be symmetric");
}

#[test]
fn relative_luminance_is_monotonic_and_bounded() {
    assert!((relative_luminance((0, 0, 0))).abs() < 1e-9, "black is 0");
    assert!(
        (relative_luminance((255, 255, 255)) - 1.0).abs() < 1e-9,
        "white is 1"
    );
    assert!(relative_luminance((128, 128, 128)) > relative_luminance((64, 64, 64)));
}

#[test]
fn contrast_gate_flags_an_invisible_glyph() {
    // Stage a surface where a real glyph sits on a near-identical background:
    // dark-gray text on the macOS reference black is fine, but pure black text
    // on black is invisible. Build a bespoke captured surface with one such
    // cell and confirm the gate catches it.
    let mut captured =
        AuditSurface::capture(Surface::ShortChat, TerminalProfile::MacosDark, 80, 24);
    // Find the first painted letter and recolor it black-on-black.
    let idx = captured
        .cells
        .iter()
        .position(|c| !c.is_blank() && c.symbol.chars().all(|ch| ch.is_ascii_alphabetic()))
        .expect("a painted letter exists");
    captured.cells[idx].fg = Color::Rgb(0, 0, 0);
    captured.cells[idx].bg = Color::Reset; // → reference black bg
    let report = audit_surface(&captured);
    let contrast = report.of_kind(GateKind::Contrast);
    assert_eq!(
        contrast.len(),
        1,
        "exactly one contrast violation for the black-on-black cell: {:#?}",
        report.violations
    );
    assert!(contrast[0].detail.contains("contrast"));
}

#[test]
fn contrast_gate_skips_reset_and_indexed_colors() {
    // A cell whose fg is Reset (inherits the terminal default) or Indexed (a
    // palette we cannot resolve) is outside our color contract and must not be
    // flagged — those degrade to the terminal's own scheme.
    let mut captured =
        AuditSurface::capture(Surface::ShortChat, TerminalProfile::LinuxTmuxDark, 80, 24);
    let idx = captured
        .cells
        .iter()
        .position(|c| !c.is_blank())
        .expect("a painted cell exists");
    captured.cells[idx].fg = Color::Indexed(8);
    captured.cells[idx].bg = Color::Reset;
    let report = audit_surface(&captured);
    // The recolored cell contributes no contrast violation (the rest of the
    // built-in surface already passes, so any contrast violation would be this
    // cell). The whole surface should still pass contrast.
    assert!(
        report.of_kind(GateKind::Contrast).is_empty(),
        "indexed/reset colors must be skipped, not flagged: {:#?}",
        report.violations
    );
}

// ---------------------------------------------------------------------------
// Screen-reader text gate — content is extractable as words, not color/glyph
// ---------------------------------------------------------------------------

#[test]
fn screen_reader_text_extracts_content_without_chrome() {
    let captured = AuditSurface::capture(Surface::SystemNotice, TerminalProfile::MacosDark, 80, 24);
    let text = captured.screen_reader_text();
    // Both the system notice and the typed composer text must be present as
    // real words — proof meaning is not color-only.
    assert!(text.contains("sandbox denied"), "notice text:\n{text}");
    assert!(text.contains("still typing"), "composer text:\n{text}");
    // The composer caret (a chrome glyph) must NOT leak into the readable text.
    assert!(
        !text.contains('┃'),
        "the composer caret is chrome, not readable text:\n{text}"
    );
}

#[test]
fn screen_reader_gate_fails_when_content_is_missing() {
    // Stage a surface whose required content is absent from the cell grid (an
    // empty app captured but labelled ShortChat). The gate must flag the
    // missing words rather than silently pass.
    let app = new_audit_app();
    let captured =
        AuditSurface::capture_app(&app, Surface::ShortChat, TerminalProfile::MacosDark, 80, 24);
    let report = audit_surface(&captured);
    let sr = report.of_kind(GateKind::ScreenReaderText);
    assert!(
        !sr.is_empty(),
        "missing content must trip the screen-reader gate"
    );
    assert!(
        sr.iter().any(|v| v.detail.contains("Clamp the offset")),
        "the missing-content violation must name the absent phrase: {sr:#?}"
    );
}

#[test]
fn screen_reader_gate_is_noop_for_chrome_only_surface() {
    let captured = AuditSurface::capture(Surface::Empty, TerminalProfile::MacosDark, 80, 24);
    let report = audit_surface(&captured);
    assert!(
        report.of_kind(GateKind::ScreenReaderText).is_empty(),
        "the chrome-only Empty surface has no required content"
    );
}

// ---------------------------------------------------------------------------
// Minimal-glyph gate — chrome stays inside the bounded set; content is exempt
// ---------------------------------------------------------------------------

#[test]
fn minimal_glyph_gate_allows_content_glyphs() {
    // Stage a surface with CJK *content* (user prose). Content is exempt from
    // the minimal-glyph gate — only chrome is audited — so this must pass.
    let mut app = new_audit_app();
    app.push_transcript_item(TranscriptItem::user("explain this 関数 trace 部分"));
    app.push_transcript_item(TranscriptItem::assistant(
        "これは説明です — the fix is local.",
    ));
    let captured = AuditSurface::capture_app(
        &app,
        Surface::LongSession,
        TerminalProfile::MacosDark,
        80,
        24,
    );
    let report = audit_surface(&captured);
    assert!(
        report.of_kind(GateKind::MinimalGlyph).is_empty(),
        "CJK content must not trip the minimal-glyph gate: {:#?}",
        report.violations
    );
}

#[test]
fn minimal_glyph_gate_flags_a_stray_chrome_block_glyph() {
    // Inject a chrome-block glyph (a non-allowed geometric shape) into a chrome
    // position and confirm the gate catches it.
    let mut captured =
        AuditSurface::capture(Surface::LongSession, TerminalProfile::MacosDark, 80, 24);
    // U+25C6 BLACK DIAMOND is in the geometric-shapes block but NOT in the
    // allowed chrome set — a deliberately non-minimal chrome glyph.
    let idx = captured
        .cells
        .iter()
        .position(|c| !c.is_blank())
        .expect("a painted cell exists");
    captured.cells[idx].symbol = "◆".to_string();
    let report = audit_surface(&captured);
    let mg = report.of_kind(GateKind::MinimalGlyph);
    assert_eq!(
        mg.len(),
        1,
        "the stray diamond must trip the minimal-glyph gate: {:#?}",
        report.violations
    );
    assert!(mg[0].detail.contains("non-minimal"));
}

#[test]
fn allowed_chrome_glyphs_are_recognized_as_chrome() {
    // Every allowed chrome glyph must be classified as chrome (so it is stripped
    // from the screen-reader stream and exempt from the minimal-glyph gate).
    for &g in ALLOWED_CHROME_GLYPHS {
        assert!(
            is_chrome_glyph(g),
            "allowed chrome glyph {g:?} must be recognized as chrome"
        );
    }
}

// ---------------------------------------------------------------------------
// Keyboard reachability gate — every mouse affordance has a keyboard twin
// ---------------------------------------------------------------------------

#[test]
fn every_mouse_affordance_has_a_keyboard_equivalent() {
    let mut violations = Vec::new();
    keyboard_reachability_gate(&mut violations);
    assert!(
        violations.is_empty(),
        "every mouse affordance must have a keyboard equivalent: {violations:#?}"
    );
}

#[test]
fn keyboard_equivalent_is_total_over_the_mouse_vocabulary() {
    // Exhaustiveness: every entry in the audited mouse-action set resolves to a
    // keyboard path. (The `match` in `keyboard_equivalent` is itself exhaustive,
    // so a new variant breaks the build — this guards the AUDIT_ALL list too.)
    for &action in interaction::Action::AUDIT_ALL {
        assert!(
            keyboard_equivalent(action).is_some(),
            "mouse affordance {action:?} resolved to no keyboard path"
        );
    }
}

#[test]
fn keyboard_paths_point_at_real_keymap_actions() {
    // The keymap-backed paths must reference actions that exist in the keymap's
    // own ALL registry, so a rename can't leave a dangling reference.
    use crate::keymap::Action as KA;
    for &action in interaction::Action::AUDIT_ALL {
        if let Some(KeyboardPath::Keymap(ka)) = keyboard_equivalent(action) {
            assert!(
                KA::ALL.contains(&ka),
                "keyboard path for {action:?} references unknown keymap action {ka:?}"
            );
        }
    }
}

#[test]
fn keyboard_paths_resolve_in_the_default_keymap() {
    // Non-vacuity: every `Keymap(action)` keyboard path must genuinely resolve
    // in the default keymap (its default binding round-trips back to it under the
    // reverse lookup). A mouse-only action that names a keymap action which is
    // shadowed by a colliding default would be unreachable from the keyboard.
    let default_keymap =
        crate::keymap::KeymapResolver::from_overrides(&std::collections::BTreeMap::new());
    let mut violations = Vec::new();
    keyboard_reachability_gate_with(&default_keymap, &mut violations);
    assert!(
        violations.is_empty(),
        "every keymap-backed mouse affordance must resolve in the default keymap: {violations:#?}"
    );
}

#[test]
fn keyboard_reachability_gate_flags_a_shadowed_keymap_path() {
    // The gate is a *checked* one, not a string claim: if a `Keymap(action)`
    // path's target is shadowed in the keymap (some other action wins its key in
    // the reverse lookup), the gate must report a violation naming it. Here we
    // rebind `open_search` onto QueueUndo's default `u`, so `u` no longer
    // resolves to QueueUndo (the QueueUndo keymap-path target) and the gate
    // catches it. Without the resolution check this scenario is silently passed.
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("open_search".to_string(), "u".to_string());
    let shadowed = crate::keymap::KeymapResolver::from_overrides(&overrides);
    let mut violations = Vec::new();
    keyboard_reachability_gate_with(&shadowed, &mut violations);
    assert!(
        violations
            .iter()
            .any(|v| v.detail.contains("QueueUndo") && v.detail.contains("does not resolve")),
        "gate must flag the shadowed QueueUndo keymap path: {violations:#?}"
    );
}

// ---------------------------------------------------------------------------
// audit_app — the bespoke-app entry point
// ---------------------------------------------------------------------------

#[test]
fn audit_app_passes_for_a_clean_built_app() {
    let mut app = new_audit_app();
    app.push_transcript_item(TranscriptItem::user("a normal prompt"));
    app.push_transcript_item(TranscriptItem::assistant("a normal answer"));
    let report = audit_app(&app, Surface::Empty, TerminalProfile::MacosDark, 80, 24);
    assert!(
        report.passed(),
        "a clean app must pass the gate: {:#?}",
        report.violations
    );
}

#[test]
fn every_painted_chrome_glyph_is_in_the_allow_list() {
    // The minimal-glyph gate's allow-list must be a *superset* of every
    // chrome-block glyph the real renderer actually paints across the surface
    // and size matrix — otherwise the gate would flag genuine Squeezy chrome.
    // This is the regression guard that keeps the allow-list honest: if a future
    // renderer change introduces a new chrome glyph, this test fails and forces
    // an explicit allow-list decision.
    let mut missing: std::collections::BTreeSet<char> = std::collections::BTreeSet::new();
    for &surface in &Surface::ALL {
        for &(w, h) in SIZES {
            let captured = AuditSurface::capture(surface, TerminalProfile::MacosDark, w, h);
            for c in &captured.cells {
                if c.is_blank() {
                    continue;
                }
                let g = c.symbol.chars().next().unwrap_or(' ');
                if !g.is_ascii()
                    && looks_like_chrome_block(g)
                    && !ALLOWED_CHROME_GLYPHS.contains(&g)
                {
                    missing.insert(g);
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "renderer paints chrome glyphs absent from ALLOWED_CHROME_GLYPHS: {}",
        missing
            .iter()
            .map(|g| format!("{g:?} (U+{:04X})", *g as u32))
            .collect::<Vec<_>>()
            .join(", ")
    );
}
