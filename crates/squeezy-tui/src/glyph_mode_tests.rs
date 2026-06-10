//! Unit tests for the pure Minimal Glyph Mode model (§12.7.6).
//!
//! These cover the mode enum (slug round-trip, cycle, index), the resolved token
//! table (no ASCII token leaks a non-ASCII glyph; no Compact token leaks a *wide*
//! glyph), the `downgrade` chrome catch-all, and the interactive editor
//! cursor/cycle/reset rules in isolation — no terminal, no `TuiApp`. The overlay's
//! behaviour through the real `render()` + key/mouse dispatch and the persist
//! round-trip are covered by the capture-sink suite in `lib_tests.rs`.

use super::*;
use crate::is_wide_rendered_glyph;

// ---------------------------------------------------------------------------
// GlyphMode enum
// ---------------------------------------------------------------------------

#[test]
fn all_holds_every_mode_most_capable_first() {
    assert_eq!(
        GlyphMode::ALL,
        [GlyphMode::Unicode, GlyphMode::Compact, GlyphMode::Ascii]
    );
    // The default is the most-capable (full Unicode) mode.
    assert_eq!(GlyphMode::DEFAULT, GlyphMode::Unicode);
}

#[test]
fn slug_round_trips_for_every_mode() {
    for mode in GlyphMode::ALL {
        assert_eq!(
            GlyphMode::from_str(mode.as_str()),
            Some(mode),
            "slug {:?} must round-trip",
            mode.as_str()
        );
    }
}

#[test]
fn from_str_rejects_unknown_slug() {
    assert_eq!(GlyphMode::from_str("fancy"), None);
    assert_eq!(GlyphMode::from_str(""), None);
    assert_eq!(GlyphMode::from_str("UNICODE"), None, "case-sensitive");
}

#[test]
fn next_cycles_through_all_modes_and_wraps() {
    assert_eq!(GlyphMode::Unicode.next(), GlyphMode::Compact);
    assert_eq!(GlyphMode::Compact.next(), GlyphMode::Ascii);
    assert_eq!(GlyphMode::Ascii.next(), GlyphMode::Unicode);
    // Three nexts return to the start.
    let mut m = GlyphMode::Unicode;
    for _ in 0..GlyphMode::ALL.len() {
        m = m.next();
    }
    assert_eq!(m, GlyphMode::Unicode);
}

#[test]
fn index_matches_position_in_all() {
    for (i, mode) in GlyphMode::ALL.iter().enumerate() {
        assert_eq!(mode.index(), i);
    }
}

#[test]
fn labels_and_descriptions_are_distinct_and_nonempty() {
    let mut labels = std::collections::HashSet::new();
    for mode in GlyphMode::ALL {
        assert!(!mode.label().is_empty());
        assert!(!mode.description().is_empty());
        assert!(labels.insert(mode.label()), "labels must be distinct");
    }
}

// ---------------------------------------------------------------------------
// GlyphTokens table
// ---------------------------------------------------------------------------

#[test]
fn ascii_tokens_are_pure_ascii() {
    let tokens = GlyphMode::Ascii.tokens();
    for (name, glyph) in tokens.labelled() {
        assert!(
            glyph.is_ascii(),
            "ASCII glyph mode token {name:?} = {glyph:?} must be pure ASCII"
        );
        assert!(!glyph.is_empty(), "token {name:?} must not be empty");
    }
}

#[test]
fn compact_tokens_carry_no_wide_glyphs() {
    // Compact's whole point: keep single-cell Unicode but drop the *wide* glyphs
    // xterm.js inflates (the `is_wide_rendered_glyph` family).
    let tokens = GlyphMode::Compact.tokens();
    for (name, glyph) in tokens.labelled() {
        for c in glyph.chars() {
            assert!(
                !is_wide_rendered_glyph(c),
                "Compact token {name:?} glyph {c:?} must not be a wide-rendered glyph"
            );
        }
    }
}

#[test]
fn unicode_tokens_are_nonempty() {
    let tokens = GlyphMode::Unicode.tokens();
    for (name, glyph) in tokens.labelled() {
        assert!(
            !glyph.is_empty(),
            "Unicode token {name:?} must not be empty"
        );
    }
}

#[test]
fn labelled_covers_fifteen_distinct_token_families() {
    // The spec enumerates borders, rails, folds, spinners, markers, drag handles,
    // scrollbars, status, queue, expand/collapse, search — the table must expose a
    // bounded, fixed-size set so a new token is shown in the editor automatically.
    let tokens = GlyphMode::Unicode.tokens();
    assert_eq!(tokens.labelled().len(), 15);
}

#[test]
fn resolve_is_total_and_matches_tokens_accessor() {
    for mode in GlyphMode::ALL {
        assert_eq!(GlyphTokens::resolve(mode), mode.tokens());
    }
}

// ---------------------------------------------------------------------------
// downgrade
// ---------------------------------------------------------------------------

#[test]
fn downgrade_leaves_plain_ascii_and_user_text_untouched_in_every_mode() {
    for mode in GlyphMode::ALL {
        for c in ['a', 'Z', '7', ' ', '!', '/', '\t'] {
            assert_eq!(
                GlyphTokens::downgrade(mode, c),
                c,
                "plain char {c:?} must be untouched in {mode:?}"
            );
        }
    }
}

#[test]
fn downgrade_unicode_mode_is_identity_for_chrome() {
    // Unicode mode never rewrites anything.
    for c in ['\u{2588}', '\u{25b6}', '\u{2726}', '\u{2502}', '\u{4e2d}'] {
        assert_eq!(GlyphTokens::downgrade(GlyphMode::Unicode, c), c);
    }
}

