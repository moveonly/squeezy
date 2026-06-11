//! Unit tests for the Automatic Degraded-Mode Suggestions engine (§12.9.4). Pure,
//! terminal-free coverage of the spec's checklist: the per-condition detection
//! (mangled wide glyphs, tiny size, no color, unreliable remote/multiplexed probe),
//! the confidence floor ("incorrect suggestion is worse than unknown"), the target
//! that only moves knobs not already at the suggested value, the settle delay, the
//! accept/dismiss latch, modal suppression, the disabled-config path, and —
//! critically — the no-idle-redraw-loop / zero-idle-cost invariant.

use super::*;

use crate::dogfood::TerminalProfile as TerminalKind;
use crate::terminal_profile::TerminalCapabilities;

/// A capability probe for `kind` with every environment hint cleared — the base the
/// per-condition tests perturb one flag at a time.
fn caps(kind: TerminalKind) -> TerminalCapabilities {
    TerminalCapabilities {
        kind,
        truecolor_env: false,
        no_color: false,
        over_ssh: false,
        inside_multiplexer: false,
    }
}

/// A roomy, capable size + modes so the only degradation in a test is the one it
/// sets — used as the "not degraded" baseline.
const ROOMY_COLS: u16 = 120;
const ROOMY_ROWS: u16 = 40;

/// Detect with the current (capable) defaults: full Unicode chrome, auto density,
/// mouse enabled.
fn detect_capable(caps: TerminalCapabilities, cols: u16, rows: u16) -> Option<DegradedSuggestion> {
    DegradedModeSuggestor::detect(
        caps,
        cols,
        rows,
        GlyphMode::Unicode,
        DensityMode::Auto,
        MouseMode::Enabled,
    )
}

/// `now` advanced past the settle window so a settled candidate actually paints.
fn settled(base: std::time::Instant) -> std::time::Instant {
    base + SUGGEST_SETTLE + Duration::from_millis(1)
}

#[test]
fn a_capable_modern_terminal_is_not_degraded() {
    // iTerm2 at a roomy size with color and no remote hop: nothing to suggest.
    let suggestion = detect_capable(caps(TerminalKind::MacosIterm2), ROOMY_COLS, ROOMY_ROWS);
    assert!(
        suggestion.is_none(),
        "a capable terminal yields no suggestion: {suggestion:?}",
    );
}

#[test]
fn vscode_mangled_wide_glyphs_is_detected_and_suggests_ascii() {
    // VS Code / xterm.js mis-renders wide glyphs — the lead, high-confidence signal.
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS)
        .expect("VS Code is degraded by its wide-glyph quirk");
    assert_eq!(suggestion.lead_signal(), DegradedSignal::MangledWideGlyphs);
    assert!(
        suggestion
            .signals()
            .contains(&DegradedSignal::MangledWideGlyphs),
        "the glyph signal is present",
    );
    // The target suggests ASCII chrome (glyphs are the problem) but leaves mouse
    // alone (no mouse-relevant signal fired) and only pins compact if not already.
    assert_eq!(suggestion.target().glyph_mode, Some(GlyphMode::Ascii));
    assert_eq!(
        suggestion.target().mouse,
        None,
        "no mouse signal ⇒ mouse untouched"
    );
}

#[test]
fn tiny_window_is_detected_and_suggests_compact() {
    // A genuinely cramped window on an otherwise-capable terminal.
    let suggestion = detect_capable(caps(TerminalKind::MacosIterm2), TINY_COLS, TINY_ROWS)
        .expect("a tiny window is degraded");
    assert_eq!(suggestion.lead_signal(), DegradedSignal::TinySize);
    assert_eq!(suggestion.target().density, Some(DensityMode::Compact));
    // No glyph/mouse problem on iTerm2 at a tiny size, only density moves… but the
    // suggestion still suggests ASCII because Unicode is not the safe floor — assert
    // the density knob specifically.
    assert!(
        suggestion.target().density.is_some(),
        "the tiny-size suggestion pins compact density",
    );
}

#[test]
fn one_row_over_the_tiny_floor_is_not_tiny() {
    // Just above the tiny floor on a capable terminal ⇒ not degraded (Adaptive
    // Density handles merely-narrow silently; the banner only fires on genuinely
    // cramped windows).
    let suggestion = detect_capable(
        caps(TerminalKind::MacosIterm2),
        TINY_COLS + 1,
        TINY_ROWS + 1,
    );
    assert!(
        suggestion.is_none(),
        "one row/col over the tiny floor is not degraded: {suggestion:?}",
    );
}

#[test]
fn no_color_is_detected() {
    let mut c = caps(TerminalKind::MacosIterm2);
    c.no_color = true;
    let suggestion = detect_capable(c, ROOMY_COLS, ROOMY_ROWS).expect("NO_COLOR is degraded");
    assert_eq!(suggestion.lead_signal(), DegradedSignal::NoColor);
}

