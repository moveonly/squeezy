use ratatui::style::Color;

use super::*;

/// A representative spread of geometries: the classic 80x24, a wide layout, and
/// a deliberately tiny one that still has to paint a composer.
const SIZES: &[GridSize] = &[
    GridSize::new(80, 24),
    GridSize::new(160, 48),
    GridSize::new(40, 10),
];

// ---------------------------------------------------------------------------
// Scenario metadata — slugs/titles are stable and unique
// ---------------------------------------------------------------------------

#[test]
fn scenario_slugs_are_unique_and_stable() {
    let mut slugs: Vec<&str> = VisualScenario::ALL.iter().map(|s| s.slug()).collect();
    let count = slugs.len();
    slugs.sort_unstable();
    slugs.dedup();
    assert_eq!(slugs.len(), count, "scenario slugs must be unique");
    assert_eq!(count, 8);
}

#[test]
fn anomaly_kind_labels_are_unique() {
    let kinds = [
        AnomalyKind::ClippedText,
        AnomalyKind::Overlap,
        AnomalyKind::StaleCell,
        AnomalyKind::DuplicatedComposer,
        AnomalyKind::MissingFocus,
        AnomalyKind::BadColor,
        AnomalyKind::ResizeArtifact,
    ];
    let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    let count = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), count, "anomaly labels must be unique");
}

// ---------------------------------------------------------------------------
// Capture — the integration path: drives the real render() over a TestBackend
// ---------------------------------------------------------------------------

#[test]
fn capture_drives_real_render_into_a_full_grid() {
    // The integration test of record: a scenario captured through the real
    // `render()` must fill exactly the viewport with real cells.
    let size = GridSize::new(80, 24);
    let grid = FrameGrid::capture(VisualScenario::ShortChat, size, "default");
    assert_eq!(grid.cells.len(), size.width as usize * size.height as usize);
    assert_eq!(grid.scenario, VisualScenario::ShortChat);
    assert_eq!(grid.size, size);
    // The short chat's prose must actually be painted somewhere on screen.
    let text = grid.plain_text();
    assert!(
        text.contains("Clamp the offset"),
        "rendered grid must contain the assistant prose:\n{text}"
    );
    // And the empty-composer caret is painted exactly once (focus present).
    assert_eq!(count_caret(&grid), 1, "exactly one composer caret expected");
}

#[test]
fn empty_scenario_paints_a_lone_composer_caret() {
    // Edge case: an empty transcript still paints exactly one composer caret at
    // every size — the minimum frame. The *structural* anomalies (focus,
    // duplicated composer, stale cell, resize) must never fire on a clean
    // identity frame. The content heuristics (clip/overlap/bad-color) may
    // legitimately fire on real rendered prose (the startup card), so they are
    // not asserted absent here — `every_scenario_renders_cleanly_across_a_resize_sweep`
    // pins the structural invariants across the suite.
    for &size in SIZES {
        let grid = FrameGrid::capture(VisualScenario::Empty, size, "default");
        assert_eq!(
            count_caret(&grid),
            1,
            "empty scenario at {}x{} must paint one caret",
            size.width,
            size.height
        );
        let report = ScenarioReport::from_grids(grid.clone(), grid);
        assert_eq!(report.diff.changed_cells, 0, "identity diff is empty");
        let structural = [
            AnomalyKind::MissingFocus,
            AnomalyKind::DuplicatedComposer,
            AnomalyKind::StaleCell,
            AnomalyKind::ResizeArtifact,
        ];
        for kind in structural {
            assert!(
                !report.anomalies.iter().any(|a| a.kind == kind),
                "a clean empty identity frame must not flag {:?} at {}x{}, got {:?}",
                kind,
                size.width,
                size.height,
                report.anomalies
            );
        }
    }
}

