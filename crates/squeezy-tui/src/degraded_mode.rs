//! Automatic Degraded-Mode Suggestions (§12.9.4).
//!
//! When the terminal is *degraded* — a failed capability probe, mangled wide
//! glyphs, a tiny window, or no colour — Squeezy can keep painting, but the full
//! Unicode/expanded/mouse-driven chrome reads worse there than a plainer mode
//! would. Rather than silently forcing a fallback (which is wrong when the
//! detection is wrong) or saying nothing (which leaves the user staring at a torn
//! layout), this module *proactively suggests* a degraded mode the user can
//! accept with one keystroke or click: minimal-glyph chrome, compact density, and
//! mouse off. The spec is explicit that an **incorrect suggestion is worse than
//! unknown**, so every suggestion carries a confidence and is shown only when the
//! evidence is strong enough — and it is always dismissible.
//!
//! ## Reuses the existing capability / glyph / density machinery
//!
//! Nothing here re-detects anything. The detector consumes the §12.7.3
//! [`TerminalCapabilities`](crate::terminal_profile::TerminalCapabilities) probe
//! (the bounded, env-injected classifier), the §12.7.6
//! [`GlyphMode`](crate::glyph_mode::GlyphMode) the renderer is currently drawing
//! with, and the §12.4.1 [`DensityMode`](crate::density::DensityMode) /
//! resolved-size the layout already computed. The suggested target is expressed in
//! those same vocabularies, so accepting it routes straight through the existing
//! `set glyph mode` / `set density` / `set mouse` paths — there is no fourth
//! "degraded" engine.
//!
//! ## Model, not chrome
//!
//! Like its §12 peers ([`crate::first_run_hints`], [`crate::glyph_mode`],
//! [`crate::terminal_profile`]) this file owns only the *pure* model — the
//! evidence enum, the suggested target, the detection rule, and the
//! dismissed/settle state machine — so every rule is unit-testable without
//! standing up a `TuiApp` or a terminal. `lib.rs` owns the side effects: the
//! detect-and-arm call, the per-frame render of the dim suggestion banner, the
//! accept (apply the modes) and dismiss keybindings, and the click targets.
//!
//! ## Dismissible, settle-delayed, zero idle cost
//!
//! The banner never flashes on the first frame: a candidate must remain eligible
//! for a short settle window before it paints (the same restraint the §12.1.8
//! first-run hints use). The user can accept it (apply the modes) or dismiss it; a
//! dismissal latches for the session so the same suggestion never nags again.
//! Accepting also latches it. Once latched — or when the terminal is not degraded,
//! or already at the suggested modes — [`DegradedModeSuggestor::is_quiet`] is
//! `true`: the render path paints nothing and the redraw gate schedules no tick,
//! so an idle, non-degraded (or already-accepted) session pays one bool check per
//! frame and nothing more.

#![cfg_attr(not(unix), allow(dead_code))]

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::density::DensityMode;
use crate::glyph_mode::GlyphMode;
use crate::terminal_profile::{MouseMode, TerminalCapabilities};

/// One piece of evidence that the terminal is degraded, in *priority order* (the
/// declaration order is the order the banner lists them and the tie-break the
/// detector uses to name the lead reason). Each maps to a concrete impact and a
/// confidence so the spec's "show impact + confidence, never a bare guess" holds.
///
/// The set is intentionally small and closed: the four conditions the §12.9.4
/// steps call out for the *suggestion* surface — a failed/limited capability
/// probe (no truecolor where chrome wants it, an SSH/multiplexer hop that mangles
/// reporting), wide-glyph mis-rendering, a tiny window, and no colour. Richer
/// per-feature rows (OSC52, alternate scroll, focus, DEC2026) belong to the
/// `/terminal` table, not this proactive banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DegradedSignal {
    /// The terminal mis-renders *wide* Unicode glyphs (xterm.js inflates the
    /// braille spinner / moon family to two cells), so the full-Unicode chrome
    /// tears. The highest-confidence, highest-impact signal: it is the very bug
    /// that motivated the append-only renderer.
    MangledWideGlyphs,
    /// The window is genuinely tiny — below the size where the roomy chrome fits —
    /// so spacing should yield to content (compact density).
    TinySize,
    /// The terminal advertises no colour (`$NO_COLOR`, a dumb terminal, a pipe), so
    /// colour-coded chrome conveys nothing and the plainer mode reads the same.
    NoColor,
    /// A capability probe came back limited or unreliable: an SSH hop or a
    /// terminal multiplexer where mouse reporting and truecolor are the most likely
    /// to be silently dropped. Lower confidence — these are *probabilistic* — so it
    /// alone never crosses the suggestion threshold; it corroborates a stronger
    /// signal.
    UnreliableProbe,
}

