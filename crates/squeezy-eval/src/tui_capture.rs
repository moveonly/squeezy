//! Per-turn TUI render-capture pipeline.
//!
//! Phase 5 of the eval-harness plan. When a scenario opts in via
//! `[tui_capture] enabled = true`, the driver builds a fresh
//! `ratatui::Terminal<TestBackend>` for each completed turn, renders the
//! assembled assistant text through the same markdown pipeline the TUI
//! uses (`squeezy_tui::render_markdown`), and writes a `frames_tui.jsonl`
//! record carrying:
//!
//! - the cell grid (one entry per `(x, y)` with symbol + fg/bg/modifiers)
//! - the plain-text projection (symbols-only)
//! - an ANSI re-render suitable for `cat`
//! - a snapshot of any overlay-triggering events observed during the
//!   turn (approvals, MCP elicitations, RequestUserInput)
//!
//! This is intentionally narrower than the "drive the full `TuiApp`
//! event loop from the eval driver" model documented in the plan: that
//! requires moving `TuiApp` and `handle_key` / `drain_agent_events`
//! out of `pub(crate)` behind a feature gate. The lighter shape lands
//! the screen-level cell+style capture and the overlay-state hooks
//! today, with a forward-compat shape that a follow-up can layer the
//! deeper state machine onto without re-doing the file format.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use serde::{Deserialize, Serialize};

use crate::driver::EvalError;
use crate::scenario::TuiCaptureConfig;

const DEFAULT_WIDTH: u16 = 120;
const DEFAULT_HEIGHT: u16 = 40;

/// Single rendered cell. Coordinates are 0-based with `(0, 0)` at the
/// top-left of the backing `TestBackend`. `bg` / `fg` are named when
/// they map to a ratatui named color, otherwise `rgb(R,G,B)` or
/// `indexed(N)`. Cells whose symbol is a bare space and whose styling
/// is the default are omitted from the serialized grid to keep
/// `frames_tui.jsonl` small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

/// One record per turn (or per key/action when `drive_tui = true`),
/// written to `frames_tui.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiFrame {
    pub turn_id: String,
    pub width: u16,
    pub height: u16,
    pub cells: Vec<TuiCell>,
    /// Flattened plain-text projection of the grid, row-major. Trailing
    /// spaces per row are preserved so diffs see column shifts.
    pub plain_text: String,
    /// ANSI-escaped re-render suitable for `cat`ing into a terminal.
    pub ansi: String,
    /// True when the serialized grid clipped rendered content below
    /// `height`. Consumers can distinguish "blank lower rows" from
    /// "more content existed but was off-screen".
    #[serde(default)]
    pub visual_truncated: bool,
    /// Estimated wrapped visual rows omitted by `height` clipping.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_line_count: u64,
    /// Overlay-triggering events observed in this turn. Each entry is
    /// a small `{kind, summary}` pair so reviewers can spot
    /// "approval was requested but not answered" without reaching
    /// into `trace.jsonl`.
    #[serde(default)]
    pub overlays: Vec<TuiOverlayEvent>,
    /// What produced this frame. Absent on records emitted by the
    /// pre-`drive_tui` per-turn pipeline (those are implicitly
    /// `turn_completed`); populated for key- and action-driven
    /// snapshots so reviewers can interleave them with turn frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TuiFrameTrigger>,
    /// Public projection of the live transcript at frame time.
    /// Populated only by harness-driven snapshots; the legacy
    /// markdown-only path leaves it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<TuiTranscriptSummary>,
    /// Status-bar text at frame time. Toggle handlers write here
    /// (`"expanded 1 of 3"`) so assertions can key off it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
}

/// What caused a frame to be captured. `kind` distinguishes turn
/// boundaries from explicit `send_key` / `send_keys` / other scripted
/// actions. Action-driven frames also carry the originating index +
/// the literal key string when applicable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiFrameTrigger {
    /// `"turn_completed" | "key" | "action"`.
    pub kind: String,
    /// Zero-based index of the producing step into the scenario's
    /// `steps` list. Absent for triggers that aren't tied to a step
    /// (e.g. a post-mortem snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
    /// The literal key spec (`"Ctrl+O"`) for `kind = "key"` frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Public projection of one transcript entry. Mirrors
/// `squeezy_tui::testing::TranscriptEntrySummary`; serialized into the
/// frame record so a reviewer can read transcript state without
/// re-running the scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiTranscriptSummary {
    /// Tag string: `"message" | "tool_result" | "log" | "plan_card" |
    /// "diff" | "reasoning" | "slash_echo"`.
    pub kind: String,
    pub collapsed: bool,
    /// Up to ~80 characters of the entry's primary text. Empty when
    /// the entry kind has no textual primary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
}

