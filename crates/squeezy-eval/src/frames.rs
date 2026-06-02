use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::driver::EvalError;

/// One "frame" per completed (or terminated) agent turn. This is the
/// human-friendly view of what a TUI user would have seen: the assembled
/// assistant text, plus the tool calls fired and any error/finish reason.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameRecord {
    pub turn_id: String,
    pub prompt: String,
    /// Concatenation of all assistant text deltas for this turn, in order.
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCallSummary>,
    /// Tool calls queued by the agent before execution. This can be
    /// larger than `tool_calls` when a provider stream fails after
    /// emitting calls but before the agent starts them, which is exactly
    /// the failure shape users need eval frames to make visible.
    #[serde(default)]
    pub queued_tool_calls: Vec<ToolCallSummary>,
    pub tool_errors: Vec<String>,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Estimated turn cost in USD microdollars (1_000_000 = $1.00). Computed
    /// from the model's pricing entry via `squeezy_llm::estimate_cost`. Zero
    /// when no pricing data is available for the model.
    #[serde(default)]
    pub cost_micro_usd: u64,
    /// Human-readable rendering of `cost_micro_usd`, e.g. `"$0.0123"`.
    #[serde(default)]
    pub cost_display: String,
    /// Structured representation of `assistant_text` rendered through the
    /// same markdown→ratatui pipeline the TUI uses. One entry per
    /// rendered line; ratatui types are flattened into plain JSON.
    #[serde(default)]
    pub styled_lines: Vec<StyledLine>,
    /// ANSI-escaped re-rendering of `styled_lines`. Suitable for piping
    /// into a terminal to preview "what the TUI would have shown" for
    /// this turn.
    #[serde(default)]
    pub ansi: String,
    pub finish: FrameFinish,
    /// Provider-reported normalized stop kind from the final round of
    /// this turn, propagated from `AgentEvent::Completed`. `None` for
    /// turns that did not reach a real provider stream (helper paths,
    /// failed/cancelled turns, replay reconstruction). Surfaced into
    /// `frames.jsonl` so regression rules and the `view` subcommand can
    /// branch on the actual terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<squeezy_llm::StopReason>,
    /// `true` iff the final round was a Qwen3-style "reasoning-only
    /// finish" (`stop_reason=EndTurn` with reasoning text but no
    /// content or tool call). See
    /// `LlmEvent::Completed::reasoning_only_stop` for the exact gate.
    #[serde(default)]
    pub reasoning_only_stop: bool,
    /// Count of tool-call frames the chat-completions provider dropped
    /// during this turn because their stream cut before a function
    /// name arrived (`compatible.rs::drain_tool_calls`). Surfaced
    /// because a silent drop is a strong "I'll do X then stop" smoking
    /// gun for Qwen-class models — the model emits intent text, then
    /// the tool call goes missing on the wire. Always 0 for native
    /// OpenAI / Anthropic / Google / Bedrock / Ollama streams.
    #[serde(default)]
    pub dropped_tool_calls: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StyledLine {
    pub spans: Vec<StyledSpan>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StyledSpan {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

/// Re-render markdown the way the TUI does and return both the
/// structured `Line`s and an ANSI-escaped string suitable for piping
/// straight into a terminal.
///
/// Reuses `squeezy_tui::render_markdown` so any TUI-side palette or
/// styling change shows up in eval frames for free.
pub fn render_styled(markdown: &str) -> (Vec<StyledLine>, String) {
    let lines = squeezy_tui::render_markdown(markdown);
    let mut styled = Vec::with_capacity(lines.len());
    let mut ansi = String::new();
    for line in lines {
        let mut styled_line = StyledLine::default();
        for span in line.spans {
            let st = span.style;
            let span_text = span.content.into_owned();
            styled_line.spans.push(StyledSpan {
                text: span_text.clone(),
                fg: st.fg.and_then(color_name),
                bg: st.bg.and_then(color_name),
                modifiers: modifier_names(st.add_modifier),
            });
            push_ansi(&mut ansi, &st, &span_text);
        }
        ansi.push('\n');
        styled.push(styled_line);
    }
    (styled, ansi)
}

fn modifier_names(modifier: Modifier) -> Vec<String> {
    let mut out = Vec::new();
    let pairs: &[(Modifier, &str)] = &[
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underlined"),
        (Modifier::SLOW_BLINK, "slow_blink"),
        (Modifier::RAPID_BLINK, "rapid_blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed_out"),
    ];
    for (flag, name) in pairs {
        if modifier.contains(*flag) {
            out.push((*name).to_string());
        }
    }
    out
}

fn color_name(c: Color) -> Option<String> {
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

fn push_ansi(out: &mut String, style: &Style, text: &str) {
    let sgr_start = out.len();
    out.push_str("\x1b[");
    let mut wrote_code = false;
    if let Some(c) = style.fg {
        push_fg_codes(out, c, &mut wrote_code);
    }
    if let Some(c) = style.bg {
        push_bg_codes(out, c, &mut wrote_code);
    }
    let mods = style.add_modifier;
    if mods.contains(Modifier::BOLD) {
        push_sgr_code(out, &mut wrote_code, "1");
    }
    if mods.contains(Modifier::DIM) {
        push_sgr_code(out, &mut wrote_code, "2");
    }
    if mods.contains(Modifier::ITALIC) {
        push_sgr_code(out, &mut wrote_code, "3");
    }
    if mods.contains(Modifier::UNDERLINED) {
        push_sgr_code(out, &mut wrote_code, "4");
    }
    if mods.contains(Modifier::REVERSED) {
        push_sgr_code(out, &mut wrote_code, "7");
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        push_sgr_code(out, &mut wrote_code, "9");
    }
    if wrote_code {
        out.push('m');
    } else {
        out.truncate(sgr_start);
    }
    out.push_str(text);
    if wrote_code {
        out.push_str("\x1b[0m");
    }
}

fn push_sgr_code(out: &mut String, wrote_code: &mut bool, code: impl std::fmt::Display) {
    use std::fmt::Write as _;
    if *wrote_code {
        out.push(';');
    } else {
        *wrote_code = true;
    }
    let _ = write!(out, "{code}");
}

fn push_fg_codes(out: &mut String, c: Color, wrote_code: &mut bool) {
    match c {
        Color::Reset => push_sgr_code(out, wrote_code, "39"),
        Color::Black => push_sgr_code(out, wrote_code, "30"),
        Color::Red => push_sgr_code(out, wrote_code, "31"),
        Color::Green => push_sgr_code(out, wrote_code, "32"),
        Color::Yellow => push_sgr_code(out, wrote_code, "33"),
        Color::Blue => push_sgr_code(out, wrote_code, "34"),
        Color::Magenta => push_sgr_code(out, wrote_code, "35"),
        Color::Cyan => push_sgr_code(out, wrote_code, "36"),
        Color::Gray => push_sgr_code(out, wrote_code, "37"),
        Color::DarkGray => push_sgr_code(out, wrote_code, "90"),
        Color::LightRed => push_sgr_code(out, wrote_code, "91"),
        Color::LightGreen => push_sgr_code(out, wrote_code, "92"),
        Color::LightYellow => push_sgr_code(out, wrote_code, "93"),
        Color::LightBlue => push_sgr_code(out, wrote_code, "94"),
        Color::LightMagenta => push_sgr_code(out, wrote_code, "95"),
        Color::LightCyan => push_sgr_code(out, wrote_code, "96"),
        Color::White => push_sgr_code(out, wrote_code, "97"),
        Color::Rgb(r, g, b) => {
            push_sgr_code(out, wrote_code, "38");
            push_sgr_code(out, wrote_code, "2");
            push_sgr_code(out, wrote_code, r);
            push_sgr_code(out, wrote_code, g);
            push_sgr_code(out, wrote_code, b);
        }
        Color::Indexed(i) => {
            push_sgr_code(out, wrote_code, "38");
            push_sgr_code(out, wrote_code, "5");
            push_sgr_code(out, wrote_code, i);
        }
    }
}

fn push_bg_codes(out: &mut String, c: Color, wrote_code: &mut bool) {
    match c {
        Color::Reset => push_sgr_code(out, wrote_code, "49"),
        Color::Black => push_sgr_code(out, wrote_code, "40"),
        Color::Red => push_sgr_code(out, wrote_code, "41"),
        Color::Green => push_sgr_code(out, wrote_code, "42"),
        Color::Yellow => push_sgr_code(out, wrote_code, "43"),
        Color::Blue => push_sgr_code(out, wrote_code, "44"),
        Color::Magenta => push_sgr_code(out, wrote_code, "45"),
        Color::Cyan => push_sgr_code(out, wrote_code, "46"),
        Color::Gray => push_sgr_code(out, wrote_code, "47"),
        Color::DarkGray => push_sgr_code(out, wrote_code, "100"),
        Color::LightRed => push_sgr_code(out, wrote_code, "101"),
        Color::LightGreen => push_sgr_code(out, wrote_code, "102"),
        Color::LightYellow => push_sgr_code(out, wrote_code, "103"),
        Color::LightBlue => push_sgr_code(out, wrote_code, "104"),
        Color::LightMagenta => push_sgr_code(out, wrote_code, "105"),
        Color::LightCyan => push_sgr_code(out, wrote_code, "106"),
        Color::White => push_sgr_code(out, wrote_code, "107"),
        Color::Rgb(r, g, b) => {
            push_sgr_code(out, wrote_code, "48");
            push_sgr_code(out, wrote_code, "2");
            push_sgr_code(out, wrote_code, r);
            push_sgr_code(out, wrote_code, g);
            push_sgr_code(out, wrote_code, b);
        }
        Color::Indexed(i) => {
            push_sgr_code(out, wrote_code, "48");
            push_sgr_code(out, wrote_code, "5");
            push_sgr_code(out, wrote_code, i);
        }
    }
}

/// Per-tool-call breadcrumb stored on the frame so a reviewer can spot
/// duplicate or unexpected calls without reaching into `trace.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub name: String,
    /// First ~200 chars of the JSON-encoded arguments. Designed for
    /// human eyeballing, not parsing.
    pub args_preview: String,
    /// Hex SHA-256 of the full canonical-JSON arguments. Stable
    /// identifier used by the auto-findings rules to detect duplicate
    /// calls within a turn.
    pub args_sha256: String,
    /// Tool status when known (`success`, `error`, `cancelled`, ...).
    #[serde(default)]
    pub status: Option<String>,
}

impl ToolCallSummary {
    pub fn from_call(name: &str, arguments: &Value) -> Self {
        let serialized = serde_json::to_string(arguments).unwrap_or_else(|_| "null".into());
        let mut hasher = Sha256::new();
        hasher.update(serialized.as_bytes());
        let digest = hasher.finalize();
        let args_sha256 = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        let args_preview: String = serialized.chars().take(200).collect();
        Self {
            name: name.to_string(),
            args_preview,
            args_sha256,
            status: None,
        }
    }
}

pub fn format_cost_micro_usd(micro: u64) -> String {
    let dollars = micro as f64 / 1_000_000.0;
    format!("${dollars:.4}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFinish {
    #[default]
    Completed,
    Cancelled,
    Failed,
    NoTurn,
}

pub struct FrameWriter {
    inner: Mutex<FrameInner>,
}

struct FrameInner {
    path: PathBuf,
    file: std::fs::File,
}

impl FrameWriter {
    pub fn create(dir: &Path) -> Result<Self, EvalError> {
        std::fs::create_dir_all(dir)
            .map_err(|err| EvalError::Io(format!("create_dir_all {dir:?}: {err}")))?;
        let path = dir.join("frames.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        Ok(Self {
            inner: Mutex::new(FrameInner { path, file }),
        })
    }

    pub fn write(&self, frame: &FrameRecord) -> Result<(), EvalError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| EvalError::Internal(format!("frame mutex poisoned: {err}")))?;
        let line = serde_json::to_string(frame)
            .map_err(|err| EvalError::Internal(format!("serialize frame: {err}")))?;
        writeln!(guard.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append frame: {err}")))?;
        Ok(())
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().expect("frame lock").path.clone()
    }
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