#[test]
fn composer_text_scenario_keeps_a_single_caret() {
    // A non-empty composer still paints exactly one caret (focus present, no
    // duplicate). Proves the focus check survives typed text.
    let grid = FrameGrid::capture(
        VisualScenario::ComposerText,
        GridSize::new(80, 24),
        "default",
    );
    assert_eq!(count_caret(&grid), 1);
    let text = grid.plain_text();
    assert!(
        text.contains("still typing"),
        "composer text must be painted:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Diff — one-cell / moved / clipped / stale fixtures (spec verify list)
// ---------------------------------------------------------------------------

/// Build a tiny solid grid of a single repeated glyph for fixture diffs.
fn solid_grid(size: GridSize, symbol: &str, fg: Color) -> FrameGrid {
    let cell = GridCell {
        symbol: symbol.to_string(),
        fg,
        bg: Color::Reset,
        bold: false,
    };
    FrameGrid {
        scenario: VisualScenario::Empty,
        size,
        theme: "fixture",
        cells: vec![cell; size.width as usize * size.height as usize],
    }
}

#[test]
fn identical_grids_diff_to_zero_changes() {
    let size = GridSize::new(8, 4);
    let g = solid_grid(size, "a", Color::Gray);
    let diff = GridDiff::compute(&g, &g);
    assert_eq!(diff.changed_cells, 0);
    assert!(diff.changes.iter().all(|c| *c == CellChange::Same));
}

#[test]
fn one_cell_change_is_pinpointed() {
    // Fixture: change exactly one cell's glyph; the diff must report exactly one
    // changed cell, classified Glyph, at the right position.
    let size = GridSize::new(8, 4);
    let base = solid_grid(size, "a", Color::Gray);
    let mut current = base.clone();
    let idx = (2 * size.width + 3) as usize; // (x=3, y=2)
    current.cells[idx].symbol = "b".to_string();

    let diff = GridDiff::compute(&base, &current);
    assert_eq!(diff.changed_cells, 1, "exactly one cell changed");
    assert_eq!(diff.change(3, 2), CellChange::Glyph);
    assert_eq!(diff.change(0, 0), CellChange::Same);
}

#[test]
fn style_only_change_is_classified_as_style() {
    // Same glyph, different color → Style, not Glyph.
    let size = GridSize::new(4, 2);
    let base = solid_grid(size, "x", Color::Gray);
    let mut current = base.clone();
    current.cells[0].fg = Color::Rgb(10, 20, 30);

    let diff = GridDiff::compute(&base, &current);
    assert_eq!(diff.changed_cells, 1);
    assert_eq!(diff.change(0, 0), CellChange::Style);
}

#[test]
fn appeared_and_vanished_cells_are_distinguished() {
    // A blank→glyph cell is Appeared; a glyph→blank cell is Vanished.
    let size = GridSize::new(4, 1);
    let mut base = solid_grid(size, " ", Color::Reset);
    base.cells[3].symbol = "z".to_string(); // baseline has a glyph at (3,0)
    let mut current = solid_grid(size, " ", Color::Reset);
    current.cells[0].symbol = "y".to_string(); // current has a glyph at (0,0)

    let diff = GridDiff::compute(&base, &current);
    assert_eq!(diff.change(0, 0), CellChange::Appeared);
    assert_eq!(diff.change(3, 0), CellChange::Vanished);
    assert_eq!(diff.changed_cells, 2);
}

#[test]
fn scrolling_a_long_session_moves_the_view() {
    // "Moved" fixture through the real renderer: the long session before vs
    // after scrolling must produce a non-trivial diff — the canonical
    // resize/scroll artifact the dashboard surfaces.
    let report = compare_scenarios(
        VisualScenario::LongSession,
        VisualScenario::ScrolledLongSession,
        GridSize::new(80, 24),
        "default",
    );
    assert!(
        report.diff.changed_cells > 0,
        "scrolling must change the rendered grid"
    );
}

// ---------------------------------------------------------------------------
// Anomaly detection — every defect class the spec enumerates
// ---------------------------------------------------------------------------

#[test]
fn missing_caret_flags_missing_focus() {
    // A grid with no composer caret → MissingFocus.
    let size = GridSize::new(8, 4);
    let grid = solid_grid(size, "a", Color::Gray);
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::MissingFocus),
        "a caret-less grid must flag missing focus: {:?}",
        report.anomalies
    );
}

