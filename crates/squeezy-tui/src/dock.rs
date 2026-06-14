//! Dockable Panels (§12.4.4): pin an auxiliary panel (the scratchpad, the
//! subagent timeline, or the focused-entry detail) to a side of the main view —
//! left, right, or bottom — and persist that choice across restarts.
//!
//! **One dock-state model.** [`DockState`] is the single source of truth: which
//! [`DockPanel`] is the dock target and which [`DockEdge`] it occupies (or none —
//! undocked). The renderer reads [`DockState::placement`] to carve the panel rect
//! off the main content, and a dispatch/status read reads the same model, so the
//! painted layout and any off-frame readout never disagree.
//!
//! **Reuses the split-pane machinery.** A dock edge is just a constrained call
//! into the §12.4.2 [`crate::smart_split`] layout solver: `Right`/`Left` force a
//! side-by-side split (with the panel on the right or, mirrored, on the left) and
//! `Bottom` forces a stacked split. The solver already enforces the
//! transcript/composer minimums and degrades to a single column when the terminal
//! is too small, so a dock inherits that "collapse gracefully when minimums fail"
//! behaviour for free rather than re-deriving its own thresholds.
//!
//! **Keyboard + mouse, one handler.** The crate root binds a key that cycles the
//! active panel's edge (`undocked → left → right → bottom → undocked`) and a click
//! on the docked panel's header routes to the *same* cycle, so every mouse
//! affordance has a keyboard twin by construction.
//!
//! **Persisted, zero idle cost.** The pick round-trips through a bounded
//! `panel:edge` slug at `[tui].dock`, restored before the first paint. Every
//! method here is pure, allocation-light arithmetic with no clock and no I/O; an
//! idle session that paints nothing resolves a placement zero times, so the
//! feature adds no idle redraw and no background work.

use ratatui::layout::Rect;

use crate::smart_split::{
    LayoutSolver, Orientation, PaneKind, PanePlacement, SEPARATOR, SplitRatio,
};

/// Which auxiliary panel is the dock target (§12.4.4). The spec calls out the
/// scratchpad, the subagent timeline, and the focused-entry detail as the
/// dockable auxiliaries; this enum is the closed set the dock model and the
/// persistence round-trip sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum DockPanel {
    /// The §12.3.3 scratchpad notes buffer, pinned as a side/bottom panel rather
    /// than its fullscreen overlay. The default dock target (a session opens with
    /// the scratchpad as the candidate panel, but undocked).
    #[default]
    Scratchpad,
    /// The §12.8.1 subagent timeline, pinned as a side/bottom panel so the
    /// delegate roster stays visible beside the transcript.
    SubagentTimeline,
    /// The focused-entry detail (the §11G.10 diff / detail body) pinned as a
    /// side/bottom panel.
    Detail,
}

impl DockPanel {
    /// Every panel, in a stable cycle order — the panel-switch order and the
    /// exhaustive set the tests sweep. A new variant must be added here or it is
    /// invisible to the cycle and the persistence round-trip.
    pub(crate) const ALL: &'static [DockPanel] = &[
        DockPanel::Scratchpad,
        DockPanel::SubagentTimeline,
        DockPanel::Detail,
    ];

    /// The bounded slug persisted in the `[tui].dock` value. Stable wire form;
    /// keep in sync with [`DockPanel::from_slug`].
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DockPanel::Scratchpad => "scratchpad",
            DockPanel::SubagentTimeline => "subagents",
            DockPanel::Detail => "detail",
        }
    }

    /// Short human label for the dock header / status readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DockPanel::Scratchpad => "scratchpad",
            DockPanel::SubagentTimeline => "subagents",
            DockPanel::Detail => "detail",
        }
    }

    /// Parse a persisted panel slug. Unknown / absent slugs collapse to `None` so
    /// the caller keeps the built-in default. Case-insensitive on the ASCII slug
    /// so a hand-edited config is forgiving.
    pub(crate) fn from_slug(slug: &str) -> Option<DockPanel> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "scratchpad" => Some(DockPanel::Scratchpad),
            "subagents" => Some(DockPanel::SubagentTimeline),
            "detail" => Some(DockPanel::Detail),
            _ => None,
        }
    }

    /// The next panel in the cycle (wraps). Used by a panel-switch affordance to
    /// retarget the dock without changing the edge.
    pub(crate) fn next(self) -> DockPanel {
        let all = DockPanel::ALL;
        let idx = all.iter().position(|p| *p == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    /// The §12.4.2 pane kind the dock uses when it routes through the layout
    /// solver. The scratchpad maps to the slim [`PaneKind::Scratch`] sidebar; the
    /// subagent timeline and the detail body both want the generous
    /// [`PaneKind::Detail`] share (a roster / a wrapped diff each read better with
    /// room). Keeping this here means the dock never re-derives split thresholds —
    /// it borrows the pane kind's `ideal_fraction` / `min_main` table.
    pub(crate) fn pane_kind(self) -> PaneKind {
        match self {
            DockPanel::Scratchpad => PaneKind::Scratch,
            DockPanel::SubagentTimeline | DockPanel::Detail => PaneKind::Detail,
        }
    }
}

/// Which edge the panel is pinned to (§12.4.4). `Left`/`Right` are side-by-side
/// splits (the panel column on that side of the transcript); `Bottom` is a
/// stacked split (the panel band below the transcript).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DockEdge {
    /// Panel pinned to the left of the transcript (a mirrored side split).
    Left,
    /// Panel pinned to the right of the transcript (the natural side split).
    Right,
    /// Panel pinned below the transcript (a stacked split).
    Bottom,
}

