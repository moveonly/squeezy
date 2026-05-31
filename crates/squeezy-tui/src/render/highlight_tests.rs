use std::sync::Arc;

use super::*;

#[test]
fn current_palette_reads_theme_tokens() {
    let palette = HighlightPalette::current();
    assert_eq!(
        palette.keyword,
        crate::render::theme::color(crate::render::theme::token::SYNTAX_KEYWORD)
    );
    assert_eq!(
        palette.string,
        crate::render::theme::color(crate::render::theme::token::SYNTAX_STRING)
    );
    assert_eq!(
        palette.number,
        crate::render::theme::color(crate::render::theme::token::SYNTAX_LITERAL)
    );
    assert_eq!(
        palette.function,
        crate::render::theme::color(crate::render::theme::token::SYNTAX_FUNCTION)
    );
    assert_eq!(
        palette.punctuation,
        crate::render::theme::color(crate::render::theme::token::SYNTAX_OPERATOR)
    );
}

/// Two consecutive lookups for the same language must return the same
/// `Arc<HighlightConfiguration>`. Without the per-language cache the TUI
/// pays a multi-millisecond `HighlightConfiguration::new` +
/// `configure(&HIGHLIGHT_NAMES)` rebuild on every render — this is the
/// regression guard for F09.
#[test]
fn highlight_config_is_arc_ptr_eq_on_repeat() {
    let first = highlight_config(HighlightLanguage::Rust)
        .expect("rust highlight config must build from the bundled grammar");
    let second = highlight_config(HighlightLanguage::Rust)
        .expect("rust highlight config must build from the bundled grammar");
    assert!(
        Arc::ptr_eq(&first, &second),
        "consecutive lookups for HighlightLanguage::Rust must return the cached Arc, \
         not a freshly rebuilt HighlightConfiguration",
    );
}

/// Different languages must produce distinct cached configs. Without
/// this isolation a fence tagged ```python` would reuse the Rust grammar
/// (or vice versa) and emit nonsense highlights.
#[test]
fn highlight_config_isolates_languages() {
    let rust = highlight_config(HighlightLanguage::Rust).expect("rust highlight config must build");
    let python =
        highlight_config(HighlightLanguage::Python).expect("python highlight config must build");
    assert!(
        !Arc::ptr_eq(&rust, &python),
        "Rust and Python must have distinct cached HighlightConfiguration entries",
    );
}