#[test]
fn a_lone_unreliable_probe_stays_under_the_confidence_floor() {
    // An SSH hop alone, on an otherwise-capable+roomy+colored terminal, is a single
    // LOW-confidence signal: the spec's "incorrect suggestion is worse than unknown"
    // floor means it does NOT cross the threshold on its own.
    let mut c = caps(TerminalKind::LinuxXterm);
    c.over_ssh = true;
    let suggestion = detect_capable(c, ROOMY_COLS, ROOMY_ROWS);
    assert!(
        suggestion.is_none(),
        "a lone unreliable-probe stays under the floor: {suggestion:?}",
    );
}

#[test]
fn an_unreliable_probe_corroborates_a_stronger_signal_and_suggests_mouse_off() {
    // SSH (mouse-relevant, low confidence) PLUS no-color (sure) crosses the floor,
    // and because a mouse-relevant signal fired the target also suggests mouse off.
    let mut c = caps(TerminalKind::LinuxXterm);
    c.over_ssh = true;
    c.no_color = true;
    let suggestion =
        detect_capable(c, ROOMY_COLS, ROOMY_ROWS).expect("a corroborated case crosses the floor");
    assert!(
        suggestion
            .signals()
            .contains(&DegradedSignal::UnreliableProbe)
    );
    assert!(suggestion.signals().contains(&DegradedSignal::NoColor));
    assert_eq!(
        suggestion.target().mouse,
        Some(MouseMode::Disabled),
        "a mouse-relevant signal suggests mouse off",
    );
    // One sure signal (no-color, weight 3) + one weak probe (weight 1) = 4: still
    // "med", because the weak probe alone must not inflate the badge to "high"
    // (the spec's "incorrect suggestion is worse than unknown" caution).
    assert_eq!(suggestion.confidence_label(), "med");
}

#[test]
fn two_sure_signals_read_as_high_confidence() {
    // VS Code (mangled wide glyphs, weight 3) at a tiny size (weight 3) = 6: a
    // strongly corroborated case reads "high".
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), TINY_COLS, TINY_ROWS)
        .expect("a doubly-degraded terminal is degraded");
    assert!(suggestion.confidence() >= SUGGEST_CONFIDENCE_FLOOR * 2);
    assert_eq!(suggestion.confidence_label(), "high");
}

#[test]
fn confidence_floor_admits_one_sure_signal_but_not_one_weak_one() {
    // Guard the tuning: a single sure signal clears the floor, a single weak one
    // does not. These are method calls on runtime values, so a plain assert is the
    // right tool (no assertions_on_constants concern).
    assert!(DegradedSignal::NoColor.confidence() >= SUGGEST_CONFIDENCE_FLOOR);
    assert!(DegradedSignal::UnreliableProbe.confidence() < SUGGEST_CONFIDENCE_FLOOR);
}

#[test]
fn target_skips_knobs_already_at_the_suggested_value() {
    // A degraded VS Code session ALREADY on ASCII chrome + compact density: the
    // glyph and density knobs are skipped, leaving an empty target ⇒ nothing to
    // suggest (no point nagging about modes already in effect).
    let suggestion = DegradedModeSuggestor::detect(
        caps(TerminalKind::MacosVscode),
        ROOMY_COLS,
        ROOMY_ROWS,
        GlyphMode::Ascii,
        DensityMode::Compact,
        MouseMode::Enabled,
    );
    assert!(
        suggestion.is_none(),
        "already at every suggested mode ⇒ no suggestion: {suggestion:?}",
    );
}

#[test]
fn target_summary_lists_only_the_knobs_that_move() {
    let target = DegradedTarget {
        glyph_mode: Some(GlyphMode::Ascii),
        density: None,
        mouse: Some(MouseMode::Disabled),
    };
    assert!(!target.is_empty());
    assert_eq!(target.summary(), "ASCII chrome + mouse off");

    let empty = DegradedTarget {
        glyph_mode: None,
        density: None,
        mouse: None,
    };
    assert!(empty.is_empty());
    assert_eq!(empty.summary(), "");
}

#[test]
fn every_signal_has_a_slug_an_impact_and_a_positive_confidence() {
    // Sweep the exhaustive ALL set so a new signal can't silently ship without a
    // label/impact/weight.
    for signal in DegradedSignal::ALL {
        assert!(!signal.slug().is_empty(), "{signal:?} has a slug");
        assert!(signal.slug().is_ascii(), "{signal:?} slug is ASCII");
        assert!(
            !signal.impact().is_empty(),
            "{signal:?} has an impact clause"
        );
        assert!(
            signal.confidence() > 0,
            "{signal:?} has a positive confidence"
        );
    }
}

#[test]
fn fresh_engine_is_enabled_and_not_dismissed() {
    let engine = DegradedModeSuggestor::default();
    assert!(engine.is_enabled());
    assert!(!engine.is_dismissed());
    assert!(!engine.is_quiet(), "a fresh, enabled engine has work to do");
    assert!(!engine.is_showing());
}