impl DockEdge {
    /// Every edge, in the dock cycle order `left → right → bottom`. The cycle in
    /// [`DockState::cycle`] walks `undocked → left → right → bottom → undocked`;
    /// this is the docked portion of that walk and the exhaustive set the tests
    /// sweep.
    pub(crate) const ALL: &'static [DockEdge] =
        &[DockEdge::Left, DockEdge::Right, DockEdge::Bottom];

    /// The bounded slug persisted in the `[tui].dock` value (after the `:`). Keep
    /// in sync with [`DockEdge::from_slug`].
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DockEdge::Left => "left",
            DockEdge::Right => "right",
            DockEdge::Bottom => "bottom",
        }
    }

    /// Short human label for the dock header / status readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DockEdge::Left => "left",
            DockEdge::Right => "right",
            DockEdge::Bottom => "bottom",
        }
    }

    /// Parse a persisted edge slug. Unknown / absent slugs collapse to `None`
    /// (the panel stays undocked). Case-insensitive on the ASCII slug.
    pub(crate) fn from_slug(slug: &str) -> Option<DockEdge> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "left" => Some(DockEdge::Left),
            "right" => Some(DockEdge::Right),
            "bottom" => Some(DockEdge::Bottom),
            _ => None,
        }
    }

    /// The §12.4.2 orientation this edge forces in the layout solver: the side
    /// edges force a side-by-side split, the bottom edge forces a stacked one.
    fn orientation(self) -> Orientation {
        match self {
            DockEdge::Left | DockEdge::Right => Orientation::Side,
            DockEdge::Bottom => Orientation::Stacked,
        }
    }

    /// Whether the panel sits *before* the transcript on the split axis (a left
    /// dock), so the placement reflects the solver's `main`/`pane` geometry about
    /// the content (the small pane keeps its share on the leading edge). The
    /// solver always returns the transcript first (left / top); `Left` is the only
    /// edge that mirrors it.
    fn panel_first(self) -> bool {
        matches!(self, DockEdge::Left)
    }
}

/// Where the dock split placed the panel relative to the transcript. `main` is
/// always the transcript's reduced rect; `panel` is the docked panel's rect;
/// `separator` is the one-cell rule between them. The solver may degrade to a
/// single column (no room to split) — in that case [`DockPlacement::panel`] is
/// `None` and the caller leaves the panel undrawn for the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DockPlacement {
    /// The transcript's rect after the panel is carved off.
    main: Rect,
    /// The docked panel's rect, or `None` when the terminal was too small to
    /// split (graceful degradation — the transcript keeps the whole content).
    panel: Option<Rect>,
    /// The one-cell separator rect, or `None` when there was no split.
    separator: Option<Rect>,
    /// The edge the panel was pinned to (carried so the renderer can title the
    /// header / orient the body without re-reading the dock state).
    edge: DockEdge,
}

impl DockPlacement {
    /// The transcript's rect — the area the main-view layout is given. Equals the
    /// full content rect when the layout degraded to a single column.
    pub(crate) fn main(self) -> Rect {
        self.main
    }