impl DegradedSignal {
    /// Every signal in priority order — the banner's list order and the exhaustive
    /// set the tests sweep.
    pub(crate) const ALL: [DegradedSignal; 4] = [
        DegradedSignal::MangledWideGlyphs,
        DegradedSignal::TinySize,
        DegradedSignal::NoColor,
        DegradedSignal::UnreliableProbe,
    ];

    /// A short, stable, ASCII-only slug — used by tests and any future persisted
    /// record so the key never depends on the `Debug` spelling. `cfg(test)`-only
    /// today: production renders signals by their impact clause, never by slug.
    #[cfg(test)]
    pub(crate) fn slug(self) -> &'static str {
        match self {
            DegradedSignal::MangledWideGlyphs => "mangled_wide_glyphs",
            DegradedSignal::TinySize => "tiny_size",
            DegradedSignal::NoColor => "no_color",
            DegradedSignal::UnreliableProbe => "unreliable_probe",
        }
    }

    /// The user-facing impact clause shown on the banner (the spec's "show
    /// impact"). Terse — one short phrase — so the banner stays a single dim line.
    pub(crate) fn impact(self) -> &'static str {
        match self {
            DegradedSignal::MangledWideGlyphs => "wide glyphs mis-render (torn borders)",
            DegradedSignal::TinySize => "tiny window crowds the layout",
            DegradedSignal::NoColor => "no color (coded chrome is lost)",
            DegradedSignal::UnreliableProbe => "remote/multiplexed session may drop mouse+color",
        }
    }

    /// Detection confidence for this signal, higher = surer. The detector sums the
    /// active signals' confidence and only suggests once the total clears
    /// [`SUGGEST_CONFIDENCE_FLOOR`], so a lone low-confidence probe never fires but
    /// a strong glyph/size/color signal does. Keeping the weights here (not in the
    /// detector) means a re-tuning is a one-line edit.
    pub(crate) fn confidence(self) -> u8 {
        match self {
            // Deterministic from the detected terminal kind's known quirk — sure.
            DegradedSignal::MangledWideGlyphs => 3,
            // Measured directly from the painted size — sure.
            DegradedSignal::TinySize => 3,
            // `$NO_COLOR` / monochrome is an explicit, observable state — sure.
            DegradedSignal::NoColor => 3,
            // Probabilistic: an SSH/multiplexer hop *might* drop reporting. On its
            // own it stays under the floor; it only tips an already-strong case.
            DegradedSignal::UnreliableProbe => 1,
        }
    }
}

/// Minimum summed confidence (over the active [`DegradedSignal`]s) before the
/// banner is offered at all. Set so any single sure signal (weight `3`) crosses it
/// but a lone unreliable-probe (weight `1`) does not — the spec's "incorrect
/// suggestion is worse than unknown" floor.
pub(crate) const SUGGEST_CONFIDENCE_FLOOR: u8 = 3;

/// Width or height at/below which the window counts as [`DegradedSignal::TinySize`].
/// Deliberately small — well under the §12.4.1 compact threshold — so the banner
/// only fires on a genuinely cramped terminal, not a merely narrow one (which
/// Adaptive Density already handles silently).
pub(crate) const TINY_COLS: u16 = 48;
/// Height at/below which the window counts as tiny (see [`TINY_COLS`]).
pub(crate) const TINY_ROWS: u16 = 12;

/// The concrete degraded-mode target the banner suggests — expressed in the
/// existing §12.7.6 / §12.4.1 / §12.7.3 vocabularies so accepting it routes
/// straight through the live `set glyph mode` / `set density` / `set mouse`
/// paths. Each field is `Some` only when that knob is *not already* at the
/// suggested value, so accepting changes exactly what is degraded and nothing
/// else (and a banner with every field `None` is suppressed — there is nothing
/// left to suggest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DegradedTarget {
    /// Suggested glyph mode (ASCII-safe chrome), or `None` when the renderer is
    /// already at or below it.
    pub(crate) glyph_mode: Option<GlyphMode>,
    /// Suggested density (pinned compact), or `None` when already compact.
    pub(crate) density: Option<DensityMode>,
    /// Suggested mouse policy (off), or `None` when mouse is already disabled or no
    /// mouse-related signal fired.
    pub(crate) mouse: Option<MouseMode>,
}

