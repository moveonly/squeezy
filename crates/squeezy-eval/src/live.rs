//! Live streaming output of squeezy's activity during a scenario run.
//!
//! Without this, `squeezy-eval run` is silent until the final summary
//! line — a user watching has no idea what the agent is doing. The
//! `LivePrinter` hooks into the driver's event loop and writes a
//! human-friendly narrative to stdout (or any writer) as events arrive:
//!
//! - assistant text streams in chunks as the model emits it
//! - tool calls announce when they start ("🔧 search: definition_search")
//!   and when they complete (status + bytes)
//! - approvals, slash commands, findings, errors get one-line callouts
//!
//! All formatting goes through this single module so the live view and
//! the post-run `view` subcommand stay close in style.

use std::io::Write;
use std::sync::Mutex;

use serde_json::Value;
use squeezy_tools::human_label_for_call;

use crate::capture::EvalEventKind;
use crate::scenario::{Action, Step};

const TOOL_ARG_PREVIEW_CHARS: usize = 80;

/// Sink for live narration. The driver feeds events; `LivePrinter`
/// writes formatted lines (and assistant-text chunks) to the underlying
/// writer.
pub struct LivePrinter {
    inner: Mutex<Inner>,
}

struct Inner {
    writer: Box<dyn Write + Send>,
    current_turn: Option<String>,
    assistant_chunk_open: bool,
    enabled: bool,
}

impl LivePrinter {
    /// Live printer that writes to stdout.
    pub fn stdout(enabled: bool) -> Self {
        Self::new(Box::new(std::io::stdout()), enabled)
    }

    pub fn new(writer: Box<dyn Write + Send>, enabled: bool) -> Self {
        Self {
            inner: Mutex::new(Inner {
                writer,
                current_turn: None,
                assistant_chunk_open: false,
                enabled,
            }),
        }
    }

