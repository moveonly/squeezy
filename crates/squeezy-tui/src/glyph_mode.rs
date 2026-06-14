//! Minimal Glyph Mode (§12.7.6).
//!
//! A low-fidelity glyph mode that swaps Squeezy's decorative / wide Unicode
//! *chrome* for ASCII-safe equivalents on terminals that cannot render the full
//! set cleanly: legacy Windows consoles, bare Linux/remote sessions, narrow
//! fonts, and accessibility setups. Only Squeezy *chrome* changes; user/tool
//! transcript content is never rewritten (the spec scopes this strictly to
//! chrome).
//!
//! ## Wired consumers (so far)
//!
//! The token table below enumerates the full chrome vocabulary the §12.7.6 spec
//! calls out, but only a subset is *routed through it* today. As of this commit
//! the mode actually changes:
//!
//! * the **terminal / OS title bar** (the working spinner + notification dot,
//!   via [`GlyphTokens::downgrade`]),
//! * the **main transcript scrollbar** (thumb / track),
//! * the **transcript rail** separator,
//! * the **shared modal surface** border (via [`GlyphMode::border_set`] — every
//!   centered overlay block routes through it),
//! * the **subagent-lane disclosure** marker (fold collapsed / expanded), and
//! * the editor overlay's own **live preview** strip.
//!
//! The remaining named tokens — status dots, queue bullets, drag handles, and
//! search markers — are a planned follow-up: they are defined and previewed here
//! but not yet spliced into their paint sites, so toggling the mode does not move
//! them yet.
//!
//! ## Three sets, not a switch
//!
//! Per the §12.7.6 steps this is a `GlyphSet` of *tokens* (borders, rails,
//! folds, spinners, markers, drag handles, scrollbars, status, queue,
//! expand/collapse, search) with three fidelity levels:
//!
//! * [`GlyphMode::Unicode`] — the full repertoire (rounded borders, braille
//!   spinner, block scrollbar, triangle disclosure). The default on capable
//!   terminals.
//! * [`GlyphMode::Compact`] — keeps single-cell Unicode that is broadly safe
//!   (arrows, middots, light box-drawing) but drops the *wide* / decorative
//!   glyphs ([`crate::is_wide_rendered_glyph`] — the moon/spinner family that
//!   xterm.js inflates to two cells) for narrow single-cell stand-ins. A middle
//!   ground for terminals whose only problem is wide-glyph cell width.
//! * [`GlyphMode::Ascii`] — pure 7-bit ASCII (`+-|` borders, `>`/`v` markers,
//!   `#`/`|` scrollbar, `-\|/` spinner). The guaranteed-renderable floor.
//!
//! ## Model, not chrome
//!
//! Like its §12.7 peers ([`crate::terminal_profile`], [`crate::theme_editor`])
//! this file owns only the *pure* model — the mode enum, the resolved
//! [`GlyphTokens`] table, the config round-trip, and the interactive editor
//! cursor — so every resolution / navigation / downgrade rule is unit-testable
//! without standing up a `TuiApp` or a terminal. `lib.rs` owns the side effects:
//! the keybinding, the open/close flag, the per-frame render call through the
//! single fullscreen `render()`, and the persist-to-config commit.
//!
//! ## Bounds & idle cost
//!
//! The token table is a compile-time match; the resolved mode is one enum tag;
//! the editor cursor is one `usize`. The overlay is closed by default (a single
//! `Option` on `TuiApp`) and at rest paints nothing and schedules no redraw, so
//! an idle session pays one enum-tag check and nothing more.

#![cfg_attr(not(unix), allow(dead_code))]

use crate::is_wide_rendered_glyph;

/// The fidelity of the glyph repertoire Squeezy chrome draws with. Ordered
/// most-capable first, which is also the editor's cycle order and the
/// exhaustive set the tests sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum GlyphMode {
    /// Full Unicode chrome: rounded borders, braille spinner, block scrollbar,
    /// triangle disclosure markers. The default on a capable terminal.
    Unicode,
    /// Single-cell-safe Unicode: keeps broadly-portable single-cell glyphs
    /// (arrows, middots, light box-drawing) but drops the *wide* decorative
    /// glyphs the [`is_wide_rendered_glyph`] family inflates for narrow
    /// stand-ins. For terminals whose only quirk is wide-glyph cell width.
    Compact,
    /// Pure 7-bit ASCII chrome: `+-|` borders, `>`/`v` markers, `#`/`|`
    /// scrollbar, the classic `-\|/` spinner. The guaranteed-renderable floor.
    Ascii,
}