impl DegradedTarget {
    /// Whether this target would change *anything*. A target that matches the
    /// current state on every knob is empty — the detector treats it as "nothing
    /// to suggest" and stays quiet.
    pub(crate) fn is_empty(self) -> bool {
        self.glyph_mode.is_none() && self.density.is_none() && self.mouse.is_none()
    }

    /// A short, screen-reader-friendly summary of what accepting changes, e.g.
    /// "ASCII chrome + compact + mouse off". Lists only the knobs that actually
    /// move, in a stable order. Never empty when [`is_empty`](Self::is_empty) is
    /// false.
    pub(crate) fn summary(self) -> String {
        let mut parts: Vec<&'static str> = Vec::new();
        if self.glyph_mode.is_some() {
            parts.push("ASCII chrome");
        }
        if self.density.is_some() {
            parts.push("compact");
        }
        if self.mouse.is_some() {
            parts.push("mouse off");
        }
        parts.join(" + ")
    }
}

/// A fully-formed degraded-mode suggestion: the active evidence (with the lead
/// reason first) and the concrete target to apply. Produced by
/// [`DegradedModeSuggestor::detect`] from the live capability / glyph / density
/// inputs; consumed by the banner render and the accept handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DegradedSuggestion {
    /// The active signals in priority order; `signals[0]` is the lead reason the
    /// banner names. Never empty (a suggestion with no evidence is never built).
    signals: Vec<DegradedSignal>,
    /// The concrete modes to apply on accept. Never empty (a no-op target is never
    /// built).
    target: DegradedTarget,
}

impl DegradedSuggestion {
    /// The active signals, in priority order. `cfg(test)`-only: production names the
    /// lead reason via [`lead_signal`](Self::lead_signal) and counts via
    /// [`signal_count`](Self::signal_count); the full list is swept by the tests.
    #[cfg(test)]
    pub(crate) fn signals(&self) -> &[DegradedSignal] {
        &self.signals
    }

    /// How many distinct degraded signals fired. Production reads this to note "(+N
    /// more)" on the banner when several reasons corroborate; the lead reason is
    /// shown in full and the rest are summarized by count so the line stays terse.
    pub(crate) fn signal_count(&self) -> usize {
        self.signals.len()
    }

    /// The lead (highest-priority) signal — the reason the banner names first.
    /// Always present: a suggestion is never built with an empty signal set.
    pub(crate) fn lead_signal(&self) -> DegradedSignal {
        self.signals[0]
    }

    /// The concrete modes to apply on accept.
    pub(crate) fn target(&self) -> DegradedTarget {
        self.target
    }

    /// The summed confidence over the active signals — the value the detector
    /// compared to [`SUGGEST_CONFIDENCE_FLOOR`]. Exposed so the banner can render a
    /// low/med/high badge (the spec's "include confidence").
    pub(crate) fn confidence(&self) -> u8 {
        self.signals.iter().map(|s| s.confidence()).sum()
    }

    /// A coarse confidence label for the banner badge. Single sure signal → "med";
    /// two or more (a corroborated case) → "high". Never "low": a sub-floor case is
    /// never built into a suggestion at all.
    pub(crate) fn confidence_label(&self) -> &'static str {
        if self.confidence() >= SUGGEST_CONFIDENCE_FLOOR * 2 {
            "high"
        } else {
            "med"
        }
    }
}

/// How long a suggestion must remain eligible before the banner paints. Short
/// enough to feel responsive, long enough that a transient tiny-window resize
/// (dragging a pane across the threshold) does not flash the banner. Distinct from
/// any animation timing — purely "don't show instantly".
const SUGGEST_SETTLE: Duration = Duration::from_millis(600);