#[test]
fn two_carets_flag_a_duplicated_composer() {
    let size = GridSize::new(8, 2);
    let mut grid = solid_grid(size, "a", Color::Gray);
    grid.cells[0].symbol = "┃".to_string();
    grid.cells[1].symbol = "┃".to_string();
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::DuplicatedComposer),
        "two carets must flag a duplicated composer: {:?}",
        report.anomalies
    );
}

#[test]
fn mid_word_right_edge_flags_clipped_text() {
    // A row whose last two columns are letters reads as clipped text.
    let size = GridSize::new(4, 1);
    let mut grid = solid_grid(size, " ", Color::Reset);
    grid.cells[0].symbol = "┃".to_string(); // keep focus present so only clip fires
    grid.cells[2].symbol = "o".to_string();
    grid.cells[3].symbol = "p".to_string();
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::ClippedText),
        "mid-word right edge must flag clipped text: {:?}",
        report.anomalies
    );
}

#[test]
fn wide_glyph_clobber_flags_overlap() {
    // A wide glyph whose trailing cell still carries a non-blank symbol is an
    // overlap. ratatui blanks the trailing half; a populated one is a clobber.
    let size = GridSize::new(4, 1);
    let mut grid = solid_grid(size, " ", Color::Reset);
    grid.cells[0].symbol = "┃".to_string(); // focus present
    grid.cells[1].symbol = "世".to_string(); // wide
    grid.cells[2].symbol = "x".to_string(); // should have been blanked
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::Overlap),
        "a wide-glyph clobber must flag overlap: {:?}",
        report.anomalies
    );
}

#[test]
fn too_bright_fg_flags_bad_color() {
    let size = GridSize::new(4, 1);
    let mut grid = solid_grid(size, " ", Color::Reset);
    grid.cells[0].symbol = "┃".to_string();
    grid.cells[2].symbol = "X".to_string();
    grid.cells[2].fg = Color::Rgb(255, 255, 255); // luminance 255 > 200
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::BadColor),
        "a too-bright fg must flag bad color: {:?}",
        report.anomalies
    );
}

#[test]
fn vanished_cell_flags_stale() {
    // A cell that goes blank vs baseline is "stale" — content that on a real
    // terminal must be actively cleared, not left.
    let size = GridSize::new(4, 1);
    let mut base = solid_grid(size, " ", Color::Reset);
    base.cells[0].symbol = "┃".to_string();
    base.cells[2].symbol = "z".to_string();
    let mut current = base.clone();
    current.cells[2].symbol = " ".to_string(); // dropped to blank
    let report = ScenarioReport::from_grids(base, current);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::StaleCell),
        "a vanished cell must flag stale: {:?}",
        report.anomalies
    );
}

#[test]
fn geometry_mismatch_flags_resize_artifact() {
    // Comparing two different geometries is a resize; the dashboard surfaces it.
    let mut base = solid_grid(GridSize::new(6, 2), "a", Color::Gray);
    base.cells[0].symbol = "┃".to_string();
    let mut current = solid_grid(GridSize::new(8, 2), "a", Color::Gray);
    current.cells[0].symbol = "┃".to_string();
    let report = ScenarioReport::from_grids(base, current);
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::ResizeArtifact),
        "a geometry mismatch must flag a resize artifact: {:?}",
        report.anomalies
    );
}

// ---------------------------------------------------------------------------
// Real renderer output stays clean across a resize sweep
// ---------------------------------------------------------------------------

