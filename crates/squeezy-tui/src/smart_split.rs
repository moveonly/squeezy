//! Smart Split Panes (§12.4.2).
//!
//! A single layout *policy* that decides how to pair the scrolling transcript
//! with a secondary content pane — a detail/diff pane, the scratchpad, or the
//! pinned-compare surface — choosing the *orientation* (side-by-side vs.
//! top/bottom stack) and the split sizes from the terminal's aspect ratio and
//! the pane's minimum size, and degrading gracefully to a single transcript
//! column when the terminal is too small to split at all.
//!
//! ## Why a policy module
//!
//! Several features already carve a second surface off the main view —
//! [`crate::diff_detail_pane`] (§11G.10) splits the Ctrl+T overlay into a
//! transcript column + a fixed detail pane, [`crate::pinned_compare`] (§12.2.3)
//! shows two equal panes side-by-side or stacked, and the §12.3.3 scratchpad
//! splits the main view into transcript + scratch. Each picked its own
//! threshold and orientation rule. The §12.4.2 spec asks for *one* policy table
//! ("keep one policy table") that answers "given this terminal and this pane,
//! where does the pane go?" so the special-case sprawl stops growing. This
//! module is that table: [`LayoutSolver::solve`] is the single decision point,
//! and the geometry primitives ([`pane_inner`], [`rect_contains`]) mirror the
//! detail-pane / pinned-compare helpers so a reader of any one understands the
//! rest.
//!
//! ## The decision
//!
//! Given a content [`Rect`], a [`PaneKind`], and a requested [`Orientation`],
//! [`LayoutSolver::solve`] returns a [`PanePlacement`]:
//!
//! * [`PanePlacement::Side`] — a wide terminal: the transcript on the left and
//!   the pane on the right, with a one-cell vertical separator. Chosen when the
//!   content is wide enough for two readable columns (`width >= MIN_SIDE_WIDTH`)
//!   and the aspect favours width, or when the caller forces it.
//! * [`PanePlacement::Stacked`] — a tall/narrow terminal: the transcript on top
//!   and the pane below a one-row divider. Chosen when there is not enough width
//!   for two columns but enough height for two readable bands
//!   (`height >= MIN_STACK_HEIGHT`), or when the caller forces it.
//! * [`PanePlacement::SingleColumn`] — the graceful fallback: too small for
//!   either split, so the transcript keeps the whole content and the pane is
//!   deferred to a modal/overlay (the spec's "impossible layouts degrade
//!   gracefully"). The pane is never painted into a uselessly-narrow sliver.
//!
//! The pane's share of the content is [`PaneKind::ideal_fraction`] of the
//! splittable axis, clamped so the pane keeps at least [`PaneKind::min_main`]
//! cells for the transcript and at least one cell for itself, and biased by the
//! user's [`SplitRatio`] adjustment so the split is keyboard/mouse tunable.
//!
//! ## Pure geometry, no `TuiApp`
//!
//! Like its peer leaf modules ([`crate::diff_detail_pane`],
//! [`crate::pinned_compare`], [`crate::interaction`]) this file holds only the
//! pure policy + geometry, the small overlay-state struct, and the tiny preview
//! region drawing helper that belongs to that overlay. The crate root owns the
//! keybinding, the per-frame render call, the overlay open/close flag, and the
//! hit-test registration. Keeping the math here lets every threshold and
//! placement rule be unit-tested without a terminal.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

/// The minimum content width (in cells) at which a side-by-side split is worth
/// doing. Below this the two columns would each be uselessly narrow, so the
/// solver falls back to a stack (if tall enough) or a single column. Matches the
/// detail-pane / pinned-compare `MIN_SPLIT_WIDTH` so all three features flip to
/// their fallback at the same width.
pub(crate) const MIN_SIDE_WIDTH: u16 = 64;

/// The minimum content height at which a stacked split can show two readable
/// bands (each needs a border + at least one content row, plus a one-row
/// divider). Below this even stacking is hopeless and the solver returns a
/// single column. Mirrors `pinned_compare::MIN_STACK_HEIGHT`.
pub(crate) const MIN_STACK_HEIGHT: u16 = 7;

