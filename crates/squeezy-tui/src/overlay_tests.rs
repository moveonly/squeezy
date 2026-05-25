use super::*;

#[test]
fn model_overlay_lists_registry_entries() {
    let overlay = build_model_overlay("openai", "nonexistent");
    let Overlay::Model(inner) = overlay else {
        panic!("expected model overlay")
    };
    assert_eq!(inner.entries.len(), MODEL_REGISTRY.len());
}

#[test]
fn model_overlay_selects_current() {
    let entry = &MODEL_REGISTRY[0];
    let overlay = build_model_overlay(entry.provider, entry.id);
    let Overlay::Model(inner) = overlay else {
        panic!("expected model overlay")
    };
    assert_eq!(inner.selected().unwrap().id, entry.id);
}

#[test]
fn verbosity_overlay_includes_three_levels() {
    let overlay = build_verbosity_overlay(ResponseVerbosity::Normal);
    let Overlay::Verbosity(inner) = overlay else {
        panic!("expected verbosity overlay")
    };
    assert_eq!(inner.entries.len(), 3);
    assert_eq!(inner.selected().unwrap().0, ResponseVerbosity::Normal);
}

#[test]
fn select_overlay_navigation_clamps_at_bounds() {
    let mut overlay = build_verbosity_overlay(ResponseVerbosity::Normal);
    overlay.move_up();
    overlay.move_up();
    overlay.move_up(); // should not panic
    overlay.move_down();
    overlay.move_down();
    overlay.move_down();
    overlay.move_down(); // should not panic
    // unchanged in shape
    if let Overlay::Verbosity(inner) = overlay {
        assert_eq!(inner.entries.len(), 3);
    }
}

#[test]
fn overlay_render_lines_include_title_and_options() {
    let overlay = build_verbosity_overlay(ResponseVerbosity::Concise);
    let rendered: String = overlay
        .render_lines()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Select response verbosity"), "{rendered}");
    assert!(rendered.contains("concise"), "{rendered}");
    assert!(rendered.contains("normal"), "{rendered}");
    assert!(rendered.contains("verbose"), "{rendered}");
}