/// The Automatic Degraded-Mode Suggestions engine (§12.9.4): the dismissed latch
/// plus the settle clock that, together with a freshly-detected
/// [`DegradedSuggestion`], decides whether the banner paints this frame. Held by
/// `TuiApp` directly (not behind an `Option`) because the resting state — enabled,
/// not yet dismissed — is itself cheap, and the dismissed/non-degraded state
/// collapses to a single bool check via [`is_quiet`].
///
/// [`is_quiet`]: DegradedModeSuggestor::is_quiet
#[derive(Debug, Clone)]
pub(crate) struct DegradedModeSuggestor {
    /// Whether the feature is enabled. Off ⇒ no banner ever shows and
    /// [`is_quiet`](Self::is_quiet) is `true`, so a user who finds the suggestion
    /// noise can silence it entirely.
    enabled: bool,
    /// Latched `true` once the user accepts or dismisses the suggestion. One-way
    /// for the session: a dismissed suggestion never nags again, even if the same
    /// degraded condition persists. Accepting also latches it (the condition is now
    /// addressed).
    dismissed: bool,
    /// When the suggestion first became *eligible* to show (the first frame
    /// [`active`](Self::active) saw a degraded terminal while not dismissed).
    /// `None` until then. The settle delay is `now - first_eligible`, so the banner
    /// never flashes on the frame it becomes eligible. A [`Cell`] so the render
    /// path can stamp it while holding `&TuiApp`, exactly like the §12.1.8 hint
    /// engine and the frame-local hit-test registry.
    first_eligible: Cell<Option<Instant>>,
    /// Whether the banner is currently painted on screen. A [`Cell`] for the same
    /// reason as `first_eligible`: the render-time [`active`](Self::active) query
    /// records that the banner is live (so accept/dismiss know whether to request
    /// an erase repaint) while holding only `&self`.
    showing: Cell<bool>,
}

impl Default for DegradedModeSuggestor {
    fn default() -> Self {
        Self {
            // Enabled by default: the banner is restrained (a single dim line,
            // settle-delayed, dismissible, shown at most once per session) and only
            // ever appears on a genuinely degraded terminal.
            enabled: true,
            dismissed: false,
            first_eligible: Cell::new(None),
            showing: Cell::new(false),
        }
    }
}

impl DegradedModeSuggestor {
    /// Whether the feature is enabled. `cfg(test)`-only: production reads the flag
    /// through the internal checks in [`active`](Self::active).
    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Disable the whole feature, clearing any banner currently shown so a stale
    /// line cannot linger. `cfg(test)`-only today: production keeps the feature on
    /// and reaches quiet through accept/dismiss; the disabled path is exercised by
    /// the "disabled config" unit test.
    #[cfg(test)]
    pub(crate) fn disable(&mut self) {
        self.enabled = false;
        self.showing.set(false);
    }

    /// Whether the suggestion has been accepted or dismissed this session.
    /// `cfg(test)`-only: production decides what to paint through
    /// [`active`](Self::active), never by reading the latch directly.
    #[cfg(test)]
    pub(crate) fn is_dismissed(&self) -> bool {
        self.dismissed
    }

    /// True when the engine has nothing to do: disabled, or the suggestion has been
    /// accepted/dismissed. The render path paints nothing and the redraw gate
    /// schedules no tick — the cheap idle check the spec's "zero idle cost" wants.
    /// (A *non-degraded* terminal is also effectively quiet: [`active`](Self::
    /// active) returns `None` and stamps no clock, so `reveal_pending` stays
    /// `false`.)
    pub(crate) fn is_quiet(&self) -> bool {
        !self.enabled || self.dismissed
    }

    /// Whether the banner is currently painted on screen — recorded by the
    /// render-time [`active`](Self::active) query. The accept/dismiss handlers gate
    /// on this so the chord/click is a no-op fall-through unless a banner is
    /// actually showing (so it never steals a key from the composer beneath).
    pub(crate) fn is_showing(&self) -> bool {
        self.showing.get()
    }

    /// Latch the suggestion accepted (the user pressed the accept chord / clicked
    /// Accept). Returns `true` when this call actually retired a still-pending
    /// suggestion (so the caller can request one erase repaint). Idempotent: a
    /// no-op once already latched.
    pub(crate) fn accept(&mut self) -> bool {
        let was_pending = !self.dismissed;
        self.dismissed = true;
        self.showing.set(false);
        was_pending
    }

    /// Latch the suggestion dismissed (the user pressed the dismiss chord / clicked
    /// the strip). Returns `true` when this call actually retired a still-pending
    /// suggestion. Idempotent.
    pub(crate) fn dismiss(&mut self) -> bool {
        let was_pending = !self.dismissed;
        self.dismissed = true;
        self.showing.set(false);
        was_pending
    }

