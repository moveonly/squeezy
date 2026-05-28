use super::*;

#[test]
fn renders_plain_text_into_grid() {
    let (cells, plain, ansi) = render_markdown_to_grid("hello world", 16, 4).expect("render");
    assert!(plain.starts_with("hello world"), "plain={plain:?}");
    assert!(ansi.contains("hello world"), "ansi={ansi:?}");
    // First row should have 11 non-blank cells.
    let row0: Vec<&TuiCell> = cells.iter().filter(|c| c.y == 0).collect();
    assert!(!row0.is_empty(), "expected non-blank cells in row 0");
}

#[test]
fn dimensions_round_to_grid_bounds() {
    let (_, plain, _) = render_markdown_to_grid("hi", 4, 2).expect("render");
    // Two rows of four columns + one newline per row.
    assert_eq!(plain.len(), (4 + 1) * 2);
}

#[test]
fn provision_returns_none_when_disabled() {
    let dir = std::env::temp_dir().join(format!("squeezy-eval-tui-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cfg = TuiCaptureConfig::default();
    let writer = TuiCaptureWriter::provision(&dir, &cfg).expect("provision");
    assert!(writer.is_none(), "disabled config should not provision");
}
