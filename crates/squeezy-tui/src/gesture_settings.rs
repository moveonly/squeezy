//! Gesture Settings (§12.7.5).
//!
//! A settings surface for the mouse / trackpad gesture behaviour the §12.7.5
//! spec calls out: wheel/trackpad scroll speed, Shift-wheel horizontal pan,
//! hover dwell (the delay before hover affordances reveal), the double-click
//! action, and drag-select. The spec is explicit that *every gesture must have a
//! keyboard equivalent* because mouse reporting may be disabled by policy or
//! terminal capability — so this overlay is fully keyboard-drivable and the
//! values it tunes are read by the same recognizer the keyboard path already
//! exercises.
//!
//! ## Model, not chrome
//!
//! Like its peer leaf settings modules ([`crate::terminal_profile`],
//! [`crate::theme_editor`], [`crate::keybinding_editor`]) this file owns only the
//! *pure* model — the tunable [`GestureSettings`], the editor cursor, the
//! per-field cycle/nudge rules, and the config (de)serialization — so every
//! navigation / adjust / reset / persist rule is unit-testable without standing
//! up a `TuiApp` or a terminal. `lib.rs` owns the side effects: the keybinding,
//! the open/close flag, the per-frame render call through the single fullscreen
//! `render()`, and the persist-to-config commit.
//!
//! ## Reuse of the interaction timing constants
//!
//! The dwell and double-click defaults are *single-sourced* from
//! [`crate::interaction`]: [`GestureSettings::DEFAULT`] seeds `hover_dwell_ms`
//! from [`crate::interaction::HOVER_INTENT_MS`] and the double-click window note
//! from [`crate::interaction::MULTI_CLICK_MS`], so the settings surface can never
//! drift from the recognizer's actual timing. The dwell is clamped to a bounded
//! range; the recognizer reads `hover_dwell_ms` as its intent delay.
//!
//! ## Bounds & idle cost
//!
//! [`GestureSettings`] is five small `Copy` fields; the editor cursor is one
//! `usize`. The overlay is closed by default (a single `Option` on `TuiApp`) and
//! at rest paints nothing and schedules no redraw, so an idle session pays one
//! enum-tag check and nothing more. There is no background timer and no
//! per-frame probe.
//!
//! ## Windows clock trap
//!
//! `hover_dwell_ms` is stored as a plain `u16`, never as a back-dated
//! `Instant`. The one place a test needs to simulate "dwell elapsed" derives the
//! synthetic earlier instant with [`std::time::Instant::checked_sub`] and a safe
//! fallback (see `gesture_settings_tests.rs`), never a bare `Instant::now() -
//! Duration`, which panics on a fresh Windows CI runner whose monotonic clock is
//! younger than the offset.

#![cfg_attr(not(unix), allow(dead_code))]

use crate::interaction::{HOVER_INTENT_MS, MULTI_CLICK_MS};

/// What a double-click on a transcript card does. The spec calls out the
/// double-click action as a tunable; the recognizer already produces a
/// `DoubleClick` gesture (within [`MULTI_CLICK_MS`]), and this picks which
/// handler that gesture routes to. Listed in the editor's cycle order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DoubleClickAction {
    /// Expand the double-clicked card if it is collapsed (the default — the same
    /// `ExpandEntry` action the card-affordance path already dispatches).
    Expand,
    /// Open the double-clicked entry in the Ctrl+T detail overlay.
    OpenDetail,
    /// Do nothing on a double-click (treat it as two single clicks).
    None,
}

impl DoubleClickAction {
    /// Every action, in the editor's cycle order and the exhaustive set the
    /// tests sweep.
    pub(crate) const ALL: [DoubleClickAction; 3] = [
        DoubleClickAction::Expand,
        DoubleClickAction::OpenDetail,
        DoubleClickAction::None,
    ];

