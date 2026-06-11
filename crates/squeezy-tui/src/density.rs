//! Adaptive Density (§12.4.1): auto-select a compact / default / expanded UI
//! density from the painted terminal size while honouring an explicit user
//! override. One model, one resolver — the renderer consumes the resolved
//! density (transcript-to-prompt gap, startup-card threshold, status detail
//! level) instead of re-deriving the same width/height checks at every paint
//! site.
//!
//! **One mode, one resolver.** The user-facing knob is a single
//! [`DensityMode`] — `Auto` (size-driven, the default), or one of the three
//! pinned modes (`Compact` / `Default` / `Expanded`). [`DensityMode::resolve`]
//! is the *one* place the terminal size is mapped to a [`ResolvedDensity`]; in
//! `Auto` it picks a tier from the painted `(cols, rows)`, and a pinned mode
//! short-circuits straight to that tier regardless of size. Every render seam
//! reads the resolved tier, never the raw dimensions, so the density policy
//! lives in exactly one table.
//!
//! **Reuses the existing size seam.** The caller resolves from the size the
//! renderer actually painted with — `last_frame_size` /
//! `off_frame_terminal_size` — so the density tracks the real viewport and is
//! deterministic under a headless `TestBackend` (no `terminal_size()` syscall in
//! the layout math). On a real terminal it equals the live size, so behaviour is
//! unchanged.
//!
//! **Preserves state.** Density only scales spacing, a card threshold, and the
//! status detail level — it never touches scroll anchors, selection, focus, the
//! queue, or the composer. A density change is a pure relayout of the same
//! model, so the spec's "preserve scroll anchor, selection, focus, queue, and
//! composer state" holds by construction (nothing here owns or mutates that
//! state).
//!
//! **Zero idle cost.** [`DensityMode::resolve`] is a pure, allocation-free
//! arithmetic function with no clock, no I/O, and no caching to invalidate. It
//! is called only while a frame is being laid out (and when the user cycles the
//! override); an idle session that paints nothing calls it zero times, so the
//! feature adds no idle redraw and no background work.

/// The user-facing density knob (§12.4.1). `Auto` (the default) derives the tier
/// from the painted terminal size; the three pinned modes force a fixed tier
/// regardless of size (the explicit user override the spec calls out). Persisted
/// at `[tui].density` as a bounded slug so a pick survives a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum DensityMode {
    /// Size-driven: [`DensityMode::resolve`] picks the tier from `(cols, rows)`.
    /// The built-in default, so a session that never sets an override adapts on
    /// its own.
    #[default]
    Auto,
    /// Pinned compact: minimal spacing and the leanest chrome regardless of size
    /// (what `Auto` would pick on a small terminal, forced everywhere).
    Compact,
    /// Pinned default: the baseline spacing/chrome regardless of size.
    Default,
    /// Pinned expanded: roomier spacing and the fullest chrome regardless of
    /// size (what `Auto` would pick on a large terminal, forced everywhere).
    Expanded,
}

impl DensityMode {
    /// Every mode in cycle order — `Auto → Compact → Default → Expanded → Auto`.
    /// Exhaustive on purpose: a new variant must be added here or it never
    /// appears in the cycle / persistence round-trip.
    pub(crate) const ALL: &'static [DensityMode] = &[
        DensityMode::Auto,
        DensityMode::Compact,
        DensityMode::Default,
        DensityMode::Expanded,
    ];

    /// The bounded slug persisted at `[tui].density`. Stable wire form; keep in
    /// sync with [`DensityMode::from_slug`].
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DensityMode::Auto => "auto",
            DensityMode::Compact => "compact",
            DensityMode::Default => "default",
            DensityMode::Expanded => "expanded",
        }
    }

    /// Short human label for the status line / cycle toast.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DensityMode::Auto => "auto",
            DensityMode::Compact => "compact",
            DensityMode::Default => "default",
            DensityMode::Expanded => "expanded",
        }
    }

    /// Parse a persisted slug back to a mode. Unknown / absent slugs collapse to
    /// `None` so the caller keeps the built-in default. Case-insensitive on the
    /// ASCII slug so a hand-edited config is forgiving.
    pub(crate) fn from_slug(slug: &str) -> Option<DensityMode> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(DensityMode::Auto),
            "compact" => Some(DensityMode::Compact),
            "default" => Some(DensityMode::Default),
            "expanded" => Some(DensityMode::Expanded),
            _ => None,
        }
    }

    /// The next mode in the cycle (`Auto → Compact → Default → Expanded → Auto`).
    /// Wraps, so repeated presses walk every mode and return to the start.
    pub(crate) fn next(self) -> DensityMode {
        let all = DensityMode::ALL;
        let idx = all.iter().position(|m| *m == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    /// Resolve this mode against the painted terminal size into the concrete
    /// [`ResolvedDensity`] the renderer consumes. In `Auto` the tier is derived
    /// from `(cols, rows)`; a pinned mode short-circuits to its fixed tier
    /// regardless of size. The single mapping from "what the user picked + how
    /// big the terminal is" to "how the frame is spaced".
    pub(crate) fn resolve(self, cols: u16, rows: u16) -> ResolvedDensity {
        let tier = match self {
            DensityMode::Auto => DensityTier::for_size(cols, rows),
            DensityMode::Compact => DensityTier::Compact,
            DensityMode::Default => DensityTier::Default,
            DensityMode::Expanded => DensityTier::Expanded,
        };
        ResolvedDensity { mode: self, tier }
    }
}

/// The resolved density tier — the bucket the renderer actually keys its spacing
/// off. Distinct from [`DensityMode`] because `Auto` collapses to one of these
/// once the size is known, and the pinned modes map 1:1. Ordered compact → roomy
/// so a `>=` comparison reads "at least this roomy".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum DensityTier {
    /// Small terminal: minimal spacing, leanest chrome.
    Compact,
    /// Mid terminal: the baseline spacing/chrome.
    Default,
    /// Large terminal: roomier spacing, fullest chrome.
    Expanded,
}