#[test]
fn banner_does_not_flash_before_the_settle_delay() {
    let engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    // The first eligible frame stamps the clock but paints nothing.
    assert!(!engine.active(suggestion.as_ref(), base, false));
    assert!(!engine.is_showing());
    // Just shy of the settle window: still nothing.
    let almost = base + SUGGEST_SETTLE - Duration::from_millis(1);
    assert!(!engine.active(suggestion.as_ref(), almost, false));
}

#[test]
fn banner_paints_after_the_settle_delay() {
    let engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    assert!(!engine.active(suggestion.as_ref(), base, false));
    assert!(
        engine.active(suggestion.as_ref(), settled(base), false),
        "the settled suggestion paints",
    );
    assert!(engine.is_showing());
}

#[test]
fn a_recovered_terminal_resets_the_settle_clock() {
    let engine = DegradedModeSuggestor::default();
    let degraded = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    // Arm the settle clock while degraded.
    assert!(!engine.active(degraded.as_ref(), base, false));
    // The terminal recovers (no suggestion this frame): the clock resets and nothing
    // shows.
    assert!(!engine.active(None, base + Duration::from_millis(100), false));
    assert!(!engine.is_showing());
    // Re-degrade: the settle window starts FRESH, so a frame just past the original
    // window does NOT immediately paint.
    let re_armed = base + Duration::from_millis(200);
    assert!(!engine.active(degraded.as_ref(), re_armed, false));
    // Only after a fresh full settle from re_armed does it paint.
    assert!(engine.active(degraded.as_ref(), settled(re_armed), false));
}

#[test]
fn suppression_hides_the_banner_without_burning_its_showing() {
    let engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    // Settle the clock.
    assert!(!engine.active(suggestion.as_ref(), base, false));
    // A modal owns the surface: nothing paints, but the engine is NOT dismissed, so
    // it reappears once the modal closes.
    assert!(!engine.active(suggestion.as_ref(), settled(base), true));
    assert!(!engine.is_showing());
    assert!(!engine.is_dismissed());
    // Modal closes: the banner paints again (the settle clock was preserved).
    assert!(engine.active(suggestion.as_ref(), settled(base), false));
}

#[test]
fn dismiss_latches_for_the_session() {
    let mut engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    assert!(!engine.active(suggestion.as_ref(), base, false));
    assert!(engine.active(suggestion.as_ref(), settled(base), false));
    // Dismiss it.
    assert!(engine.dismiss(), "dismiss retires a pending suggestion");
    assert!(engine.is_dismissed());
    assert!(engine.is_quiet());
    // It never paints again, even though the terminal is still degraded.
    assert!(!engine.active(suggestion.as_ref(), settled(base), false));
    // A second dismiss is an idempotent no-op (nothing pending to retire).
    assert!(!engine.dismiss());
}

#[test]
fn accept_latches_for_the_session() {
    let mut engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    assert!(!engine.active(suggestion.as_ref(), base, false));
    assert!(engine.active(suggestion.as_ref(), settled(base), false));
    assert!(engine.accept(), "accept retires a pending suggestion");
    assert!(engine.is_dismissed());
    assert!(engine.is_quiet());
    assert!(!engine.active(suggestion.as_ref(), settled(base), false));
    assert!(!engine.accept(), "a second accept is an idempotent no-op");
}

#[test]
fn disabled_engine_never_shows_and_is_quiet() {
    let mut engine = DegradedModeSuggestor::default();
    engine.disable();
    assert!(!engine.is_enabled());
    assert!(engine.is_quiet());
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    assert!(
        !engine.active(suggestion.as_ref(), settled(base), false),
        "a disabled engine paints nothing even on a degraded terminal",
    );
}

#[test]
fn reveal_pending_does_not_tick_from_cold_then_keeps_the_settle_alive() {
    let engine = DegradedModeSuggestor::default();
    let suggestion = detect_capable(caps(TerminalKind::MacosVscode), ROOMY_COLS, ROOMY_ROWS);
    let base = std::time::Instant::now();
    // Before any render stamps the clock, reveal_pending is false: an idle,
    // never-rendered session schedules NO tick of its own (zero idle cost).
    assert!(
        !engine.reveal_pending(suggestion.as_ref(), base, false),
        "no tick is scheduled from cold",
    );
    // The first render (active) stamps the clock…
    assert!(!engine.active(suggestion.as_ref(), base, false));
    // …after which reveal_pending keeps exactly one wake-up alive through the settle
    // window.
    assert!(engine.reveal_pending(suggestion.as_ref(), base, false));
    // Once the banner is showing, a tick is still requested (so a later erase/redraw
    // is possible), but the moment it is dismissed the engine goes quiet.
    assert!(engine.active(suggestion.as_ref(), settled(base), false));
    let mut engine = engine;
    engine.dismiss();
    assert!(
        !engine.reveal_pending(suggestion.as_ref(), settled(base), false),
        "a dismissed engine schedules no further tick",
    );
}

#[test]
fn reveal_pending_is_false_when_not_degraded() {
    let engine = DegradedModeSuggestor::default();
    let base = std::time::Instant::now();
    assert!(
        !engine.reveal_pending(None, base, false),
        "a non-degraded terminal schedules no tick",
    );
}
