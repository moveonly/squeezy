//! Unit tests for the pure Dockable Panels (§12.4.4) dock-state model: the
//! panel/edge enums, their slug round-trips, the single combined `cycle()` walk
//! across every panel and edge, and the layout placement (reusing the §12.4.2
//! split solver) including the left-mirror and the graceful single-column
//! degradation. All in isolation — no terminal, no `TuiApp`. The feature's
//! behaviour through the real `render()` + key/mouse dispatch is covered by the
//! capture-sink suite in `lib_tests.rs`.

use super::*;
use ratatui::layout::Rect;

fn content(width: u16, height: u16) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// DockPanel
// ---------------------------------------------------------------------------

#[test]
fn panel_all_lists_each_panel_once_in_order() {
    assert_eq!(
        DockPanel::ALL,
        &[
            DockPanel::Scratchpad,
            DockPanel::SubagentTimeline,
            DockPanel::Detail
        ]
    );
    // No duplicates.
    for (i, a) in DockPanel::ALL.iter().enumerate() {
        for b in &DockPanel::ALL[i + 1..] {
            assert_ne!(a, b, "ALL must hold each panel once");
        }
    }
}

#[test]
fn panel_slug_round_trips_for_every_panel() {
    for panel in DockPanel::ALL.iter().copied() {
        assert_eq!(
            DockPanel::from_slug(panel.as_str()),
            Some(panel),
            "{} must round-trip",
            panel.as_str()
        );
    }
}

#[test]
fn panel_from_slug_is_forgiving_and_bounded() {
    assert_eq!(
        DockPanel::from_slug("  SCRATCHPAD "),
        Some(DockPanel::Scratchpad)
    );
    assert_eq!(DockPanel::from_slug("Detail"), Some(DockPanel::Detail));
    assert_eq!(DockPanel::from_slug("nope"), None);
    assert_eq!(DockPanel::from_slug(""), None);
}

#[test]
fn panel_next_walks_every_panel_and_wraps() {
    let mut seen = Vec::new();
    let mut p = DockPanel::default();
    for _ in 0..DockPanel::ALL.len() {
        seen.push(p);
        p = p.next();
    }
    assert_eq!(p, DockPanel::default(), "one full lap returns to the start");
    for panel in DockPanel::ALL {
        assert_eq!(
            seen.iter().filter(|s| *s == panel).count(),
            1,
            "{} appears exactly once per lap",
            panel.label()
        );
    }
}

#[test]
fn panel_default_is_scratchpad() {
    assert_eq!(DockPanel::default(), DockPanel::Scratchpad);
}

#[test]
fn panel_pane_kind_maps_to_split_kinds() {
    use crate::smart_split::PaneKind;
    assert_eq!(DockPanel::Scratchpad.pane_kind(), PaneKind::Scratch);
    assert_eq!(DockPanel::SubagentTimeline.pane_kind(), PaneKind::Detail);
    assert_eq!(DockPanel::Detail.pane_kind(), PaneKind::Detail);
}

// ---------------------------------------------------------------------------
// DockEdge
// ---------------------------------------------------------------------------

#[test]
fn edge_all_lists_each_edge_once() {
    assert_eq!(
        DockEdge::ALL,
        &[DockEdge::Left, DockEdge::Right, DockEdge::Bottom]
    );
}

#[test]
fn edge_slug_round_trips_for_every_edge() {
    for edge in DockEdge::ALL.iter().copied() {
        assert_eq!(DockEdge::from_slug(edge.as_str()), Some(edge));
    }
    assert_eq!(DockEdge::from_slug("  RIGHT "), Some(DockEdge::Right));
    assert_eq!(DockEdge::from_slug("middle"), None);
}

// ---------------------------------------------------------------------------
// DockState — defaults, cycle walk, undock
// ---------------------------------------------------------------------------

#[test]
fn default_state_is_undocked_scratchpad() {
    let dock = DockState::default();
    assert_eq!(dock.panel(), DockPanel::Scratchpad);
    assert_eq!(dock.edge(), None);
    assert!(!dock.is_active(), "default dock is inactive");
    assert_eq!(dock.to_slug(), None, "an undocked dock persists nothing");
}