/// One row/column of separator drawn between the transcript and the pane.
pub(crate) const SEPARATOR: u16 = 1;

/// The aspect-ratio threshold, expressed as `width * 10 / height`, at or above
/// which a terminal is "wide enough" that a side split reads better than a
/// stack. `16` ≈ a 1.6:1 width:height ratio — a touch wider than the classic
/// 80x24 (≈ 3.3:1 in *cells*, but cells are ~2:1 tall, so visually ≈ 1.6:1).
/// Below this the terminal is comparatively tall and a stack uses the space
/// better. Used only when the caller asks for [`Orientation::Auto`].
const WIDE_ASPECT_TENTHS: u32 = 16;

/// Which secondary content pane the transcript is paired with. Each kind carries
/// its own ideal split fraction and minimum transcript width, so the one policy
/// table can size a heavy diff pane differently from a slim scratch pane without
/// the callers re-deriving thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum PaneKind {
    /// The §11G.10 diff / detail pane: a fully-expanded entry body (a big diff,
    /// a file excerpt, bulky tool output). Wants a generous share to show a
    /// wrapped diff without re-wrapping every line.
    Detail,
    /// The §12.3.3 scratchpad: a slim notes/composition buffer. Wants a smaller
    /// share so the transcript keeps the majority for context.
    Scratch,
    /// The §12.2.3 pinned-compare surface: two equal columns. Wants an even
    /// half-and-half split so neither side dominates.
    Compare,
}

impl PaneKind {
    /// Every kind, in a stable order — the overlay's cycle order and the
    /// exhaustive set the tests sweep.
    pub(crate) const ALL: [PaneKind; 3] = [PaneKind::Detail, PaneKind::Scratch, PaneKind::Compare];

    /// Friendly one-word label for the overlay row / status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PaneKind::Detail => "Detail",
            PaneKind::Scratch => "Scratch",
            PaneKind::Compare => "Compare",
        }
    }

    /// A one-line note on what the pane is, shown beside the label.
    pub(crate) fn description(self) -> &'static str {
        match self {
            PaneKind::Detail => "diff / detail pane (\u{a7}11G.10)",
            PaneKind::Scratch => "scratchpad notes pane (\u{a7}12.3.3)",
            PaneKind::Compare => "pinned compare surface (\u{a7}12.2.3)",
        }
    }

    /// The pane's ideal share of the splittable axis, as a `(numerator,
    /// denominator)` fraction. The detail pane takes two-fifths (room for a
    /// wrapped diff), scratch one-third (a slim sidebar), and compare one-half
    /// (two equal columns).
    pub(crate) fn ideal_fraction(self) -> (u16, u16) {
        match self {
            PaneKind::Detail => (2, 5),
            PaneKind::Scratch => (1, 3),
            PaneKind::Compare => (1, 2),
        }
    }

    /// The minimum number of cells the *transcript* (the main column/band) must
    /// keep after the pane is carved off, so the context column never collapses
    /// to an unreadable sliver. The pane share is clamped against this.
    pub(crate) fn min_main(self) -> u16 {
        match self {
            // The compare view is two equal columns, so the "main" side can be
            // as narrow as the pane; keep a modest floor.
            PaneKind::Compare => 20,
            // Detail/scratch keep the transcript as the context majority.
            PaneKind::Detail | PaneKind::Scratch => 28,
        }
    }

    /// Index of this kind into [`PaneKind::ALL`] — the row index the overlay
    /// marks and a click targets.
    pub(crate) fn index(self) -> usize {
        PaneKind::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }
}

/// The orientation the caller requests from the solver. [`Orientation::Auto`]
/// lets the solver pick from the terminal aspect; the two explicit variants
/// force a side or stacked split (still degrading to a single column when even
/// the forced orientation cannot fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Orientation {
    /// Let the solver choose from the terminal's aspect ratio.
    Auto,
    /// Force a side-by-side split (transcript left, pane right) when width
    /// allows; otherwise fall back to a single column.
    Side,
    /// Force a stacked split (transcript top, pane bottom) when height allows;
    /// otherwise fall back to a single column.
    Stacked,
}