#[test]
fn every_scenario_renders_cleanly_across_a_resize_sweep() {
    // Resize coverage: capture every scenario at three very different
    // geometries and assert the renderer never duplicates the composer or
    // loses focus. (Clip/overlap/bad-color heuristics may legitimately fire on
    // real prose, so we assert the structural focus invariants only.)
    for &scenario in &VisualScenario::ALL {
        for &size in SIZES {
            let grid = FrameGrid::capture(scenario, size, "default");
            let caret = count_caret(&grid);
            // A fullscreen overlay surface (diff overlay / review board) takes
            // over the screen and paints no main-view composer; the main-view
            // scenarios must paint exactly one.
            let expected = if scenario.is_overlay() { 0 } else { 1 };
            assert_eq!(
                caret,
                expected,
                "{} at {}x{} must paint {expected} composer caret(s), got {caret}",
                scenario.slug(),
                size.width,
                size.height
            );
        }
    }
}

#[test]
fn resize_between_states_produces_a_real_diff() {
    // The feature paints across sizes: capture the same scenario at two
    // geometries and confirm both the diff and the resize anomaly fire.
    let report = ScenarioReport::capture(
        VisualScenario::ShortChat,
        GridSize::new(80, 24),
        GridSize::new(100, 30),
        "default",
    );
    assert!(report.diff.changed_cells > 0, "a resize changes the grid");
    assert!(
        report
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::ResizeArtifact),
        "a resize between states flags a resize artifact"
    );
}

// ---------------------------------------------------------------------------
// HTML dashboard artifact — snapshot-style structural assertions
// ---------------------------------------------------------------------------

#[test]
fn dashboard_html_is_well_formed_and_lists_every_scenario() {
    let reports = default_reports(GridSize::new(80, 24));
    let html = render_dashboard_html(&reports);

    // Structural skeleton.
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "must be a full HTML doc"
    );
    assert!(html.contains("Visual Diff Dashboard"), "has a title");
    assert!(html.ends_with("</html>\n"), "must close the document");
    // The filter affordance the spec asks for.
    assert!(html.contains("changed-only"), "has a changed-only filter");
    // Every scenario panel is present and the three grids are labeled.
    for scenario in VisualScenario::ALL {
        assert!(
            html.contains(&format!("id=\"{}\"", scenario.slug()))
                || html.matches(scenario.title()).count() >= 1,
            "dashboard must include the {} scenario",
            scenario.slug()
        );
    }
    assert!(html.contains("<figcaption>baseline</figcaption>"));
    assert!(html.contains("<figcaption>current</figcaption>"));
    assert!(html.contains("<figcaption>diff</figcaption>"));
}

// ---------------------------------------------------------------------------
// VCS surfaces — diff overlay / review board / diff detail pane
// ---------------------------------------------------------------------------

#[test]
fn vcs_scenarios_are_swept_by_default_reports() {
    let reports = default_reports(GridSize::new(120, 40));
    let captured: Vec<VisualScenario> = reports.iter().map(|r| r.current.scenario).collect();
    for scenario in [
        VisualScenario::DiffOverlay,
        VisualScenario::ReviewBoard,
        VisualScenario::DiffDetailPane,
    ] {
        assert!(
            captured.contains(&scenario),
            "default_reports must sweep the {} VCS scenario",
            scenario.slug()
        );
    }
}

#[test]
fn diff_overlay_scenario_paints_the_diff_body() {
    let grid = FrameGrid::capture(
        VisualScenario::DiffOverlay,
        GridSize::new(120, 40),
        "default",
    );
    assert_eq!(grid.cells.len(), 120 * 40, "the grid fills the viewport");
    let text = grid.plain_text();
    assert!(
        text.contains("Diff"),
        "the diff card header paints:\n{text}"
    );
    assert!(
        text.contains('+') && text.contains('-'),
        "the +/- diff body paints:\n{text}"
    );
}