impl GlyphMode {
    /// Every mode, most-capable first — the editor's cycle order and the
    /// exhaustive set the tests sweep.
    pub(crate) const ALL: [GlyphMode; 3] =
        [GlyphMode::Unicode, GlyphMode::Compact, GlyphMode::Ascii];

    /// The resting default: full Unicode chrome. A terminal that cannot render
    /// it opts down via the toggle (and persists the choice).
    pub(crate) const DEFAULT: GlyphMode = GlyphMode::Unicode;

    /// The fixed, hand-audited slug shown in the editor and persisted to config.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            GlyphMode::Unicode => "unicode",
            GlyphMode::Compact => "compact",
            GlyphMode::Ascii => "ascii",
        }
    }

    /// Friendly one-word label for the editor row / status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            GlyphMode::Unicode => "Unicode",
            GlyphMode::Compact => "Compact",
            GlyphMode::Ascii => "ASCII",
        }
    }

    /// A one-line note on what the mode trades, shown beside the label.
    pub(crate) fn description(self) -> &'static str {
        match self {
            GlyphMode::Unicode => "full Unicode chrome (rounded borders, braille spinner)",
            GlyphMode::Compact => "single-cell-safe Unicode, no wide glyphs",
            GlyphMode::Ascii => "pure ASCII chrome for legacy/remote terminals",
        }
    }

    /// Parse a persisted / config slug back to a mode. Unknown slugs return
    /// `None` so a stale config silently falls back to the default rather than
    /// guessing.
    pub(crate) fn from_str(s: &str) -> Option<GlyphMode> {
        GlyphMode::ALL.iter().copied().find(|m| m.as_str() == s)
    }

    /// The next mode in the cycle (wraps), used by the editor's ←/→/Space and a
    /// click on a row.
    pub(crate) fn next(self) -> GlyphMode {
        let idx = GlyphMode::ALL.iter().position(|m| *m == self).unwrap_or(0);
        GlyphMode::ALL[(idx + 1) % GlyphMode::ALL.len()]
    }

    /// Index of this mode into [`GlyphMode::ALL`] — the row index the editor
    /// marks and a click targets.
    pub(crate) fn index(self) -> usize {
        GlyphMode::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }

    /// The resolved token table for this mode. The renderer reads the *wired*
    /// chrome glyphs (scrollbar thumb/track, transcript rail) from here; the rest
    /// of the table is a defined-but-not-yet-routed vocabulary (see the module
    /// docs for the current wired set).
    pub(crate) fn tokens(self) -> GlyphTokens {
        GlyphTokens::resolve(self)
    }

    /// The ratatui border [`Set`](ratatui::symbols::border::Set) a bordered
    /// surface (modal blocks, cards) should draw with for this mode. `Unicode`
    /// and `Compact` keep the rounded box-drawing border (both render single-cell
    /// box-drawing cleanly); `Ascii` swaps to the `+-|` token set so an explicit
    /// ASCII opt-in on a limited terminal never paints box-drawing tofu.
    pub(crate) fn border_set(self) -> ratatui::symbols::border::Set<'static> {
        match self {
            GlyphMode::Unicode | GlyphMode::Compact => ratatui::symbols::border::ROUNDED,
            GlyphMode::Ascii => {
                let t = self.tokens();
                ratatui::symbols::border::Set {
                    top_left: t.corner_top_left,
                    top_right: t.corner_top_left,
                    bottom_left: t.corner_top_left,
                    bottom_right: t.corner_bottom_right,
                    vertical_left: t.border_vertical,
                    vertical_right: t.border_vertical,
                    horizontal_top: t.border_horizontal,
                    horizontal_bottom: t.border_horizontal,
                }
            }
        }
    }
}