impl Orientation {
    /// Every orientation, in a stable order — the overlay's cycle order and the
    /// exhaustive set the tests sweep.
    pub(crate) const ALL: [Orientation; 3] =
        [Orientation::Auto, Orientation::Side, Orientation::Stacked];

    /// Friendly label for the overlay / status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Orientation::Auto => "Auto",
            Orientation::Side => "Side",
            Orientation::Stacked => "Stacked",
        }
    }

    /// The next orientation in the cycle (wraps), used by the overlay's
    /// ←/→/Space and a click on the orientation row.
    pub(crate) fn next(self) -> Orientation {
        let idx = Orientation::ALL
            .iter()
            .position(|o| *o == self)
            .unwrap_or(0);
        Orientation::ALL[(idx + 1) % Orientation::ALL.len()]
    }
}

/// A keyboard/mouse-tunable bias on the pane's share, in fixed steps. The base
/// share is [`PaneKind::ideal_fraction`]; each step shifts roughly one tenth of
/// the splittable axis toward (`Wider`) or away from (`Narrower`) the pane,
/// clamped so neither side starves. Stored as a signed step count so the bias is
/// a small bounded integer the overlay can nudge and reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SplitRatio(i8);

impl SplitRatio {
    /// The neutral bias — the pane gets exactly its ideal fraction.
    pub(crate) const NEUTRAL: SplitRatio = SplitRatio(0);

    /// The bias step range, `[-MAX_STEPS, MAX_STEPS]`. Three steps each way is
    /// enough to swing the split from "slim sidebar" to "pane-dominant" without
    /// letting either side vanish (the clamp in [`solve_axis`] still applies).
    const MAX_STEPS: i8 = 3;

    /// The fraction of the splittable axis one step shifts, as tenths.
    const STEP_TENTHS: i32 = 1;

    /// Nudge the pane wider by one step (clamped at the maximum). Returns whether
    /// the bias actually changed.
    pub(crate) fn widen(&mut self) -> bool {
        if self.0 >= Self::MAX_STEPS {
            return false;
        }
        self.0 += 1;
        true
    }

    /// Nudge the pane narrower by one step (clamped at the minimum). Returns
    /// whether the bias actually changed.
    pub(crate) fn narrow(&mut self) -> bool {
        if self.0 <= -Self::MAX_STEPS {
            return false;
        }
        self.0 -= 1;
        true
    }

    /// The signed step count, for the overlay's readout / status line.
    pub(crate) fn steps(self) -> i8 {
        self.0
    }

    /// The bias as a signed number of *tenths* of the splittable axis, applied
    /// on top of the pane's ideal fraction in [`solve_axis`].
    fn bias_tenths(self) -> i32 {
        i32::from(self.0) * Self::STEP_TENTHS
    }
}

/// Where the solver placed the secondary pane relative to the transcript. The
/// `main` rect is always the transcript; `pane` is the secondary surface;
/// `separator` is the one-cell rule between them. [`PanePlacement::SingleColumn`]
/// carries only the transcript: there was no room to split, so the pane is
/// deferred to a modal/overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanePlacement {
    /// Wide terminal: transcript left, pane right, vertical separator between.
    Side {
        main: Rect,
        separator: Rect,
        pane: Rect,
    },
    /// Tall/narrow terminal: transcript top, pane bottom, horizontal divider
    /// between.
    Stacked {
        main: Rect,
        separator: Rect,
        pane: Rect,
    },
    /// Too small for either split: the transcript keeps the whole content and
    /// the pane is deferred. The graceful fallback.
    SingleColumn { main: Rect },
}

impl PanePlacement {
    /// The transcript rect, regardless of placement.
    pub(crate) fn main(self) -> Rect {
        match self {
            PanePlacement::Side { main, .. }
            | PanePlacement::Stacked { main, .. }
            | PanePlacement::SingleColumn { main } => main,
        }
    }

    /// The secondary pane's rect, or `None` when the layout degraded to a single
    /// column (so the caller routes the pane to a modal/overlay instead).
    pub(crate) fn pane(self) -> Option<Rect> {
        match self {
            PanePlacement::Side { pane, .. } | PanePlacement::Stacked { pane, .. } => Some(pane),
            PanePlacement::SingleColumn { .. } => None,
        }
    }