#[test]
fn cycle_walks_every_panel_and_edge_then_returns_to_undocked() {
    // The single combined walk: undocked -> for each panel (left,right,bottom) ->
    // undocked. Collect the sequence of (panel, edge) states one full lap produces.
    let mut dock = DockState::default();
    let total_steps = DockPanel::ALL.len() * DockEdge::ALL.len() + 1;
    let mut docked_states = Vec::new();
    for _ in 0..total_steps {
        dock.cycle();
        if let Some(edge) = dock.edge() {
            docked_states.push((dock.panel(), edge));
        }
    }
    // After the full lap we are back to undocked.
    assert!(!dock.is_active(), "a full lap returns to undocked");
    // Every (panel, edge) combination appeared exactly once.
    assert_eq!(
        docked_states.len(),
        DockPanel::ALL.len() * DockEdge::ALL.len()
    );
    for panel in DockPanel::ALL.iter().copied() {
        for edge in DockEdge::ALL.iter().copied() {
            assert_eq!(
                docked_states
                    .iter()
                    .filter(|(p, e)| *p == panel && *e == edge)
                    .count(),
                1,
                "{} on {} must appear exactly once in the walk",
                panel.label(),
                edge.label()
            );
        }
    }
}

#[test]
fn first_cycle_docks_left_then_right_then_bottom() {
    let mut dock = DockState::default();
    dock.cycle();
    assert_eq!(dock.edge(), Some(DockEdge::Left));
    dock.cycle();
    assert_eq!(dock.edge(), Some(DockEdge::Right));
    dock.cycle();
    assert_eq!(dock.edge(), Some(DockEdge::Bottom));
    // Still the same panel through its three edges.
    assert_eq!(dock.panel(), DockPanel::Scratchpad);
}

#[test]
fn undock_clears_edge_keeps_panel() {
    let mut dock = DockState::default();
    dock.cycle(); // scratchpad:left
    dock.cycle_panel(); // detail/subagents target, same edge
    let panel = dock.panel();
    dock.undock();
    assert_eq!(dock.edge(), None);
    assert_eq!(dock.panel(), panel, "undock leaves the target panel intact");
}

// ---------------------------------------------------------------------------
// DockState — slug round-trip + describe
// ---------------------------------------------------------------------------

#[test]
fn state_slug_round_trips_for_every_panel_edge() {
    for panel in DockPanel::ALL.iter().copied() {
        for edge in DockEdge::ALL.iter().copied() {
            // Build the state directly via the slug to avoid depending on the
            // cycle order.
            let slug = format!("{}:{}", panel.as_str(), edge.as_str());
            let dock = DockState::from_slug(&slug).expect("valid slug parses");
            assert_eq!(dock.panel(), panel);
            assert_eq!(dock.edge(), Some(edge));
            assert_eq!(dock.to_slug().as_deref(), Some(slug.as_str()));
        }
    }
}

#[test]
fn state_from_slug_rejects_malformed_and_undocked() {
    assert_eq!(
        DockState::from_slug("scratchpad"),
        None,
        "bare panel has no edge"
    );
    assert_eq!(DockState::from_slug("scratchpad:"), None);
    assert_eq!(DockState::from_slug(":left"), None);
    assert_eq!(DockState::from_slug("nope:left"), None);
    assert_eq!(DockState::from_slug("scratchpad:middle"), None);
    assert_eq!(DockState::from_slug(""), None);
}

#[test]
fn describe_names_panel_and_edge_or_undocked() {
    let dock = DockState::default();
    assert_eq!(dock.describe(), "scratchpad undocked");
    let docked = DockState::from_slug("subagents:bottom").unwrap();
    assert_eq!(docked.describe(), "subagents docked bottom");
}

// ---------------------------------------------------------------------------
// DockState::placement — split geometry, left mirror, degradation
// ---------------------------------------------------------------------------

#[test]
fn inactive_dock_yields_full_content_single_column() {
    let dock = DockState::default();
    let area = content(120, 40);
    let placed = dock.placement(area);
    assert_eq!(
        placed.main(),
        area,
        "an inactive dock hands the whole area to main"
    );
    assert_eq!(placed.panel(), None);
    assert_eq!(placed.separator(), None);
    assert!(!placed.is_docked());
}