    /// The fixed, hand-audited label persisted to config.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DoubleClickAction::Expand => "expand",
            DoubleClickAction::OpenDetail => "open_detail",
            DoubleClickAction::None => "none",
        }
    }

    /// Friendly label shown in the editor row.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DoubleClickAction::Expand => "Expand card",
            DoubleClickAction::OpenDetail => "Open in detail",
            DoubleClickAction::None => "Do nothing",
        }
    }

    /// Parse a persisted / config label back to an action. Unknown labels return
    /// `None` so a stale config silently falls back to the default rather than
    /// guessing.
    pub(crate) fn from_str(s: &str) -> Option<DoubleClickAction> {
        DoubleClickAction::ALL
            .iter()
            .copied()
            .find(|a| a.as_str() == s)
    }

    /// The next action in the cycle (wraps), used by the editor's ←/→/Space and a
    /// click on the row.
    pub(crate) fn next(self) -> DoubleClickAction {
        let idx = DoubleClickAction::ALL
            .iter()
            .position(|a| *a == self)
            .unwrap_or(0);
        DoubleClickAction::ALL[(idx + 1) % DoubleClickAction::ALL.len()]
    }
}

/// The tunable gesture behaviour (§12.7.5). Five small `Copy` fields, each with a
/// bounded range so a persisted value can never push the recognizer into an
/// absurd state. The renderer / input loop would consult these to decide wheel
/// step, pan axis, hover dwell, double-click routing, and whether a drag selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GestureSettings {
    /// Lines scrolled per wheel notch / trackpad tick (scroll speed). Clamped to
    /// [`GestureSettings::SCROLL_MIN`]..=[`GestureSettings::SCROLL_MAX`].
    pub(crate) scroll_lines: u8,
    /// Whether Shift+wheel pans the view horizontally (vs. the default vertical
    /// scroll). On by default; horizontal pan is only useful for wide blocks.
    pub(crate) shift_wheel_pan: bool,
    /// Hover dwell in milliseconds — how long the pointer must rest on a target
    /// before hover affordances reveal. Seeded from
    /// [`crate::interaction::HOVER_INTENT_MS`]; clamped to
    /// [`GestureSettings::DWELL_MIN`]..=[`GestureSettings::DWELL_MAX`].
    pub(crate) hover_dwell_ms: u16,
    /// What a double-click on a card does.
    pub(crate) double_click: DoubleClickAction,
    /// Whether a left-drag over the transcript selects text. On by default; off
    /// for terminals where a drag is unreliable or reserved for the emulator.
    pub(crate) drag_select: bool,
}

impl GestureSettings {
    /// Minimum / maximum lines per wheel notch. One line keeps fine control; a
    /// high cap keeps a fast scroll from skipping whole screens unpredictably.
    pub(crate) const SCROLL_MIN: u8 = 1;
    pub(crate) const SCROLL_MAX: u8 = 10;
    /// Minimum / maximum hover dwell. The floor keeps the dwell perceptible (so a
    /// sweep across targets does not flicker affordances); the ceiling keeps it
    /// from feeling broken. A `0` floor defeated the stable-intent debounce
    /// entirely — the preview popped on essentially every pointer cell — so the
    /// floor is a small non-zero value.
    pub(crate) const DWELL_MIN: u16 = 40;
    pub(crate) const DWELL_MAX: u16 = 1000;
    /// The amount a single +/- nudge moves the dwell, so a few presses span the
    /// useful range without a press-per-millisecond grind.
    pub(crate) const DWELL_STEP: u16 = 25;

    /// The built-in defaults. The dwell is single-sourced from the recognizer's
    /// [`crate::interaction::HOVER_INTENT_MS`] so the settings surface and the
    /// live hover-intent delay can never disagree at rest. The scroll speed is a
    /// conservative 3 lines; pan and the double-click default match the
    /// card-affordance recognizer's existing behaviour (Shift-pan on for the
    /// no-wrap view, double-click expands, drag selects).
    pub(crate) const DEFAULT: GestureSettings = GestureSettings {
        scroll_lines: 3,
        shift_wheel_pan: true,
        // `HOVER_INTENT_MS` is a `u128` constant well under `u16::MAX`; the
        // `const` clamp keeps the seed honest even if the source constant ever
        // grows past the dwell ceiling.
        hover_dwell_ms: if HOVER_INTENT_MS > GestureSettings::DWELL_MAX as u128 {
            GestureSettings::DWELL_MAX
        } else {
            HOVER_INTENT_MS as u16
        },
        double_click: DoubleClickAction::Expand,
        drag_select: true,
    };