    /// The separator rect, or `None` for a single column (no split, no rule).
    pub(crate) fn separator(self) -> Option<Rect> {
        match self {
            PanePlacement::Side { separator, .. } | PanePlacement::Stacked { separator, .. } => {
                Some(separator)
            }
            PanePlacement::SingleColumn { .. } => None,
        }
    }

    /// Whether the solver split the surface at all (vs. the single-column
    /// fallback). Drives the overlay's "would split" / "single column" readout.
    pub(crate) fn is_split(self) -> bool {
        !matches!(self, PanePlacement::SingleColumn { .. })
    }

    /// A short label for the chosen placement, for the overlay / status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PanePlacement::Side { .. } => "side-by-side",
            PanePlacement::Stacked { .. } => "stacked",
            PanePlacement::SingleColumn { .. } => "single column",
        }
    }
}

/// The Smart Split Panes layout policy. Holds the min thresholds (defaulting to
/// the module constants) so a test can drive the solver at custom sizes, and
/// exposes the single [`Self::solve`] decision the renderer calls. Stateless
/// beyond its thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutSolver {
    /// Minimum content width for a side-by-side split.
    pub(crate) min_side_width: u16,
    /// Minimum content height for a stacked split.
    pub(crate) min_stack_height: u16,
}

impl Default for LayoutSolver {
    fn default() -> Self {
        Self {
            min_side_width: MIN_SIDE_WIDTH,
            min_stack_height: MIN_STACK_HEIGHT,
        }
    }
}

impl LayoutSolver {
    /// Whether the terminal's aspect ratio favours a side split. Computed as
    /// `width * 10 / height >= WIDE_ASPECT_TENTHS`, with a zero-height guard. The
    /// `Auto` orientation uses this to pick side vs. stacked; the explicit
    /// orientations ignore it.
    pub(crate) fn prefers_side(self, content: Rect) -> bool {
        if content.height == 0 {
            return false;
        }
        let tenths = u32::from(content.width) * 10 / u32::from(content.height);
        tenths >= WIDE_ASPECT_TENTHS
    }

    /// THE decision: place the `kind` pane beside / below / deferred-from the
    /// transcript inside `content`, honouring the requested `orientation` and the
    /// `ratio` bias. The single policy point §12.4.2 asks for.
    ///
    /// Order of operations:
    /// 1. A zero-area content always yields a (zero) single column — nothing to
    ///    split.
    /// 2. `Auto` resolves to side or stacked from [`Self::prefers_side`]; the
    ///    explicit orientations are taken as-is.
    /// 3. The chosen orientation is attempted; if it does not fit, the *other*
    ///    orientation is tried (a wide-but-short terminal that asked to stack can
    ///    still side-split, and vice versa), and only if neither fits does the
    ///    layout degrade to a single column.
    pub(crate) fn solve(
        self,
        content: Rect,
        kind: PaneKind,
        orientation: Orientation,
        ratio: SplitRatio,
    ) -> PanePlacement {
        if content.width == 0 || content.height == 0 {
            return PanePlacement::SingleColumn { main: content };
        }
        let want_side = match orientation {
            Orientation::Side => true,
            Orientation::Stacked => false,
            Orientation::Auto => self.prefers_side(content),
        };
        if want_side {
            self.try_side(content, kind, ratio)
                .or_else(|| self.try_stacked(content, kind, ratio))
                .unwrap_or(PanePlacement::SingleColumn { main: content })
        } else {
            self.try_stacked(content, kind, ratio)
                .or_else(|| self.try_side(content, kind, ratio))
                .unwrap_or(PanePlacement::SingleColumn { main: content })
        }
    }