    /// Announce that a step is starting (a prompt or an action). Called
    /// from the driver before the corresponding squeezy work runs.
    pub fn step(&self, index: usize, step: &Step) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !g.enabled {
            return;
        }
        g.finish_assistant_chunk_inplace();
        match step {
            Step::Prompt { text, .. } => {
                let _ = writeln!(g.writer, "\n━━━ step {idx}: prompt", idx = index + 1);
                let _ = writeln!(g.writer, "  > {}", trim_oneline(text, 200));
            }
            Step::Action(action) => {
                let _ = writeln!(g.writer, "\n━━━ step {idx}: action", idx = index + 1);
                let _ = writeln!(g.writer, "  > {}", describe_action(action));
            }
        }
        let _ = g.writer.flush();
    }

    /// Feed one trace event. The printer decides whether to surface it.
    pub fn event(&self, event: &EvalEventKind, turn_id: Option<&str>) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !g.enabled {
            return;
        }
        // Track turn boundaries so we can flush the assistant chunk.
        if let Some(t) = turn_id
            && g.current_turn.as_deref() != Some(t)
        {
            g.finish_assistant_chunk_inplace();
            g.current_turn = Some(t.to_string());
        }
        match event {
            EvalEventKind::AssistantDelta { delta } => {
                g.open_assistant_chunk_inplace();
                let _ = g.writer.write_all(delta.as_bytes());
                let _ = g.writer.flush();
            }
            EvalEventKind::ToolCallStarted { call, origin } => {
                g.finish_assistant_chunk_inplace();
                let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                let label = call
                    .get("arguments")
                    .map(|args| human_label_for_call(name, args))
                    .unwrap_or_else(|| name.to_string());
                let _ = writeln!(
                    g.writer,
                    "  {icon} {label}",
                    icon = icon_for_origin(origin),
                    label = trim_oneline(&label, TOOL_ARG_PREVIEW_CHARS)
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::ToolCallCompleted { result } => {
                let name = result
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let status = result.get("status").and_then(Value::as_str).unwrap_or("?");
                let bytes = result
                    .get("cost_hint")
                    .and_then(|v| v.get("output_bytes"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let icon = match status {
                    "Success" => "✅",
                    "Error" => "❌",
                    "Cancelled" => "⏹",
                    "Denied" => "⛔",
                    _ => "·",
                };
                let _ = writeln!(g.writer, "     ↳ {icon} {name} ({bytes}B)");
                let _ = g.writer.flush();
            }
            EvalEventKind::TurnCompleted { cost, metrics, .. } => {
                g.finish_assistant_chunk_inplace();
                let input = cost
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = cost
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let tools = metrics
                    .get("tool_calls")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let _ = writeln!(
                    g.writer,
                    "  ✓ turn complete · {tools} tool call(s) · in={input} out={output}"
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::TurnFailed { error } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(g.writer, "  🚨 turn failed: {}", trim_oneline(error, 240));
                let _ = g.writer.flush();
            }
            EvalEventKind::TurnCancelled => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(g.writer, "  ⏹ turn cancelled");
                let _ = g.writer.flush();
            }
            EvalEventKind::Approval { request, decision } => {
                g.finish_assistant_chunk_inplace();
                let tool = request.get("tool").and_then(Value::as_str).unwrap_or("?");
                let _ = writeln!(g.writer, "  🛂 approval requested: {tool} → {decision}");
                let _ = g.writer.flush();
            }
            EvalEventKind::SlashCommand { command } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(g.writer, "  ⌘ slash: {command}");
                let _ = g.writer.flush();
            }
            EvalEventKind::ContextCompacted { .. } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(g.writer, "  🗜 context compacted");
                let _ = g.writer.flush();
            }
            EvalEventKind::Finding {
                rule_id,
                severity,
                summary,
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  🔎 finding [{severity}] {rule_id}: {}",
                    trim_oneline(summary, 240)
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::ToolProgress {
                tool_name,
                elapsed_ms,
                ..
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "     ⌛ {tool_name} still running ({elapsed:.1}s)",
                    elapsed = *elapsed_ms as f64 / 1000.0
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::CostUpdate {
                tool_count,
                input_tokens,
                micro_usd,
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  💰 running this turn: {} in · {} (after {} tools)",
                    format_token_count(*input_tokens),
                    format_micro_usd(*micro_usd),
                    tool_count
                );
                let _ = g.writer.flush();
            }
            // Phase 6: surface the events that previously only landed
            // in trace.jsonl. A watcher of `squeezy-eval run` sees
            // the same critical UX signals the real TUI shows.
            EvalEventKind::SubagentEvent { event } => {
                g.finish_assistant_chunk_inplace();
                let kind = event.get("kind").and_then(Value::as_str).unwrap_or("?");
                let agent = event.get("agent").and_then(Value::as_str).unwrap_or("?");
                let icon = match kind {
                    "started" => "🤖",
                    "completed" => "✅",
                    "failed" => "🚨",
                    "rejected" => "⛔",
                    _ => "·",
                };
                let detail = match kind {
                    "completed" => event
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(|s| format!(": {}", trim_oneline(s, 80)))
                        .unwrap_or_default(),
                    "failed" => event
                        .get("error")
                        .and_then(Value::as_str)
                        .map(|s| format!(": {}", trim_oneline(s, 120)))
                        .unwrap_or_default(),
                    "rejected" => event
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(|s| format!(": {s}"))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let _ = writeln!(g.writer, "  {icon} subagent {kind} ({agent}){detail}");
                let _ = g.writer.flush();
            }
            EvalEventKind::McpStatusUpdated { servers, .. } => {
                g.finish_assistant_chunk_inplace();
                let summary = summarize_mcp_status(servers);
                let _ = writeln!(g.writer, "  🛰 mcp status: {summary}");
                let _ = g.writer.flush();
            }
            EvalEventKind::JobNotification {
                title,
                summary,
                status,
                ..
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  📢 job {status}: {} — {}",
                    trim_oneline(title, 60),
                    trim_oneline(summary, 120)
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::CostWarning {
                spent_usd_micros,
                cap_usd_micros,
                percent,
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  ⚠ cost warning: spent ${:.4} / cap ${:.4} ({}%)",
                    *spent_usd_micros as f64 / 1_000_000.0,
                    *cap_usd_micros as f64 / 1_000_000.0,
                    percent
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::AiReviewerTripped { reason } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  🛑 ai reviewer tripped: {}",
                    trim_oneline(reason, 200)
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::ShellSandboxDegraded {
                backend,
                fallback_count,
            } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  🪨 sandbox degraded: backend={backend} fallback_count={fallback_count}"
                );
                let _ = g.writer.flush();
            }
            EvalEventKind::ReasoningSegment { display_text, .. } => {
                g.finish_assistant_chunk_inplace();
                let _ = writeln!(
                    g.writer,
                    "  ▾ thinking: {}",
                    trim_oneline(display_text, 200)
                );
                let _ = g.writer.flush();
            }
            _ => {}
        }
    }

    /// Flush any half-open assistant chunk. Called at end of run.
    pub fn flush(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.finish_assistant_chunk_inplace();
            let _ = g.writer.flush();
        }
    }
}

impl Inner {
    fn open_assistant_chunk_inplace(&mut self) {
        if !self.assistant_chunk_open {
            let _ = writeln!(self.writer, "  💬");
            let _ = self.writer.write_all(b"     ");
            self.assistant_chunk_open = true;
        }
    }

    fn finish_assistant_chunk_inplace(&mut self) {
        if self.assistant_chunk_open {
            let _ = writeln!(self.writer);
            self.assistant_chunk_open = false;
        }
    }
}

fn describe_action(action: &Action) -> String {
    match action {
        Action::Approve { r#match, .. } => format!(
            "approve {}",
            r#match
                .as_ref()
                .and_then(|m| m.tool.as_deref())
                .map(|t| format!("tool={t}"))
                .unwrap_or_else(|| "any".into())
        ),
        Action::Deny { reason, .. } => {
            format!("deny ({})", reason.as_deref().unwrap_or("no reason"))
        }
        Action::SlashCommand { command, .. } => format!("slash: {command}"),
        Action::EditFile { path, .. } => format!("edit_file: {}", path.display()),
        Action::WaitSeconds { seconds, .. } => format!("wait {seconds}s"),
        Action::CancelTurn { .. } => "cancel turn".into(),
        Action::Assert { .. } => "assert".into(),
        Action::InjectUserText { text, .. } => {
            format!("inject_user_text: {}", trim_oneline(text, 80))
        }
        Action::RespondElicitation {
            r#match, decision, ..
        } => {
            let label = r#match
                .as_ref()
                .and_then(|m| m.server.as_deref())
                .map(|s| format!("server={s}"))
                .unwrap_or_else(|| "any".into());
            let decision_label = match decision {
                crate::scenario::ElicitationDecision::Accept { .. } => "accept",
                crate::scenario::ElicitationDecision::Decline => "decline",
                crate::scenario::ElicitationDecision::Cancel => "cancel",
            };
            format!("respond_elicitation {label} → {decision_label}")
        }
        Action::RespondUserInput { decision, .. } => {
            let decision_label = match decision {
                crate::scenario::UserInputDecision::Choice { value } => {
                    format!("choice={}", trim_oneline(value, 40))
                }
                crate::scenario::UserInputDecision::Freeform { text } => {
                    format!("freeform={}", trim_oneline(text, 40))
                }
                crate::scenario::UserInputDecision::Cancel => "cancel".to_string(),
            };
            format!("respond_user_input → {decision_label}")
        }
        Action::ApplyDiff { path, .. } => format!("apply_diff: {}", path.display()),
        Action::SwitchMode { mode, .. } => format!("switch_mode: {mode}"),
        Action::AttachFile { path, .. } => format!("attach_file: {}", path.display()),
        Action::DetachAttachment { id, .. } => format!("detach_attachment: {id}"),
        Action::SendKey { key, .. } => format!("send_key: {key}"),
        Action::SendKeys { keys, .. } => format!("send_keys: [{}]", keys.join(", ")),
    }
}

/// Pick the leading icon for a tool-call line based on who initiated it.
/// Planner preflight runs before the model sees the prompt, so a compass
/// fits; subagent calls travel through a different dispatch and use the
/// robot face the TUI already associates with them.
fn icon_for_origin(origin: &str) -> &'static str {
    match origin {
        "planner" => "🧭",
        "subagent" => "🤖",
        _ => "🔧",
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Render an MCP per-server status map (as emitted by the typed
/// `McpStatusUpdated` event) into a compact one-line summary that
/// mirrors the TUI's status line shape.
fn summarize_mcp_status(servers: &Value) -> String {
    let Some(obj) = servers.as_object() else {
        return "no servers".into();
    };
    let mut ready = 0usize;
    let mut starting = 0usize;
    let mut failed = 0usize;
    let mut cancelled = 0usize;
    let mut failed_names: Vec<String> = Vec::new();
    for (name, status) in obj {
        if status.as_str() == Some("Starting") {
            starting += 1;
        } else if status.as_str() == Some("Cancelled") {
            cancelled += 1;
        } else if status.is_object() && status.get("Ready").is_some() {
            ready += 1;
        } else if status.is_object() && status.get("Failed").is_some() {
            failed += 1;
            failed_names.push(name.clone());
        }
    }
    let mut parts = vec![format!("{ready} ready")];
    if starting > 0 {
        parts.push(format!("{starting} starting"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed [{}]", failed_names.join(", ")));
    }
    if cancelled > 0 {
        parts.push(format!("{cancelled} cancelled"));
    }
    parts.join(" · ")
}

fn format_micro_usd(micro: u64) -> String {
    let dollars = micro as f64 / 1_000_000.0;
    if dollars < 0.01 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.3}")
    }
}

fn trim_oneline(s: &str, max: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