#[test]
fn right_dock_puts_panel_right_of_a_smaller_main() {
    let dock = DockState::from_slug("scratchpad:right").unwrap();
    let area = content(120, 40);
    let placed = dock.placement(area);
    let main = placed.main();
    let panel = placed.panel().expect("wide terminal splits side-by-side");
    assert!(placed.is_docked());
    assert_eq!(placed.edge(), DockEdge::Right);
    // Main is the LEFT column, the panel is to its right with a separator between.
    assert_eq!(main.x, area.x, "main keeps the left edge on a right dock");
    assert!(
        panel.x > main.x + main.width,
        "panel sits right of main + separator"
    );
    assert!(
        main.width < area.width,
        "main is reduced by the docked panel"
    );
    assert_eq!(main.height, area.height, "a side dock keeps full height");
    assert_eq!(panel.height, area.height);
}

#[test]
fn left_dock_mirrors_right_panel_on_the_left() {
    let dock = DockState::from_slug("scratchpad:left").unwrap();
    let area = content(120, 40);
    let placed = dock.placement(area);
    let main = placed.main();
    let panel = placed.panel().expect("wide terminal splits side-by-side");
    assert_eq!(placed.edge(), DockEdge::Left);
    // The PANEL takes the left edge, the (reduced) main sits to its right.
    assert_eq!(panel.x, area.x, "panel keeps the left edge on a left dock");
    assert!(
        main.x > panel.x + panel.width,
        "main sits right of the panel + separator"
    );
    assert!(main.width < area.width);
}

#[test]
fn bottom_dock_stacks_panel_below_main() {
    let dock = DockState::from_slug("subagents:bottom").unwrap();
    let area = content(120, 40);
    let placed = dock.placement(area);
    let main = placed.main();
    let panel = placed.panel().expect("tall terminal stacks");
    assert_eq!(placed.edge(), DockEdge::Bottom);
    assert_eq!(main.y, area.y, "main keeps the top on a bottom dock");
    assert!(
        panel.y > main.y + main.height,
        "panel sits below main + separator"
    );
    assert_eq!(main.width, area.width, "a stacked dock keeps full width");
    assert!(
        main.height < area.height,
        "main is reduced by the docked panel"
    );
}

#[test]
fn tiny_terminal_degrades_to_single_column_gracefully() {
    // Far too small for any split — the solver returns single column, so the dock
    // hands the whole content to main and paints no panel.
    let dock = DockState::from_slug("detail:right").unwrap();
    let area = content(10, 4);
    let placed = dock.placement(area);
    assert_eq!(placed.main(), area);
    assert_eq!(placed.panel(), None, "no room to split -> no panel");
    assert!(!placed.is_docked());
}

#[test]
fn placement_rects_stay_inside_the_content_area() {
    // Sweep panels x edges x a few sizes; every produced rect must stay within the
    // content area and never overlap main with the panel.
    for panel in DockPanel::ALL.iter().copied() {
        for edge in DockEdge::ALL.iter().copied() {
            for (w, h) in [(120u16, 40u16), (80, 24), (200, 60)] {
                let slug = format!("{}:{}", panel.as_str(), edge.as_str());
                let dock = DockState::from_slug(&slug).unwrap();
                let area = content(w, h);
                let placed = dock.placement(area);
                let main = placed.main();
                assert!(main.x + main.width <= area.x + area.width);
                assert!(main.y + main.height <= area.y + area.height);
                if let Some(pane) = placed.panel() {
                    assert!(pane.x + pane.width <= area.x + area.width);
                    assert!(pane.y + pane.height <= area.y + area.height);
                    assert!(pane.width > 0 && pane.height > 0);
                    // Main and panel must not overlap on the split axis.
                    let disjoint_x = main.x + main.width <= pane.x || pane.x + pane.width <= main.x;
                    let disjoint_y =
                        main.y + main.height <= pane.y || pane.y + pane.height <= main.y;
                    assert!(
                        disjoint_x || disjoint_y,
                        "main and panel overlap for {slug} at {w}x{h}"
                    );
                }
            }
        }
    }
}