    /// Attempt a side-by-side split, or `None` if the content is too narrow.
    fn try_side(self, content: Rect, kind: PaneKind, ratio: SplitRatio) -> Option<PanePlacement> {
        if content.width < self.min_side_width {
            return None;
        }
        let (main_w, pane_w) = solve_axis(content.width, kind, ratio)?;
        let main = Rect {
            x: content.x,
            y: content.y,
            width: main_w,
            height: content.height,
        };
        let separator = Rect {
            x: content.x + main_w,
            y: content.y,
            width: SEPARATOR,
            height: content.height,
        };
        let pane = Rect {
            x: content.x + main_w + SEPARATOR,
            y: content.y,
            width: pane_w,
            height: content.height,
        };
        Some(PanePlacement::Side {
            main,
            separator,
            pane,
        })
    }

    /// Attempt a stacked split, or `None` if the content is too short.
    fn try_stacked(
        self,
        content: Rect,
        kind: PaneKind,
        ratio: SplitRatio,
    ) -> Option<PanePlacement> {
        if content.height < self.min_stack_height {
            return None;
        }
        // The vertical min-main floor is smaller than the horizontal one (rows
        // are cheaper than columns); use a fixed two-row floor so the transcript
        // band always keeps a border + at least one row.
        let (main_h, pane_h) = solve_axis_with_floor(content.height, kind, ratio, 3)?;
        let main = Rect {
            x: content.x,
            y: content.y,
            width: content.width,
            height: main_h,
        };
        let separator = Rect {
            x: content.x,
            y: content.y + main_h,
            width: content.width,
            height: SEPARATOR,
        };
        let pane = Rect {
            x: content.x,
            y: content.y + main_h + SEPARATOR,
            width: content.width,
            height: pane_h,
        };
        Some(PanePlacement::Stacked {
            main,
            separator,
            pane,
        })
    }
}

/// Split a single axis of `total` cells into `(main, pane)` for `kind`, biased by
/// `ratio`, reserving one cell for the separator and keeping at least
/// [`PaneKind::min_main`] for the main side and one for the pane. `None` when
/// even the floor cannot fit. The horizontal solver.
fn solve_axis(total: u16, kind: PaneKind, ratio: SplitRatio) -> Option<(u16, u16)> {
    solve_axis_with_floor(total, kind, ratio, kind.min_main())
}

/// As [`solve_axis`] but with an explicit `min_main` floor, so the vertical
/// (stacked) solver can use a smaller row floor than the horizontal column
/// floor. Pure integer math; total over its inputs.
fn solve_axis_with_floor(
    total: u16,
    kind: PaneKind,
    ratio: SplitRatio,
    min_main: u16,
) -> Option<(u16, u16)> {
    // Need at least: min_main + separator + 1 (the pane's floor).
    let floor = min_main.saturating_add(SEPARATOR).saturating_add(1);
    if total < floor {
        return None;
    }
    let usable = total - SEPARATOR;
    // Base pane share from the ideal fraction, then apply the ratio bias (in
    // tenths of the usable axis). Work in i32 so the bias can go negative before
    // the clamp.
    let (num, den) = kind.ideal_fraction();
    let base = i32::from(usable) * i32::from(num) / i32::from(den);
    let bias = i32::from(usable) * ratio.bias_tenths() / 10;
    let mut pane = base + bias;
    // Clamp: the pane keeps at least one cell, and the main side keeps at least
    // `min_main`.
    let max_pane = i32::from(usable) - i32::from(min_main);
    pane = pane.clamp(1, max_pane.max(1));
    let pane_w = pane as u16;
    let main_w = usable - pane_w;
    if main_w == 0 || pane_w == 0 {
        return None;
    }
    Some((main_w, pane_w))
}

/// The text rect INSIDE a pane's rounded border (one cell of inset on every
/// side). Returns a zero-area rect when the pane is too small to hold content, so
/// callers can short-circuit without painting into the border. Mirrors
/// `diff_detail_pane::pane_inner` / `pinned_compare::pane_inner`.
pub(crate) fn pane_inner(pane: Rect) -> Rect {
    Rect {
        x: pane.x.saturating_add(1),
        y: pane.y.saturating_add(1),
        width: pane.width.saturating_sub(2),
        height: pane.height.saturating_sub(2),
    }
}