    /// The double-click window (ms) the recognizer uses, surfaced read-only in
    /// the editor so the user can see the timing their chosen double-click action
    /// fires within. Single-sourced from [`crate::interaction::MULTI_CLICK_MS`].
    pub(crate) const fn multi_click_window_ms() -> u128 {
        MULTI_CLICK_MS
    }

    /// Re-clamp every bounded field into range. Applied after a config load (a
    /// stale / hand-edited file could carry an out-of-range value) and after any
    /// nudge so the editor can never present an absurd value.
    pub(crate) fn clamped(self) -> GestureSettings {
        GestureSettings {
            scroll_lines: self.scroll_lines.clamp(Self::SCROLL_MIN, Self::SCROLL_MAX),
            hover_dwell_ms: self.hover_dwell_ms.clamp(Self::DWELL_MIN, Self::DWELL_MAX),
            ..self
        }
    }

    /// The fixed config representation as `(key, value-string)` pairs, in editor
    /// order. Single-sourced so the persist path and a round-trip test name the
    /// same keys; the inverse is [`Self::from_config_lookup`].
    pub(crate) fn as_config_pairs(self) -> [(&'static str, String); 5] {
        [
            (KEY_SCROLL_LINES, self.scroll_lines.to_string()),
            (KEY_SHIFT_WHEEL_PAN, self.shift_wheel_pan.to_string()),
            (KEY_HOVER_DWELL_MS, self.hover_dwell_ms.to_string()),
            (KEY_DOUBLE_CLICK, self.double_click.as_str().to_string()),
            (KEY_DRAG_SELECT, self.drag_select.to_string()),
        ]
    }

    /// Rebuild settings from a persisted config lookup, layered onto `fallback`
    /// (the built-in default) so a partial / stale `[tui.gestures]` table only
    /// overrides the fields it actually names and an unparsable value silently
    /// keeps the default. `get` reads a field string by its config key. Returns
    /// `None` when the table named no recognised field at all (so the caller can
    /// treat "no override" distinctly from "default override"). The result is
    /// clamped so an out-of-range file value is never honoured verbatim.
    pub(crate) fn from_config_lookup<F>(
        fallback: GestureSettings,
        get: F,
    ) -> Option<GestureSettings>
    where
        F: Fn(&str) -> Option<String>,
    {
        let scroll = get(KEY_SCROLL_LINES).and_then(|s| s.trim().parse::<u8>().ok());
        let pan = get(KEY_SHIFT_WHEEL_PAN).and_then(|s| parse_bool(&s));
        let dwell = get(KEY_HOVER_DWELL_MS).and_then(|s| s.trim().parse::<u16>().ok());
        let dbl = get(KEY_DOUBLE_CLICK).and_then(|s| DoubleClickAction::from_str(s.trim()));
        let drag = get(KEY_DRAG_SELECT).and_then(|s| parse_bool(&s));
        if scroll.is_none() && pan.is_none() && dwell.is_none() && dbl.is_none() && drag.is_none() {
            return None;
        }
        Some(
            GestureSettings {
                scroll_lines: scroll.unwrap_or(fallback.scroll_lines),
                shift_wheel_pan: pan.unwrap_or(fallback.shift_wheel_pan),
                hover_dwell_ms: dwell.unwrap_or(fallback.hover_dwell_ms),
                double_click: dbl.unwrap_or(fallback.double_click),
                drag_select: drag.unwrap_or(fallback.drag_select),
            }
            .clamped(),
        )
    }
}

/// Parse a permissive boolean: `true`/`false`, `on`/`off`, `1`/`0`, `yes`/`no`
/// (case-insensitive). A stray value returns `None` so the caller keeps the
/// default rather than guessing.
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Some(true),
        "false" | "off" | "0" | "no" => Some(false),
        _ => None,
    }
}