    /// The docked panel's rect, or `None` when there was no room to split (the
    /// caller skips painting the panel this frame).
    pub(crate) fn panel(self) -> Option<Rect> {
        self.panel
    }

    /// The separator rect, or `None` for a single column (no split, no rule).
    pub(crate) fn separator(self) -> Option<Rect> {
        self.separator
    }

    /// The edge the panel was pinned to.
    pub(crate) fn edge(self) -> DockEdge {
        self.edge
    }

    /// Whether the dock actually carved a panel off (vs. degrading to a single
    /// transcript column). Drives the "is the dock visible this frame?" decision.
    pub(crate) fn is_docked(self) -> bool {
        self.panel.is_some()
    }
}

/// The Dockable Panels (§12.4.4) state: which auxiliary panel is the dock target
/// and which edge it occupies (or `None` — the panel is undocked and the dock is
/// inert). One small value the crate root owns on `TuiApp`; persisted to
/// `[tui].dock` so a pick survives a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DockState {
    /// The auxiliary panel the dock targets. Always set (defaults to the
    /// scratchpad); only `edge` decides whether the dock is *active*.
    panel: DockPanel,
    /// The edge the panel is pinned to, or `None` when undocked (the dock paints
    /// nothing and the transcript keeps the whole content).
    edge: Option<DockEdge>,
}

impl DockState {
    /// The dock target panel (always set; defaults to the scratchpad).
    pub(crate) fn panel(self) -> DockPanel {
        self.panel
    }

    /// The pinned edge, or `None` when the dock is inactive (undocked).
    pub(crate) fn edge(self) -> Option<DockEdge> {
        self.edge
    }

    /// Whether the dock is active (a panel is pinned to an edge). A `false`
    /// result means the renderer hands the whole content to the transcript and
    /// paints no panel, so a session that never docks is byte-identical to one
    /// without the feature.
    pub(crate) fn is_active(self) -> bool {
        self.edge.is_some()
    }

    /// Advance the dock one step along the single cycle that walks *every*
    /// `(panel, edge)` combination and the undocked rest, in this order:
    ///
    /// ```text
    /// undocked → scratchpad:left → scratchpad:right → scratchpad:bottom
    ///          → subagents:left  → …                → detail:bottom
    ///          → undocked …
    /// ```
    ///
    /// So one verb (`Ctrl+Alt+F`) / one header click reaches every panel on every
    /// edge and returns to undocked — keyboard/mouse parity holds because both
    /// route here. The edge advances first; only when it wraps past `Bottom` does
    /// the target panel advance, and only when the *last* panel wraps does the
    /// dock return to undocked.
    pub(crate) fn cycle(&mut self) {
        let edges = DockEdge::ALL;
        let first = *edges.first().unwrap_or(&DockEdge::Left);
        match self.edge {
            // Undocked: dock the current panel to the first edge.
            None => self.edge = Some(first),
            Some(current) => {
                // Advance along the edge order; when we step past the last edge,
                // move to the next panel's first edge, or undock once the last
                // panel has been walked.
                let idx = edges.iter().position(|e| *e == current).unwrap_or(0);
                if idx + 1 < edges.len() {
                    self.edge = Some(edges[idx + 1]);
                } else if self.panel == *DockPanel::ALL.last().unwrap_or(&DockPanel::Scratchpad) {
                    self.undock();
                } else {
                    self.cycle_panel();
                    self.edge = Some(first);
                }
            }
        }
    }

    /// Retarget the dock to the next panel (wraps), keeping the current edge. The
    /// per-panel step [`DockState::cycle`] uses to walk from one auxiliary to the
    /// next without first undocking.
    pub(crate) fn cycle_panel(&mut self) {
        self.panel = self.panel.next();
    }

    /// Undock the panel (clear the edge) without changing the target panel — the
    /// terminus of the [`DockState::cycle`] walk (and the `Esc`/close path).
    pub(crate) fn undock(&mut self) {
        self.edge = None;
    }