#[test]
fn review_board_scenario_paints_lanes_and_caret() {
    let grid = FrameGrid::capture(
        VisualScenario::ReviewBoard,
        GridSize::new(120, 40),
        "default",
    );
    let text = grid.plain_text();
    assert!(
        text.contains("review board"),
        "the review-board header paints:\n{text}"
    );
    assert!(
        text.contains('\u{203a}'),
        "the selected-card caret paints:\n{text}"
    );
    assert!(text.contains("Running"), "a lane header paints:\n{text}");
}

#[test]
fn diff_detail_pane_scenario_paints_the_split() {
    // Wide enough to split: the pane separator and the pinned diff body paint
    // beside the transcript.
    let grid = FrameGrid::capture(
        VisualScenario::DiffDetailPane,
        GridSize::new(120, 40),
        "default",
    );
    let text = grid.plain_text();
    assert!(
        text.contains("Diff"),
        "the pinned diff entry paints in the split:\n{text}"
    );
    assert!(
        text.contains('+') && text.contains('-'),
        "the detail body's +/- lines paint:\n{text}"
    );
}

#[test]
fn dashboard_html_marks_changed_scenarios_for_the_filter() {
    // The scroll-move report is a real change; its panel must be tagged
    // data-changed="true" so the JS filter can hide the unchanged ones.
    let reports = default_reports(GridSize::new(80, 24));
    let html = render_dashboard_html(&reports);
    assert!(
        html.contains("data-changed=\"true\""),
        "at least one changed panel must be flagged for the filter"
    );
    assert!(
        html.contains("data-changed=\"false\""),
        "the identity panels must be flagged unchanged"
    );
}

#[test]
fn dashboard_html_escapes_cell_content() {
    // A grid carrying a literal '<' must be HTML-escaped, never injected raw.
    let size = GridSize::new(4, 1);
    let mut grid = solid_grid(size, " ", Color::Reset);
    grid.cells[0].symbol = "┃".to_string();
    grid.cells[2].symbol = "<".to_string();
    let report = ScenarioReport::from_grids(grid.clone(), grid);
    let html = render_dashboard_html(&[report]);
    assert!(html.contains("&lt;"), "the '<' cell must be escaped");
    assert!(
        !html.contains("<span class=\"c same\"><</span>"),
        "raw '<' must never be injected into the grid"
    );
}

// ---------------------------------------------------------------------------
// Baseline-update workflow — explicit, deterministic
// ---------------------------------------------------------------------------

#[test]
fn baseline_update_workflow_round_trips() {
    // The explicit baseline-update workflow the spec calls for: capture a fresh
    // current grid, adopt it as the new baseline, and confirm the re-diff is
    // empty (the baseline now matches). Deterministic across runs.
    let size = GridSize::new(80, 24);
    let original_baseline = FrameGrid::capture(VisualScenario::ShortChat, size, "default");
    let current = FrameGrid::capture(VisualScenario::ShortChat, size, "default");

    // Same scenario captured twice is byte-identical (deterministic renderer).
    let before = GridDiff::compute(&original_baseline, &current);
    assert_eq!(
        before.changed_cells, 0,
        "the deterministic renderer must produce an identical frame on recapture"
    );

    // Adopt `current` as the new baseline and re-diff against itself: empty.
    let updated_baseline = current.clone();
    let after = GridDiff::compute(&updated_baseline, &current);
    assert_eq!(after.changed_cells, 0, "an adopted baseline diffs clean");
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn plain_text_preserves_geometry() {
    let size = GridSize::new(3, 2);
    let grid = solid_grid(size, "a", Color::Gray);
    let text = grid.plain_text();
    // 3 cols + newline per row, 2 rows.
    assert_eq!(text, "aaa\naaa\n");
}

#[test]
fn display_symbol_maps_blank_to_nbsp() {
    assert_eq!(display_symbol(" "), "\u{00a0}");
    assert_eq!(display_symbol(""), "\u{00a0}");
    assert_eq!(display_symbol("x"), "x");
}

#[test]
fn escape_html_escapes_the_dangerous_three() {
    assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
}