// Config keys — single-sourced `&'static str` so the persist path (which needs
// `&'static str` field keys for `SetTableEntry`) and the read path agree.
pub(crate) const KEY_SCROLL_LINES: &str = "scroll_lines";
pub(crate) const KEY_SHIFT_WHEEL_PAN: &str = "shift_wheel_pan";
pub(crate) const KEY_HOVER_DWELL_MS: &str = "hover_dwell_ms";
pub(crate) const KEY_DOUBLE_CLICK: &str = "double_click";
pub(crate) const KEY_DRAG_SELECT: &str = "drag_select";

/// The editable fields of [`GestureSettings`], in render/edit order. The editor
/// cursor steps between these; ←/→/Space (or a click on the row) adjusts the
/// focused field's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GestureField {
    ScrollLines,
    ShiftWheelPan,
    HoverDwellMs,
    DoubleClick,
    DragSelect,
}

impl GestureField {
    /// Every field, in render order — the editor's row order and the exhaustive
    /// set the tests sweep.
    pub(crate) const ALL: [GestureField; 5] = [
        GestureField::ScrollLines,
        GestureField::ShiftWheelPan,
        GestureField::HoverDwellMs,
        GestureField::DoubleClick,
        GestureField::DragSelect,
    ];

    /// The row label shown in the editor.
    pub(crate) fn label(self) -> &'static str {
        match self {
            GestureField::ScrollLines => "Scroll speed",
            GestureField::ShiftWheelPan => "Shift-wheel pan",
            GestureField::HoverDwellMs => "Hover dwell",
            GestureField::DoubleClick => "Double-click",
            GestureField::DragSelect => "Drag select",
        }
    }

    /// A one-line note on what the field controls, shown beside the value.
    pub(crate) fn description(self) -> &'static str {
        match self {
            GestureField::ScrollLines => "Lines per wheel/trackpad notch",
            GestureField::ShiftWheelPan => "Shift+wheel pans horizontally",
            GestureField::HoverDwellMs => "Delay before hover affordances reveal",
            GestureField::DoubleClick => "What a double-click on a card does",
            GestureField::DragSelect => "Left-drag selects transcript text",
        }
    }

    /// The current value of this field, as its display string.
    pub(crate) fn value_label(self, s: GestureSettings) -> String {
        match self {
            GestureField::ScrollLines => format!("{} lines", s.scroll_lines),
            GestureField::ShiftWheelPan => on_off(s.shift_wheel_pan).to_string(),
            GestureField::HoverDwellMs => format!("{} ms", s.hover_dwell_ms),
            GestureField::DoubleClick => s.double_click.label().to_string(),
            GestureField::DragSelect => on_off(s.drag_select).to_string(),
        }
    }

    /// Adjust this field's value on `s` by one step in `dir` (+1 forward, -1
    /// back), returning the updated settings. Numeric fields nudge within their
    /// clamp; toggles flip (direction-independent); the double-click action
    /// cycles. Used by the editor's ←/→/+/- and a click on the row (which always
    /// steps forward).
    pub(crate) fn adjust(self, s: GestureSettings, dir: i8) -> GestureSettings {
        let forward = dir >= 0;
        match self {
            GestureField::ScrollLines => {
                let next = if forward {
                    s.scroll_lines.saturating_add(1)
                } else {
                    s.scroll_lines.saturating_sub(1)
                };
                GestureSettings {
                    scroll_lines: next,
                    ..s
                }
                .clamped()
            }
            GestureField::HoverDwellMs => {
                let next = if forward {
                    s.hover_dwell_ms.saturating_add(GestureSettings::DWELL_STEP)
                } else {
                    s.hover_dwell_ms.saturating_sub(GestureSettings::DWELL_STEP)
                };
                GestureSettings {
                    hover_dwell_ms: next,
                    ..s
                }
                .clamped()
            }
            GestureField::ShiftWheelPan => GestureSettings {
                shift_wheel_pan: !s.shift_wheel_pan,
                ..s
            },
            GestureField::DragSelect => GestureSettings {
                drag_select: !s.drag_select,
                ..s
            },
            GestureField::DoubleClick => GestureSettings {
                double_click: s.double_click.next(),
                ..s
            },
        }
    }
}