/// The resolved set of chrome glyph tokens for a [`GlyphMode`]. Every field is a
/// `&'static str` so a renderer can splice it into a `Line`/`Span` without
/// allocating, and so the set is a compile-time table. The fields enumerate the
/// chrome families the §12.7.6 spec calls out: borders, rails, folds, spinner,
/// markers, drag handle, scrollbar, status, queue, expand/collapse, search. Note
/// that only a subset is wired into real paint sites today (the scrollbar
/// thumb/track and the transcript rail's `border_vertical`); the others are
/// previewed in the editor but not yet routed (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlyphTokens {
    /// Horizontal box-drawing run (card borders, separators).
    pub(crate) border_horizontal: &'static str,
    /// Vertical box-drawing run (card borders, rails).
    pub(crate) border_vertical: &'static str,
    /// Top-left rounded/square corner.
    pub(crate) corner_top_left: &'static str,
    /// Bottom-right rounded/square corner (one representative; the renderer
    /// mirrors it for the other three).
    pub(crate) corner_bottom_right: &'static str,
    /// The transcript rail / gutter marker.
    pub(crate) rail_marker: &'static str,
    /// A collapsed fold's disclosure marker (points right / "more here").
    pub(crate) fold_collapsed: &'static str,
    /// An expanded fold's disclosure marker (points down / "open").
    pub(crate) fold_expanded: &'static str,
    /// The "drag me" handle on a reorderable row (queue item, etc.).
    pub(crate) drag_handle: &'static str,
    /// The scrollbar thumb (the filled, draggable part).
    pub(crate) scrollbar_thumb: &'static str,
    /// The scrollbar track (the empty channel behind the thumb).
    pub(crate) scrollbar_track: &'static str,
    /// The idle/notification status dot.
    pub(crate) status_dot: &'static str,
    /// A queued-item bullet in the queue strip / overlay.
    pub(crate) queue_bullet: &'static str,
    /// The expand affordance (`▶`/`>`), distinct from `fold_collapsed` for
    /// callers that want an arrow rather than a triangle.
    pub(crate) expand: &'static str,
    /// The collapse affordance (`▼`/`v`).
    pub(crate) collapse: &'static str,
    /// The search-match marker.
    pub(crate) search_marker: &'static str,
}

impl GlyphTokens {
    /// Resolve the token table for `mode`. A compile-time match; pure and total
    /// (every [`GlyphMode`] has an arm).
    pub(crate) fn resolve(mode: GlyphMode) -> GlyphTokens {
        match mode {
            GlyphMode::Unicode => GlyphTokens {
                border_horizontal: "\u{2500}",   // ─
                border_vertical: "\u{2502}",     // │
                corner_top_left: "\u{256d}",     // ╭
                corner_bottom_right: "\u{256f}", // ╯
                rail_marker: "\u{2502}",         // │
                fold_collapsed: "\u{25b8}",      // ▸
                fold_expanded: "\u{25be}",       // ▾
                drag_handle: "\u{2059}",         // ⁙ (six-dot, the grip-dots glyph)
                scrollbar_thumb: "\u{2588}",     // █
                scrollbar_track: "\u{2591}", // ░ (light shade — the main scrollbar's empty channel)
                status_dot: "\u{25cf}",      // ●
                queue_bullet: "\u{2022}",    // •
                expand: "\u{25b6}",          // ▶
                collapse: "\u{25bc}",        // ▼
                search_marker: "\u{203a}",   // ›
            },
            GlyphMode::Compact => GlyphTokens {
                // Compact keeps single-cell box-drawing (broadly portable) but
                // swaps the wide block scrollbar + decorative markers for narrow
                // single-cell stand-ins.
                border_horizontal: "\u{2500}",   // ─
                border_vertical: "\u{2502}",     // │
                corner_top_left: "\u{250c}", // ┌ (square; rounded corners render odd in some fonts)
                corner_bottom_right: "\u{2518}", // ┘
                rail_marker: "\u{2502}",     // │
                fold_collapsed: "\u{203a}",  // › (single-cell, no triangle)
                fold_expanded: "\u{2304}",   // ⌄ (down caret, single cell)
                drag_handle: "\u{2237}",     // ∷ (single-cell dots)
                scrollbar_thumb: "\u{2503}", // ┃ (heavy vertical, single cell, no block)
                scrollbar_track: "\u{2502}", // │
                status_dot: "\u{2022}",      // • (narrow, not the wide ●)
                queue_bullet: "\u{2023}",    // ‣
                expand: "\u{203a}",          // ›
                collapse: "\u{2304}",        // ⌄
                search_marker: "\u{203a}",   // ›
            },
            GlyphMode::Ascii => GlyphTokens {
                border_horizontal: "-",
                border_vertical: "|",
                corner_top_left: "+",
                corner_bottom_right: "+",
                rail_marker: "|",
                fold_collapsed: ">",
                fold_expanded: "v",
                drag_handle: ":",
                scrollbar_thumb: "#",
                scrollbar_track: "|",
                status_dot: "*",
                queue_bullet: "-",
                expand: ">",
                collapse: "v",
                search_marker: ">",
            },
        }
    }