    /// Detect a degraded terminal from the live capability probe, the painted size,
    /// and the modes the renderer is *currently* drawing with, returning the
    /// suggestion to offer — or `None` when the terminal is not degraded (or every
    /// suggested knob is already at its target, leaving nothing to change).
    ///
    /// Pure: no clock, no I/O, no `self` mutation. The detection rule is the single
    /// place "is this terminal degraded, and toward what?" is decided, so the
    /// banner render and the unit tests evaluate exactly the same logic.
    pub(crate) fn detect(
        caps: TerminalCapabilities,
        cols: u16,
        rows: u16,
        current_glyph: GlyphMode,
        current_density: DensityMode,
        current_mouse: MouseMode,
    ) -> Option<DegradedSuggestion> {
        // Gather the active evidence by sweeping `DegradedSignal::ALL` (the single
        // source of priority order) and keeping each signal whose condition holds —
        // so the collected list is already in priority order and a re-prioritization
        // is a one-line edit to `ALL`.
        let signals: Vec<DegradedSignal> = DegradedSignal::ALL
            .into_iter()
            .filter(|signal| match signal {
                // Wide-glyph mis-render is a known per-terminal quirk: xterm.js (VS
                // Code / browser terminal) inflates the wide spinner/moon family to
                // two cells. The §12.7.3 default table already flags these terminals
                // ASCII-by-default, so reuse that identity rather than re-sniffing.
                DegradedSignal::MangledWideGlyphs => matches!(
                    caps.kind,
                    crate::dogfood::TerminalProfile::MacosVscode
                        | crate::dogfood::TerminalProfile::LinuxVscode
                ),
                // A genuinely cramped window (both axes small enough that even
                // compact chrome is tight). Measured from the painted size.
                DegradedSignal::TinySize => cols <= TINY_COLS && rows <= TINY_ROWS,
                // No colour: an explicit `$NO_COLOR` opt-out or a terminal that
                // advertises none. Colour-coded chrome conveys nothing here.
                DegradedSignal::NoColor => caps.no_color,
                // A remote / multiplexed session: mouse reporting and truecolor are
                // the most likely to be silently dropped across the hop. Low
                // confidence on its own; it corroborates a stronger signal.
                DegradedSignal::UnreliableProbe => caps.over_ssh || caps.inside_multiplexer,
            })
            .collect();

        if signals.is_empty() {
            return None;
        }

        // The spec's confidence floor: never suggest off a lone low-confidence
        // probe. A single sure signal (glyph / size / color) clears it.
        let confidence: u8 = signals.iter().map(|s| s.confidence()).sum();
        if confidence < SUGGEST_CONFIDENCE_FLOOR {
            return None;
        }

        // Build the concrete target, but only for knobs that are not already at the
        // suggested value (so accepting changes exactly what is degraded).
        let glyph_mode = (current_glyph != GlyphMode::Ascii).then_some(GlyphMode::Ascii);
        let density = (current_density != DensityMode::Compact).then_some(DensityMode::Compact);
        // Mouse is only suggested off when a *mouse-relevant* signal fired (an
        // unreliable remote/multiplexed probe) and it is not already off — turning
        // the mouse off on a merely no-color / tiny local terminal would be
        // gratuitous.
        let mouse_relevant = signals.contains(&DegradedSignal::UnreliableProbe);
        let mouse =
            (mouse_relevant && current_mouse != MouseMode::Disabled).then_some(MouseMode::Disabled);

        let target = DegradedTarget {
            glyph_mode,
            density,
            mouse,
        };
        if target.is_empty() {
            // Degraded, but already at every suggested mode — nothing to offer.
            return None;
        }

        Some(DegradedSuggestion { signals, target })
    }