/// A high-level overlay-event marker. The eval driver derives these
/// from the same agent events it captures into `trace.jsonl`; they're
/// duplicated here so the frame's screen capture and its
/// overlay-state snapshot live side-by-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiOverlayEvent {
    /// `"approval"`, `"mcp_elicitation"`, `"request_user_input"`,
    /// or any future overlay surface.
    pub kind: String,
    /// Short, human-readable summary (tool name, server name, prompt
    /// prefix, etc.).
    pub summary: String,
    /// Final disposition observed during the turn: `"approved"`,
    /// `"denied:<reason>"`, `"auto_cancelled"`, `"accepted"`, etc.
    pub disposition: String,
    /// Extra structured details worth displaying in the capture frame:
    /// permission summaries, choices, full free-form answers, or denial
    /// context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// Tool-specific preview lines when an overlay was raised for a tool
    /// approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<String>,
}

#[derive(Debug)]
pub struct RenderedGrid {
    pub cells: Vec<TuiCell>,
    pub plain_text: String,
    pub ansi: String,
    pub visual_truncated: bool,
    pub omitted_line_count: u64,
}

/// JSONL writer wrapping `frames_tui.jsonl`. Created lazily so
/// scenarios that disable capture don't touch the filesystem.
pub struct TuiCaptureWriter {
    inner: Mutex<TuiCaptureInner>,
    width: u16,
    height: u16,
    /// Pinned palette tone (`"dark"` or `"light"`). The actual palette
    /// override is process-wide via env (`COLORFGBG`) — set in
    /// `provision` below. Kept as a field so future helpers can
    /// re-inspect what the writer was configured with.
    #[allow(dead_code)]
    palette_tone: Option<String>,
}

struct TuiCaptureInner {
    path: PathBuf,
    file: std::fs::File,
    /// `replay.tui` — a concatenated ANSI stream a reviewer can
    /// `cat` to see exactly what each turn rendered, in order. Lives
    /// in the same run directory as `frames_tui.jsonl`.
    replay_path: PathBuf,
    replay_file: std::fs::File,
}

impl TuiCaptureWriter {
    pub fn provision(dir: &Path, config: &TuiCaptureConfig) -> Result<Option<Self>, EvalError> {
        if !config.enabled {
            return Ok(None);
        }
        std::fs::create_dir_all(dir)
            .map_err(|err| EvalError::Io(format!("create_dir_all {dir:?}: {err}")))?;
        let path = dir.join("frames_tui.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        let replay_path = dir.join("replay.tui");
        let replay_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&replay_path)
            .map_err(|err| EvalError::Io(format!("open {replay_path:?}: {err}")))?;
        // Pin a deterministic palette tone via env. The TUI's palette
        // detector reads `COLORFGBG` and `NO_COLOR`; forcing the requested
        // value here makes fixture captures reproducible even when the host
        // shell already exported a conflicting terminal palette hint.
        let value = match config.palette_tone.as_deref() {
            Some("light") => "0;15", // black on white = light terminal
            _ => "15;0",             // white on black = dark terminal
        };
        // SAFETY: process-wide env mutation. The driver runs one scenario per
        // process today; the parallel runner in Phase 7 needs a per-process
        // scratch env or to set this before forking.
        unsafe {
            std::env::set_var("COLORFGBG", value);
        }
        Ok(Some(Self {
            inner: Mutex::new(TuiCaptureInner {
                path,
                file,
                replay_path,
                replay_file,
            }),
            width: config.width.unwrap_or(DEFAULT_WIDTH),
            height: config.height.unwrap_or(DEFAULT_HEIGHT),
            palette_tone: config.palette_tone.clone(),
        }))
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().expect("tui capture lock").path.clone()
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn write(&self, frame: &TuiFrame) -> Result<(), EvalError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| EvalError::Internal(format!("tui capture mutex poisoned: {err}")))?;
        let line = serde_json::to_string(frame)
            .map_err(|err| EvalError::Internal(format!("serialize tui frame: {err}")))?;
        writeln!(guard.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append tui frame: {err}")))?;
        // Append the same ANSI grid into `replay.tui`. A clear-screen
        // sequence between frames keeps `cat replay.tui` viewable as
        // a sequential animation in a terminal that respects CSI.
        let header = format!("\x1b[2J\x1b[H[turn {}]\n", frame.turn_id);
        guard
            .replay_file
            .write_all(header.as_bytes())
            .map_err(|err| EvalError::Io(format!("append replay header: {err}")))?;
        guard
            .replay_file
            .write_all(frame.ansi.as_bytes())
            .map_err(|err| EvalError::Io(format!("append replay frame: {err}")))?;
        Ok(())
    }