    /// Every token, in a stable order, paired with a human label. The editor's
    /// preview rows iterate this so a new token is shown automatically, and the
    /// tests sweep it to prove no Ascii token leaks a non-ASCII glyph and no
    /// Compact token leaks a wide glyph.
    pub(crate) fn labelled(&self) -> [(&'static str, &'static str); 15] {
        [
            ("border", self.border_horizontal),
            ("vertical", self.border_vertical),
            ("corner", self.corner_top_left),
            ("corner2", self.corner_bottom_right),
            ("rail", self.rail_marker),
            ("fold +", self.fold_collapsed),
            ("fold -", self.fold_expanded),
            ("drag", self.drag_handle),
            ("scroll", self.scrollbar_thumb),
            ("track", self.scrollbar_track),
            ("status", self.status_dot),
            ("queue", self.queue_bullet),
            ("expand", self.expand),
            ("collapse", self.collapse),
            ("search", self.search_marker),
        ]
    }

    /// Downgrade an arbitrary chrome glyph to an ASCII-safe stand-in for the
    /// given `mode`. Used as the renderer's catch-all for chrome glyphs that do
    /// not map to a named token (a one-off spinner frame, a decorative dingbat):
    ///
    /// * [`GlyphMode::Unicode`] — return the glyph unchanged.
    /// * [`GlyphMode::Compact`] — only the *wide* glyphs
    ///   ([`is_wide_rendered_glyph`]) are replaced (with `*`), so single-cell
    ///   Unicode survives; everything else is unchanged.
    /// * [`GlyphMode::Ascii`] — any non-ASCII glyph is replaced; box-drawing maps
    ///   to its ASCII analogue, everything else to `*`.
    ///
    /// Plain ASCII and whitespace are always returned unchanged — the scope is
    /// chrome glyphs only, never letters/digits/user text.
    pub(crate) fn downgrade(mode: GlyphMode, glyph: char) -> char {
        if glyph.is_ascii() {
            return glyph;
        }
        match mode {
            GlyphMode::Unicode => glyph,
            GlyphMode::Compact => {
                if is_wide_rendered_glyph(glyph) {
                    '*'
                } else {
                    glyph
                }
            }
            GlyphMode::Ascii => ascii_analogue(glyph),
        }
    }
}

/// Map a non-ASCII chrome glyph to its closest 7-bit ASCII analogue. Box-drawing
/// runs map to `-`/`|`/`+`, vertical bars to `|`, block fills to `#`, and every
/// other decorative glyph to `*`. Total over `char`; only ever called on
/// non-ASCII input by [`GlyphTokens::downgrade`].
fn ascii_analogue(glyph: char) -> char {
    let cp = glyph as u32;
    match cp {
        // Braille patterns (U+2800..U+28FF) are the working/title spinner family
        // (the `⠋⠙⠹…` frames). They have no single static ASCII analogue — a
        // frozen `*` would kill the animation — so cycle them through the classic
        // `-\|/` spinner. The phase is the codepoint's low two bits: consecutive
        // frames have different low bits, so the ASCII spinner keeps rotating
        // instead of freezing on one glyph.
        0x2800..=0x28ff => {
            const SPINNER: [char; 4] = ['-', '\\', '|', '/'];
            SPINNER[(cp & 0b11) as usize]
        }
        // Box drawing (U+2500..U+257F): horizontals/verticals/corners/junctions.
        0x2500 | 0x2501 | 0x2504 | 0x2505 | 0x2508 | 0x2509 | 0x254c | 0x254d => '-',
        0x2502 | 0x2503 | 0x2506 | 0x2507 | 0x250a | 0x250b | 0x254e | 0x254f => '|',
        0x250c..=0x254b | 0x2550..=0x257f => '+',
        // Block elements (U+2580..U+259F) + the shade blocks → filled marker.
        0x2580..=0x259f => '#',
        // Geometric shapes — disclosure triangles/arrows → arrow analogues.
        0x25b6 | 0x25b8 | 0x25b9 | 0x25ba | 0x25bb => '>',
        0x25bc | 0x25be | 0x25bf => 'v',
        0x25c0 | 0x25c2 | 0x25c3 | 0x25c4 | 0x25c5 => '<',
        0x25b2..=0x25b5 => '^',
        // Arrows (U+2190..U+21FF).
        0x2192 | 0x2794 | 0x279c => '>',
        0x2190 => '<',
        0x2191 => '^',
        0x2193 => 'v',
        // Single/double angle quotation marks used as markers.
        0x2039 | 0x203a | 0x00ab | 0x00bb => '>',
        // Bullets / dots / middots.
        0x2022 | 0x2023 | 0x25cf | 0x25cb | 0x00b7 | 0x2219 | 0x2024 => '*',
        // Ellipsis → '.'.
        0x2026 => '.',
        // Anything else decorative → a neutral asterisk.
        _ => '*',
    }
}