    /// The suggestion to paint this frame, or `None` for nothing. Combines the
    /// freshly-detected `suggestion` (from [`detect`](Self::detect), passed in so
    /// the caller does the env-free read) with the engine's dismissed latch and
    /// settle clock:
    ///
    /// - disabled or dismissed ⇒ `None` (the quiet resting state),
    /// - no degraded suggestion this frame ⇒ `None`, and the settle clock is reset
    ///   so a terminal that *recovers* (e.g. a resize back above the tiny floor)
    ///   does not later flash a stale banner,
    /// - while `suppressed` (a modal / overlay / search owns the surface) ⇒ `None`,
    ///   without advancing the clock, so the banner never burns its showing behind
    ///   a modal,
    /// - otherwise the suggestion, but only after it has been eligible for at least
    ///   [`SUGGEST_SETTLE`] — so it never flashes instantly.
    ///
    /// Takes `&self` (not `&mut`): the only mutation is stamping the settle clock
    /// and recording the banner is on screen — both through [`Cell`]s — so the
    /// render path can call it while holding `&TuiApp`, exactly like the §12.1.8
    /// hint engine. The stamp is monotonic and idempotent; it never flips a visible
    /// decision on its own.
    pub(crate) fn active(
        &self,
        suggestion: Option<&DegradedSuggestion>,
        now: Instant,
        suppressed: bool,
    ) -> bool {
        if self.is_quiet() {
            self.first_eligible.set(None);
            self.showing.set(false);
            return false;
        }
        let Some(_suggestion) = suggestion else {
            // Not degraded (or nothing left to suggest): reset the settle clock so a
            // later degradation starts its window fresh, and stop "showing".
            self.first_eligible.set(None);
            self.showing.set(false);
            return false;
        };
        if suppressed {
            // A modal owns the surface: do not paint, but do NOT clear the settle
            // clock — the banner reappears (still un-dismissed) when the modal
            // closes, rather than restarting its settle window.
            self.showing.set(false);
            return false;
        }
        let first = match self.first_eligible.get() {
            Some(first) => first,
            None => {
                self.first_eligible.set(Some(now));
                now
            }
        };
        if now.duration_since(first) < SUGGEST_SETTLE {
            // Still settling: nothing paints yet, and we are not "showing".
            self.showing.set(false);
            return false;
        }
        self.showing.set(true);
        true
    }

    /// Whether the settle window is *in flight* — eligible (a render stamped the
    /// clock) and still inside [`SUGGEST_SETTLE`], or already showing — so the
    /// caller schedules exactly one follow-up redraw to paint the banner once the
    /// settle elapses, then goes quiet. Returns `false` when quiet, suppressed, not
    /// degraded, OR — critically — when the candidate has not yet been stamped
    /// (`first_eligible == None`): the first stamp happens inside [`active`](Self::
    /// active) on a render the focused session is already doing, so an idle,
    /// never-rendered session schedules NO tick of its own. This preserves the
    /// zero-idle-cost invariant: the engine never spins up the animation loop from
    /// cold; it only keeps an already-running settle alive. Read-only: never
    /// advances the settle clock.
    pub(crate) fn reveal_pending(
        &self,
        suggestion: Option<&DegradedSuggestion>,
        now: Instant,
        suppressed: bool,
    ) -> bool {
        if self.is_quiet() || suppressed || suggestion.is_none() {
            return false;
        }
        match self.first_eligible.get() {
            // Not yet stamped: do NOT schedule a tick from cold.
            None => false,
            // Stamped and still inside the settle window, or already showing: a
            // paint is imminent, so the caller should schedule the wake-up.
            Some(first) => now.duration_since(first) < SUGGEST_SETTLE || self.showing.get(),
        }
    }

    /// Force any pending suggestion to be *already settled* (stamp the eligibility
    /// window well in the past), so a subsequent [`active`](Self::active) at "now"
    /// paints it without a real-time wait. `cfg(test)`-only: it exists purely so the
    /// integration tests can drive a settled banner through the real `render()`
    /// deterministically, never depending on wall-clock sleeps.
    #[cfg(test)]
    pub(crate) fn force_settle_for_test(&self) {
        // Stamp the eligibility clock far enough in the past that the candidate
        // reads as long-settled at any plausible test "now". An hour dwarfs
        // SUGGEST_SETTLE, but `Instant - Duration` PANICS on platforms whose
        // monotonic clock is younger than the amount we subtract (Windows CI
        // runners boot with a tiny QPC value), so subtract via `checked_sub` and
        // fall back to the largest clock-safe offset that still clears the settle
        // window with margin.
        let now = Instant::now();
        let settled = now
            .checked_sub(Duration::from_secs(3600))
            .or_else(|| now.checked_sub(SUGGEST_SETTLE * 4))
            .unwrap_or(now);
        self.first_eligible.set(Some(settled));
    }
}

#[cfg(test)]
#[path = "degraded_mode_tests.rs"]
mod tests;