/// Width below which `Auto` drops to [`DensityTier::Compact`] (a genuinely narrow
/// terminal where every cell counts). At or above [`EXPANDED_MIN_COLS`] width
/// *and* [`EXPANDED_MIN_ROWS`] height, `Auto` rises to [`DensityTier::Expanded`].
/// Deliberately conservative: a standard 80-column terminal stays at the
/// `Default` tier so an existing session's layout is unchanged.
const COMPACT_MAX_COLS: u16 = 60;
/// Height below which `Auto` drops to [`DensityTier::Compact`] (a short terminal
/// where vertical rows are scarce — spacing must yield to content). Set at the
/// renderer's historical startup-card floor (`16`) so a normal-height terminal
/// resolves to `Default` and keeps the exact prior chrome; only a window shorter
/// than that (where the card already would not show) drops to compact.
const COMPACT_MAX_ROWS: u16 = 16;
/// Width at/above which `Auto` is eligible for [`DensityTier::Expanded`].
const EXPANDED_MIN_COLS: u16 = 120;
/// Height at/above which `Auto` is eligible for [`DensityTier::Expanded`].
const EXPANDED_MIN_ROWS: u16 = 40;

impl DensityTier {
    /// Auto-pick a tier from the painted terminal size. Compact when *either*
    /// dimension is small (a cramped terminal must spend its cells on content,
    /// not spacing); expanded when *both* dimensions are large (there is room to
    /// breathe); default in between. The asymmetry is deliberate — a single
    /// scarce dimension forces compact, but expanded requires slack on both axes
    /// so a wide-but-short or tall-but-narrow terminal stays at the default tier.
    pub(crate) fn for_size(cols: u16, rows: u16) -> DensityTier {
        if cols < COMPACT_MAX_COLS || rows < COMPACT_MAX_ROWS {
            DensityTier::Compact
        } else if cols >= EXPANDED_MIN_COLS && rows >= EXPANDED_MIN_ROWS {
            DensityTier::Expanded
        } else {
            DensityTier::Default
        }
    }
}

/// The concrete density the renderer reads, paired with the mode it came from
/// (so the status line can show "auto (compact)" vs. a pinned "compact"). Every
/// spacing/chrome decision is a method here, so the policy table is centralized
/// rather than scattered across paint sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedDensity {
    mode: DensityMode,
    tier: DensityTier,
}

impl ResolvedDensity {
    /// The mode this was resolved from (the user's pick, including `Auto`).
    pub(crate) fn mode(self) -> DensityMode {
        self.mode
    }

    /// The resolved tier the spacing keys off.
    pub(crate) fn tier(self) -> DensityTier {
        self.tier
    }

    /// This density with its tier raised to *at least* [`DensityTier::Expanded`],
    /// keeping the originating mode. Used by Presentation Mode (§12.4.6) to force
    /// the spacious layout through the *same* per-tier spacing table rather than a
    /// second spacing engine — an already-expanded density is returned unchanged,
    /// and a compact/default one is lifted to expanded. The mode is preserved so a
    /// status readout still shows what the user originally picked.
    pub(crate) fn at_least_expanded(self) -> ResolvedDensity {
        ResolvedDensity {
            mode: self.mode,
            tier: self.tier.max(DensityTier::Expanded),
        }
    }

    /// Blank rows inserted between the transcript and the prompt block. Compact
    /// spends none (every row goes to content); default keeps the single-row
    /// breather the renderer always had; expanded doubles it for a roomier feel.
    /// The renderer multiplies its own "is a gap wanted at all?" decision by this
    /// scale, so an empty session that wants no gap still gets none.
    pub(crate) fn transcript_prompt_gap(self) -> u16 {
        match self.tier {
            DensityTier::Compact => 0,
            DensityTier::Default => 1,
            DensityTier::Expanded => 2,
        }
    }

    /// Minimum terminal height at which the welcome / startup card is shown. The
    /// card is informational chrome; on a compact (short) terminal it yields to
    /// content sooner, on an expanded terminal it shows even on a slightly
    /// shorter window. Mirrors the renderer's prior hard-coded `>= 16` check at
    /// the default tier.
    pub(crate) fn startup_card_min_height(self) -> u16 {
        match self.tier {
            DensityTier::Compact => 20,
            DensityTier::Default => 16,
            DensityTier::Expanded => 14,
        }
    }

    /// The status-line detail level this density wants. Compact hides the
    /// secondary detail line to save a row; default and expanded keep it. The
    /// renderer treats this as a ceiling — it never *forces* a detail line the
    /// session has nothing to put in.
    pub(crate) fn shows_status_detail(self) -> bool {
        !matches!(self.tier, DensityTier::Compact)
    }

    /// A short, screen-reader-friendly readout of the active density for the
    /// status line / cycle toast: the picked mode, plus the resolved tier in
    /// parentheses when `Auto` (so the user can see what `Auto` chose). A pinned
    /// mode reads as just its label (the tier is implied).
    pub(crate) fn describe(self) -> String {
        match self.mode() {
            DensityMode::Auto => format!("auto ({})", self.tier_label()),
            other => other.label().to_string(),
        }
    }

    /// Lower-case tier label for [`ResolvedDensity::describe`].
    fn tier_label(self) -> &'static str {
        match self.tier() {
            DensityTier::Compact => "compact",
            DensityTier::Default => "default",
            DensityTier::Expanded => "expanded",
        }
    }
}

#[cfg(test)]
#[path = "density_tests.rs"]
mod tests;