#[test]
fn downgrade_compact_only_replaces_wide_glyphs() {
    // A wide spinner star (✦, U+2726) is in the wide family → replaced.
    assert!(is_wide_rendered_glyph('\u{2726}'));
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Compact, '\u{2726}'), '*');
    // A narrow single-cell box-drawing glyph survives Compact untouched.
    assert!(!is_wide_rendered_glyph('\u{2502}'));
    assert_eq!(
        GlyphTokens::downgrade(GlyphMode::Compact, '\u{2502}'),
        '\u{2502}'
    );
}

#[test]
fn downgrade_ascii_replaces_every_non_ascii_chrome_glyph() {
    // Box drawing → ASCII analogues.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{2500}'), '-'); // ─
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{2502}'), '|'); // │
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{256d}'), '+'); // ╭
    // Block fill → '#'.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{2588}'), '#'); // █
    // Disclosure triangles → arrows.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{25b6}'), '>'); // ▶
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{25bc}'), 'v'); // ▼
    // Dots/bullets → '*'.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{25cf}'), '*'); // ●
    // Ellipsis → '.'.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{2026}'), '.'); // …
    // An arbitrary decorative dingbat → '*'.
    assert_eq!(GlyphTokens::downgrade(GlyphMode::Ascii, '\u{2736}'), '*'); // ✶
}

#[test]
fn downgrade_ascii_output_is_always_ascii_for_chrome() {
    // Sweep every Unicode chrome token across the block Squeezy paints from: the
    // ASCII downgrade must never produce a non-ASCII char.
    for c in 0x2500u32..=0x29FFu32 {
        if let Some(ch) = char::from_u32(c) {
            let out = GlyphTokens::downgrade(GlyphMode::Ascii, ch);
            assert!(
                out.is_ascii(),
                "ASCII downgrade of {ch:?} (U+{c:04X}) produced non-ASCII {out:?}"
            );
        }
    }
}

#[test]
fn every_ascii_token_survives_its_own_downgrade() {
    // A token already in the ASCII set must pass through downgrade unchanged.
    let tokens = GlyphMode::Ascii.tokens();
    for (name, glyph) in tokens.labelled() {
        for c in glyph.chars() {
            assert_eq!(
                GlyphTokens::downgrade(GlyphMode::Ascii, c),
                c,
                "ASCII token {name:?} char {c:?} must be downgrade-stable"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// GlyphModeEditor
// ---------------------------------------------------------------------------

#[test]
fn editor_opens_seeded_on_the_active_mode() {
    let editor = GlyphModeEditor::new(GlyphMode::Compact);
    assert_eq!(editor.working(), GlyphMode::Compact);
    assert_eq!(editor.opened_with(), GlyphMode::Compact);
    assert_eq!(editor.focused_mode(), GlyphMode::Compact);
    assert_eq!(editor.cursor(), GlyphMode::Compact.index());
    assert!(!editor.is_changed());
}

#[test]
fn editor_focus_moves_clamp_at_the_ends() {
    let mut editor = GlyphModeEditor::new(GlyphMode::Unicode);
    // At the top: prev is a no-op.
    assert!(!editor.focus_prev());
    assert_eq!(editor.focused_mode(), GlyphMode::Unicode);
    // Walk down to the bottom.
    assert!(editor.focus_next());
    assert_eq!(editor.focused_mode(), GlyphMode::Compact);
    assert!(editor.focus_next());
    assert_eq!(editor.focused_mode(), GlyphMode::Ascii);
    // At the bottom: next is a no-op.
    assert!(!editor.focus_next());
    assert_eq!(editor.focused_mode(), GlyphMode::Ascii);
}

#[test]
fn editor_focus_makes_the_row_the_working_mode() {
    let mut editor = GlyphModeEditor::new(GlyphMode::Unicode);
    editor.focus_next();
    assert_eq!(editor.working(), GlyphMode::Compact);
    assert!(editor.is_changed());
}

#[test]
fn editor_focus_row_selects_by_index_and_ignores_out_of_range() {
    let mut editor = GlyphModeEditor::new(GlyphMode::Unicode);
    assert!(editor.focus_row(2));
    assert_eq!(editor.working(), GlyphMode::Ascii);
    // Re-selecting the same row is a no-op.
    assert!(!editor.focus_row(2));
    // Out of range is ignored.
    assert!(!editor.focus_row(99));
    assert_eq!(editor.working(), GlyphMode::Ascii);
}

#[test]
fn editor_cycle_advances_working_and_tracks_cursor() {
    let mut editor = GlyphModeEditor::new(GlyphMode::Unicode);
    assert_eq!(editor.cycle(), GlyphMode::Compact);
    assert_eq!(editor.cursor(), GlyphMode::Compact.index());
    assert_eq!(editor.cycle(), GlyphMode::Ascii);
    assert_eq!(editor.cycle(), GlyphMode::Unicode, "cycle wraps");
    // Back to the start: no longer changed.
    assert!(!editor.is_changed());
}

#[test]
fn editor_reset_restores_the_mode_it_opened_with() {
    let mut editor = GlyphModeEditor::new(GlyphMode::Unicode);
    editor.cycle();
    editor.cycle();
    assert!(editor.is_changed());
    assert_eq!(editor.reset(), GlyphMode::Unicode);
    assert_eq!(editor.working(), GlyphMode::Unicode);
    assert_eq!(editor.cursor(), GlyphMode::Unicode.index());
    assert!(!editor.is_changed());
}