    /// Resolve the dock against a content `area` into a [`DockPlacement`]. Routes
    /// through the §12.4.2 [`LayoutSolver`] with the edge's forced orientation and
    /// the panel's pane kind, so the dock inherits the solver's minimum-size
    /// enforcement and graceful single-column fallback. Returns the full-content
    /// single column (panel `None`) when the dock is inactive *or* the terminal is
    /// too small to split — the renderer treats both the same way (no panel
    /// painted).
    pub(crate) fn placement(self, area: Rect) -> DockPlacement {
        let Some(edge) = self.edge else {
            return DockPlacement {
                main: area,
                panel: None,
                separator: None,
                // An inactive dock has no real edge; report Right as an inert
                // placeholder — `panel()` is `None`, so this is never painted.
                edge: DockEdge::Right,
            };
        };
        let solver = LayoutSolver::default();
        let placement = solver.solve(
            area,
            self.panel.pane_kind(),
            edge.orientation(),
            SplitRatio::NEUTRAL,
        );
        // The solver falls back to the opposite orientation when the requested one
        // doesn't fit (a Right dock on a narrow/tall terminal stacks; a Bottom dock
        // on a wide/short one side-splits). Report the edge the panel was ACTUALLY
        // placed on so the header label and body orientation match the geometry,
        // never the requested edge the solver overrode. A side edge that came back
        // stacked is reported as Bottom; a bottom edge that came back side-by-side
        // is reported as Right (the natural transcript-left side split). A single
        // column keeps the requested edge — nothing is painted.
        let realized_edge = match placement {
            PanePlacement::Side { .. } if edge.orientation() == Orientation::Stacked => {
                DockEdge::Right
            }
            PanePlacement::Stacked { .. } if edge.orientation() == Orientation::Side => {
                DockEdge::Bottom
            }
            _ => edge,
        };
        // The solver always returns the transcript first (left column / top band)
        // with the larger share. A `Left` dock that actually side-splits mirrors
        // that geometry — not the rect *assignments* — so the small pane keeps its
        // share on the leading edge: reflect the pane/main/separator x-positions
        // about the content, keeping each rect's solver-chosen width. The
        // single-column fallback (no pane) carries the solver placement through
        // unchanged, as does any orientation the solver flipped (its geometry
        // already matches `realized_edge`).
        let (main, panel, separator) = match (placement.main(), placement.pane()) {
            (transcript, Some(pane)) if realized_edge.panel_first() => {
                let panel = Rect {
                    x: area.x,
                    width: pane.width,
                    ..pane
                };
                let separator = Rect {
                    x: area.x + pane.width,
                    width: SEPARATOR,
                    ..pane
                };
                let main = Rect {
                    x: area.x + pane.width + SEPARATOR,
                    width: transcript.width,
                    ..transcript
                };
                (main, Some(panel), Some(separator))
            }
            (transcript, pane) => (transcript, pane, placement.separator()),
        };
        DockPlacement {
            main,
            panel,
            separator,
            edge: realized_edge,
        }
    }

    /// The bounded `panel:edge` slug persisted at `[tui].dock`, or `None` when the
    /// dock is inactive (nothing is persisted; a restored session keeps the
    /// built-in undocked default). E.g. `scratchpad:right`.
    pub(crate) fn to_slug(self) -> Option<String> {
        let edge = self.edge?;
        Some(format!("{}:{}", self.panel.as_str(), edge.as_str()))
    }

    /// Parse a persisted `panel:edge` slug into a dock state. A missing edge half,
    /// an unknown panel/edge, or a malformed string collapses to `None` so the
    /// session keeps the undocked default. A bare `panel` with no `:edge` is
    /// rejected (an active dock always names an edge), keeping the wire form
    /// unambiguous.
    pub(crate) fn from_slug(slug: &str) -> Option<DockState> {
        let (panel_part, edge_part) = slug.split_once(':')?;
        let panel = DockPanel::from_slug(panel_part)?;
        let edge = DockEdge::from_slug(edge_part)?;
        Some(DockState {
            panel,
            edge: Some(edge),
        })
    }

    /// A short, screen-reader-friendly readout of the dock for the status line /
    /// cycle toast: `panel docked left/right/bottom`, or `panel undocked` when
    /// inactive.
    pub(crate) fn describe(self) -> String {
        match self.edge {
            Some(edge) => format!("{} docked {}", self.panel.label(), edge.label()),
            None => format!("{} undocked", self.panel.label()),
        }
    }
}

#[cfg(test)]
#[path = "dock_tests.rs"]
mod tests;