/// The pure interactive Minimal Glyph Mode editor model (§12.7.6). Holds the
/// working mode the user is shaping plus a cursor into [`GlyphMode::ALL`]. All
/// persistence side effects live in `lib.rs`; this struct is the terminal-free,
/// fully unit-testable core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlyphModeEditor {
    /// The mode that was active when the overlay opened — what a reset restores
    /// and the baseline the "(changed)" marker compares against.
    opened_with: GlyphMode,
    /// The working mode the user is shaping — what a commit persists and what a
    /// live preview would apply.
    working: GlyphMode,
    /// Cursor into [`GlyphMode::ALL`]. Always in bounds (the constructor and
    /// movers clamp it), so [`Self::focused_mode`] never panics.
    cursor: usize,
}

impl GlyphModeEditor {
    /// Open the editor seeded with the currently-active `mode`. The cursor lands
    /// on the active mode so ←/→ adjust from where the user is.
    pub(crate) fn new(mode: GlyphMode) -> Self {
        Self {
            opened_with: mode,
            working: mode,
            cursor: mode.index(),
        }
    }

    /// The working mode — the live value, what a commit persists.
    pub(crate) fn working(&self) -> GlyphMode {
        self.working
    }

    /// The mode active when the overlay opened — what a reset restores.
    pub(crate) fn opened_with(&self) -> GlyphMode {
        self.opened_with
    }

    /// The focused row's mode. Always valid: [`GlyphMode::ALL`] is non-empty and
    /// `cursor` is clamped on every move.
    pub(crate) fn focused_mode(&self) -> GlyphMode {
        GlyphMode::ALL[self.cursor.min(GlyphMode::ALL.len() - 1)]
    }

    /// Index of the focused row into [`GlyphMode::ALL`].
    pub(crate) fn cursor(&self) -> usize {
        self.cursor.min(GlyphMode::ALL.len() - 1)
    }

    /// Whether the working mode differs from the one the overlay opened with (an
    /// unsaved change is in flight). Drives the "(changed)" marker and whether a
    /// reset is meaningful.
    pub(crate) fn is_changed(&self) -> bool {
        self.working != self.opened_with
    }

    /// Move the row focus up one (clamped at the top). Returns `true` when the
    /// focus moved. Selecting a row also makes it the working mode so the preview
    /// follows the cursor.
    pub(crate) fn focus_prev(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.working = self.focused_mode();
        true
    }

    /// Move the row focus down one (clamped at the bottom). Returns `true` when
    /// the focus moved. Selecting a row also makes it the working mode.
    pub(crate) fn focus_next(&mut self) -> bool {
        if self.cursor + 1 >= GlyphMode::ALL.len() {
            return false;
        }
        self.cursor += 1;
        self.working = self.focused_mode();
        true
    }

    /// Focus a row directly by its [`GlyphMode::ALL`] index (the mouse twin of
    /// ↑/↓ over a row), making it the working mode. Out-of-range indices are
    /// ignored. Returns `true` when the focus actually moved.
    pub(crate) fn focus_row(&mut self, index: usize) -> bool {
        if index >= GlyphMode::ALL.len() || index == self.cursor {
            return false;
        }
        self.cursor = index;
        self.working = self.focused_mode();
        true
    }

    /// Cycle the working mode forward (the keyboard ←/→/Space), moving the cursor
    /// to track it. Returns the new working mode so the caller can apply a live
    /// preview.
    pub(crate) fn cycle(&mut self) -> GlyphMode {
        self.working = self.working.next();
        self.cursor = self.working.index();
        self.working
    }

    /// Reset the working mode to the one the overlay opened with (the keyboard
    /// `r`/Delete). Returns the restored mode.
    pub(crate) fn reset(&mut self) -> GlyphMode {
        self.working = self.opened_with;
        self.cursor = self.working.index();
        self.working
    }
}

#[cfg(test)]
#[path = "glyph_mode_tests.rs"]
mod tests;