    /// Absolute path to the `replay.tui` file. The driver doesn't
    /// reference it today but consumers (and future docs) might.
    #[allow(dead_code)]
    pub fn replay_path(&self) -> PathBuf {
        self.inner
            .lock()
            .expect("tui capture lock")
            .replay_path
            .clone()
    }
}

/// Render an assembled markdown body through the TUI's markdown
/// pipeline against a fresh `TestBackend` and return the resulting
/// cell grid + plain-text + ANSI re-render.
///
/// The grid is wrapped at `width` columns and clipped to `height`
/// rows. Long assistant outputs are truncated visually — the
/// reviewer is meant to see "what the user saw on screen", not the
/// full corpus.
pub fn render_markdown_to_grid(
    markdown: &str,
    width: u16,
    height: u16,
) -> Result<RenderedGrid, EvalError> {
    render_lines_to_grid(squeezy_tui::render_markdown(markdown), width, height)
}

pub fn render_capture_to_grid(
    markdown: &str,
    overlays: &[TuiOverlayEvent],
    width: u16,
    height: u16,
) -> Result<RenderedGrid, EvalError> {
    let body = capture_markdown(markdown, overlays);
    render_markdown_to_grid(&body, width, height)
}

fn render_lines_to_grid(
    lines: Vec<Line<'static>>,
    width: u16,
    height: u16,
) -> Result<RenderedGrid, EvalError> {
    let estimated_rows = estimated_wrapped_rows(&lines, width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)
        .map_err(|err| EvalError::Internal(format!("create TestBackend terminal: {err}")))?;
    terminal
        .draw(|frame| {
            let area = frame.area();
            frame.render_widget(paragraph, area);
        })
        .map_err(|err| EvalError::Internal(format!("draw paragraph: {err}")))?;
    let buffer = terminal.backend().buffer().clone();

    let mut cells: Vec<TuiCell> = Vec::new();
    let mut plain = String::with_capacity((width as usize + 1) * height as usize);
    let mut ansi = String::with_capacity((width as usize + 8) * height as usize);
    let mut last_style: Option<Style> = None;
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol().to_string();
            plain.push_str(&symbol);
            // ANSI re-render: emit a style change when the style differs
            // from the previous cell. A full reset between styles keeps
            // the encoding simple — modern terminals collapse the
            // sequences fine.
            let style = Style::default()
                .fg(cell.fg)
                .bg(cell.bg)
                .add_modifier(cell.modifier);
            let style_changed = last_style != Some(style);
            if style_changed {
                if last_style.is_some() {
                    ansi.push_str("\x1b[0m");
                }
                ansi.push_str(&style_to_ansi(&style));
                last_style = Some(style);
            }
            ansi.push_str(&symbol);
            // Skip default-styled spaces from the structured cell grid
            // to keep the file small. ANSI / plain-text still carry them.
            let is_blank = symbol == " "
                && cell.fg == Color::Reset
                && cell.bg == Color::Reset
                && cell.modifier.is_empty();
            if !is_blank {
                cells.push(TuiCell {
                    x,
                    y,
                    symbol,
                    fg: color_name(cell.fg),
                    bg: color_name(cell.bg),
                    modifiers: modifier_names(cell.modifier),
                });
            }
        }
        plain.push('\n');
        // Force a style reset at row boundary so wrap-around doesn't
        // bleed background into the next line in the ANSI output.
        if last_style.is_some() {
            ansi.push_str("\x1b[0m");
            last_style = None;
        }
        ansi.push('\n');
    }
    let omitted_line_count = estimated_rows.saturating_sub(u64::from(height));
    Ok(RenderedGrid {
        cells,
        plain_text: plain,
        ansi,
        visual_truncated: omitted_line_count > 0,
        omitted_line_count,
    })
}

fn capture_markdown(markdown: &str, overlays: &[TuiOverlayEvent]) -> String {
    let body = repair_glued_sentences(markdown);
    if overlays.is_empty() {
        return body;
    }
    let mut out = String::new();
    out.push_str("**Overlay state**\n");
    for overlay in overlays {
        out.push_str("- `");
        out.push_str(&overlay.kind);
        out.push_str("` ");
        out.push_str(&overlay.summary);
        out.push_str(" -> ");
        out.push_str(&overlay.disposition);
        out.push('\n');
        for detail in &overlay.details {
            out.push_str("  - ");
            out.push_str(detail);
            out.push('\n');
        }
        for preview in overlay.preview.iter().take(8) {
            out.push_str("  - `preview` ");
            out.push_str(preview);
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str(&body);
    out
}

fn repair_glued_sentences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        out.push(ch);
        if matches!(ch, '.' | '!' | '?')
            && let Some(next) = chars.peek()
            && next.is_ascii_uppercase()
        {
            out.push(' ');
        }
    }
    out
}