/// "on" / "off" label for a boolean field.
fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// The pure interactive Gesture Settings editor model (§12.7.5). Holds the
/// built-in default (so a reset can restore it) and the working settings the user
/// is shaping. All persistence side effects live in `lib.rs`; this struct is the
/// terminal-free, fully unit-testable core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GestureSettingsEditor {
    /// The built-in default — what a reset restores.
    default: GestureSettings,
    /// The working settings the user is editing — what a commit persists.
    working: GestureSettings,
    /// Cursor into [`GestureField::ALL`]. Always in bounds (the constructor and
    /// movers clamp it), so [`Self::focused_field`] never panics.
    field: usize,
}

impl GestureSettingsEditor {
    /// Open the editor, seeding the working settings from any persisted
    /// `override_settings` (a manual save from a previous session) or, when none,
    /// the built-in default.
    pub(crate) fn new(override_settings: Option<GestureSettings>) -> Self {
        Self {
            default: GestureSettings::DEFAULT,
            working: override_settings
                .unwrap_or(GestureSettings::DEFAULT)
                .clamped(),
            field: 0,
        }
    }

    /// The built-in default settings.
    pub(crate) fn default_settings(&self) -> GestureSettings {
        self.default
    }

    /// The working settings — the live value, what a commit persists.
    pub(crate) fn working(&self) -> GestureSettings {
        self.working
    }

    /// The focused field. Always valid: [`GestureField::ALL`] is non-empty and
    /// `field` is clamped on every move.
    pub(crate) fn focused_field(&self) -> GestureField {
        GestureField::ALL[self.field.min(GestureField::ALL.len() - 1)]
    }

    /// Index of the focused field into [`GestureField::ALL`].
    pub(crate) fn field_index(&self) -> usize {
        self.field.min(GestureField::ALL.len() - 1)
    }

    /// Whether the working settings differ from the built-in default (a manual
    /// override is in effect). Drives the "overridden" marker and whether a reset
    /// is meaningful.
    pub(crate) fn is_overridden(&self) -> bool {
        self.working != self.default
    }

    /// Move the field focus up one row (clamped at the top). Returns `true` when
    /// the focus moved.
    pub(crate) fn focus_prev_field(&mut self) -> bool {
        if self.field == 0 {
            return false;
        }
        self.field -= 1;
        true
    }

    /// Move the field focus down one row (clamped at the bottom). Returns `true`
    /// when the focus moved.
    pub(crate) fn focus_next_field(&mut self) -> bool {
        if self.field + 1 >= GestureField::ALL.len() {
            return false;
        }
        self.field += 1;
        true
    }

    /// Focus a field directly by its [`GestureField::ALL`] index (the mouse twin
    /// of ↑/↓ over a field row). Out-of-range indices are ignored. Returns `true`
    /// when the focus actually moved.
    pub(crate) fn focus_field(&mut self, index: usize) -> bool {
        if index >= GestureField::ALL.len() || index == self.field {
            return false;
        }
        self.field = index;
        true
    }

    /// Adjust the focused field's value by one step in `dir` (+1 forward, -1
    /// back). Returns the updated working settings so the caller can apply a live
    /// preview.
    pub(crate) fn adjust_focused(&mut self, dir: i8) -> GestureSettings {
        self.working = self.focused_field().adjust(self.working, dir);
        self.working
    }

    /// Reset the working settings to the built-in default (the keyboard `r`/Delete
    /// and the "Reset" button). Returns the restored settings.
    pub(crate) fn reset_to_default(&mut self) -> GestureSettings {
        self.working = self.default;
        self.working
    }
}

#[cfg(test)]
#[path = "gesture_settings_tests.rs"]
mod tests;