/// Whether a `(column, row)` cell falls inside `rect`. Half-open on both axes,
/// matching `ratatui::layout::Rect`. Used to route a wheel/click to the pane the
/// pointer is over. Mirrors `diff_detail_pane::rect_contains` /
/// `pinned_compare::rect_contains`.
pub(crate) fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Draw one region of the Smart Split preview diagram: a rounded border block
/// with a centered label, used for both the transcript and the pane sub-rects.
/// A no-op for a region too small to hold a border, so a tiny diagram degrades
/// cleanly. This is the one render helper in this module; it still has no
/// `TuiApp` dependency and only paints the geometry this module solves.
pub(crate) fn draw_preview_region(frame: &mut Frame<'_>, rect: Rect, label: &str, color: Color) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color));
    let body = block.inner(rect);
    frame.render_widget(block, rect);
    let inner = pane_inner(rect);
    if inner.width > 0 && inner.height > 0 {
        let truncated: String = label.chars().take(inner.width as usize).collect();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncated,
                Style::default().fg(color),
            )))
            .alignment(ratatui::layout::Alignment::Center),
            Rect {
                x: body.x,
                y: body.y + body.height / 2,
                width: body.width,
                height: 1,
            },
        );
    }
}

/// Which row of the Smart Split overlay the cursor is on. The overlay is a tiny
/// control surface: pick the pane kind, pick the orientation, and tune the split
/// ratio. ↑/↓ move between these rows; ←/→ adjust the focused row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SplitField {
    /// The secondary pane kind (Detail / Scratch / Compare).
    Kind,
    /// The orientation (Auto / Side / Stacked).
    Orientation,
    /// The split-ratio bias (narrower ↔ wider).
    Ratio,
}

impl SplitField {
    /// Every field, top-to-bottom — the overlay row order and the exhaustive set
    /// the tests sweep.
    pub(crate) const ALL: [SplitField; 3] =
        [SplitField::Kind, SplitField::Orientation, SplitField::Ratio];

    /// Friendly label for the overlay row.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SplitField::Kind => "Pane",
            SplitField::Orientation => "Orientation",
            SplitField::Ratio => "Split",
        }
    }

    /// Index of this field into [`SplitField::ALL`] — the row index a click
    /// targets.
    pub(crate) fn index(self) -> usize {
        SplitField::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }
}

/// The interactive Smart Split Panes overlay model (§12.4.2). Holds the working
/// pane kind, orientation, and split-ratio bias the user is shaping, plus a
/// cursor into [`SplitField::ALL`]. All side effects (rendering, status) live in
/// `lib.rs`; this struct is the terminal-free, fully unit-testable core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SmartSplitState {
    /// The secondary pane the solver is laying out.
    kind: PaneKind,
    /// The requested orientation (Auto resolves from aspect at solve time).
    orientation: Orientation,
    /// The user's split-ratio bias on top of the pane's ideal fraction.
    ratio: SplitRatio,
    /// Cursor into [`SplitField::ALL`]. Always in bounds (the movers clamp it).
    cursor: usize,
}

impl Default for SmartSplitState {
    fn default() -> Self {
        Self {
            kind: PaneKind::Detail,
            orientation: Orientation::Auto,
            ratio: SplitRatio::NEUTRAL,
            cursor: 0,
        }
    }
}