fn estimated_wrapped_rows(lines: &[Line<'_>], width: u16) -> u64 {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| {
            let chars: usize = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
            chars.div_ceil(width).max(1) as u64
        })
        .sum()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn color_name(c: Color) -> Option<String> {
    if c == Color::Reset {
        return None;
    }
    Some(match c {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark_gray".into(),
        Color::LightRed => "light_red".into(),
        Color::LightGreen => "light_green".into(),
        Color::LightYellow => "light_yellow".into(),
        Color::LightBlue => "light_blue".into(),
        Color::LightMagenta => "light_magenta".into(),
        Color::LightCyan => "light_cyan".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
        Color::Indexed(i) => format!("indexed({i})"),
    })
}

fn modifier_names(modifier: Modifier) -> Vec<String> {
    let mut out = Vec::new();
    for (flag, name) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underlined"),
        (Modifier::SLOW_BLINK, "slow_blink"),
        (Modifier::RAPID_BLINK, "rapid_blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed_out"),
    ] {
        if modifier.contains(flag) {
            out.push(name.to_string());
        }
    }
    out
}

fn style_to_ansi(style: &Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(c) = style.fg
        && c != Color::Reset
    {
        codes.extend(fg_codes(c));
    }
    if let Some(c) = style.bg
        && c != Color::Reset
    {
        codes.extend(bg_codes(c));
    }
    let mods = style.add_modifier;
    if mods.contains(Modifier::BOLD) {
        codes.push("1".into());
    }
    if mods.contains(Modifier::DIM) {
        codes.push("2".into());
    }
    if mods.contains(Modifier::ITALIC) {
        codes.push("3".into());
    }
    if mods.contains(Modifier::UNDERLINED) {
        codes.push("4".into());
    }
    if mods.contains(Modifier::REVERSED) {
        codes.push("7".into());
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        codes.push("9".into());
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn fg_codes(c: Color) -> Vec<String> {
    match c {
        Color::Reset => vec!["39".into()],
        Color::Black => vec!["30".into()],
        Color::Red => vec!["31".into()],
        Color::Green => vec!["32".into()],
        Color::Yellow => vec!["33".into()],
        Color::Blue => vec!["34".into()],
        Color::Magenta => vec!["35".into()],
        Color::Cyan => vec!["36".into()],
        Color::Gray => vec!["37".into()],
        Color::DarkGray => vec!["90".into()],
        Color::LightRed => vec!["91".into()],
        Color::LightGreen => vec!["92".into()],
        Color::LightYellow => vec!["93".into()],
        Color::LightBlue => vec!["94".into()],
        Color::LightMagenta => vec!["95".into()],
        Color::LightCyan => vec!["96".into()],
        Color::White => vec!["97".into()],
        Color::Rgb(r, g, b) => vec![
            "38".into(),
            "2".into(),
            r.to_string(),
            g.to_string(),
            b.to_string(),
        ],
        Color::Indexed(i) => vec!["38".into(), "5".into(), i.to_string()],
    }
}

fn bg_codes(c: Color) -> Vec<String> {
    match c {
        Color::Reset => vec!["49".into()],
        Color::Black => vec!["40".into()],
        Color::Red => vec!["41".into()],
        Color::Green => vec!["42".into()],
        Color::Yellow => vec!["43".into()],
        Color::Blue => vec!["44".into()],
        Color::Magenta => vec!["45".into()],
        Color::Cyan => vec!["46".into()],
        Color::Gray => vec!["47".into()],
        Color::DarkGray => vec!["100".into()],
        Color::LightRed => vec!["101".into()],
        Color::LightGreen => vec!["102".into()],
        Color::LightYellow => vec!["103".into()],
        Color::LightBlue => vec!["104".into()],
        Color::LightMagenta => vec!["105".into()],
        Color::LightCyan => vec!["106".into()],
        Color::White => vec!["107".into()],
        Color::Rgb(r, g, b) => vec![
            "48".into(),
            "2".into(),
            r.to_string(),
            g.to_string(),
            b.to_string(),
        ],
        Color::Indexed(i) => vec!["48".into(), "5".into(), i.to_string()],
    }
}

#[cfg(test)]
#[path = "tui_capture_tests.rs"]
mod tests;