impl SmartSplitState {
    /// Open the overlay seeded for the given pane `kind` (the natural pane to
    /// lay out for what the user is focused on), Auto orientation, neutral ratio,
    /// cursor on the first row.
    pub(crate) fn new(kind: PaneKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// The working pane kind.
    pub(crate) fn kind(self) -> PaneKind {
        self.kind
    }

    /// The working orientation.
    pub(crate) fn orientation(self) -> Orientation {
        self.orientation
    }

    /// The working split-ratio bias.
    pub(crate) fn ratio(self) -> SplitRatio {
        self.ratio
    }

    /// The focused field. Always valid: [`SplitField::ALL`] is non-empty and
    /// `cursor` is clamped on every move.
    pub(crate) fn focused_field(self) -> SplitField {
        SplitField::ALL[self.cursor.min(SplitField::ALL.len() - 1)]
    }

    /// Index of the focused row into [`SplitField::ALL`].
    pub(crate) fn cursor(self) -> usize {
        self.cursor.min(SplitField::ALL.len() - 1)
    }

    /// Move the row focus up one (clamped at the top). Returns whether the focus
    /// moved.
    pub(crate) fn focus_prev(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Move the row focus down one (clamped at the bottom). Returns whether the
    /// focus moved.
    pub(crate) fn focus_next(&mut self) -> bool {
        if self.cursor + 1 >= SplitField::ALL.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Focus a row directly by its [`SplitField::ALL`] index (the mouse twin of
    /// ↑/↓ over a row). Out-of-range indices are ignored. Returns whether the
    /// focus moved.
    pub(crate) fn focus_row(&mut self, index: usize) -> bool {
        if index >= SplitField::ALL.len() || index == self.cursor {
            return false;
        }
        self.cursor = index;
        true
    }

    /// Adjust the focused field "forward" (the keyboard →/Space and a click on
    /// the focused row): cycle the pane kind / orientation, or widen the split.
    /// Returns whether anything changed.
    pub(crate) fn adjust_forward(&mut self) -> bool {
        match self.focused_field() {
            SplitField::Kind => {
                let next = PaneKind::ALL[(self.kind.index() + 1) % PaneKind::ALL.len()];
                let changed = next != self.kind;
                self.kind = next;
                changed
            }
            SplitField::Orientation => {
                let next = self.orientation.next();
                let changed = next != self.orientation;
                self.orientation = next;
                changed
            }
            SplitField::Ratio => self.ratio.widen(),
        }
    }

    /// Adjust the focused field "backward" (the keyboard ←): cycle the pane kind
    /// / orientation the other way, or narrow the split. Returns whether anything
    /// changed.
    pub(crate) fn adjust_backward(&mut self) -> bool {
        match self.focused_field() {
            SplitField::Kind => {
                let len = PaneKind::ALL.len();
                let prev = PaneKind::ALL[(self.kind.index() + len - 1) % len];
                let changed = prev != self.kind;
                self.kind = prev;
                changed
            }
            SplitField::Orientation => {
                let len = Orientation::ALL.len();
                let idx = self.orientation_index();
                let prev = Orientation::ALL[(idx + len - 1) % len];
                let changed = prev != self.orientation;
                self.orientation = prev;
                changed
            }
            SplitField::Ratio => self.ratio.narrow(),
        }
    }

    /// Widen the pane share by one ratio step (clamped). The mouse twin of the
    /// Split field's → / a click on the previewed pane. Returns whether the bias
    /// changed.
    pub(crate) fn ratio_widen(&mut self) -> bool {
        self.ratio.widen()
    }

    /// Narrow the pane share by one ratio step (clamped). The mouse twin of the
    /// Split field's ← / a click on the previewed main region. Returns whether the
    /// bias changed.
    pub(crate) fn ratio_narrow(&mut self) -> bool {
        self.ratio.narrow()
    }

    /// Reset the working layout to its defaults for the current pane kind (the
    /// keyboard `r`/Delete): Auto orientation, neutral ratio. Keeps the pane kind
    /// (a reset of "how to lay out", not "what to lay out"). Returns whether
    /// anything changed.
    pub(crate) fn reset(&mut self) -> bool {
        let changed = self.orientation != Orientation::Auto || self.ratio != SplitRatio::NEUTRAL;
        self.orientation = Orientation::Auto;
        self.ratio = SplitRatio::NEUTRAL;
        changed
    }

    /// Index of the working orientation into [`Orientation::ALL`].
    fn orientation_index(self) -> usize {
        Orientation::ALL
            .iter()
            .position(|o| *o == self.orientation)
            .unwrap_or(0)
    }

    /// Solve the working layout against `content` with the default thresholds —
    /// the placement the overlay previews and the renderer would use. Convenience
    /// over building a [`LayoutSolver`] at each call site.
    pub(crate) fn solve(self, content: Rect) -> PanePlacement {
        LayoutSolver::default().solve(content, self.kind, self.orientation, self.ratio)
    }
}

#[cfg(test)]
#[path = "smart_split_tests.rs"]
mod tests;
