//! Shaping and structured-output extraction for shell tool results.
//!
//! Split out of `lib.rs` (rjr.5 F01) so the registry's tool-dispatch impl is
//! easier to navigate; nothing here touches `ToolRegistry` directly.

use std::collections::{HashSet, VecDeque};

use serde_json::Value;

use crate::collapse_whitespace;
use crate::shell_parse::{shell_command_prefix, shell_segments};

pub(crate) fn insert_content_field(content: &mut Value, key: &str, value: Value) {
    if let Some(object) = content.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}

#[derive(Debug)]
pub(crate) struct ShapedShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub family: &'static str,
    pub kind: &'static str,
    pub fallback_reason: Option<String>,
}

pub(crate) fn shape_shell_output(
    command: &str,
    stdout: &str,
    stderr: &str,
    truncated: bool,
    exit_code: Option<i32>,
) -> ShapedShellOutput {
    let family = shell_output_family(command);
    if let Some((stdout, stderr)) = structured_shell_output(family, stdout, stderr) {
        return ShapedShellOutput {
            stdout,
            stderr,
            family,
            kind: "structured",
            fallback_reason: None,
        };
    }

    let fallback_reason = structured_family(family)
        .then(|| format!("{family} structured output was unavailable or could not be parsed"));
    ShapedShellOutput {
        stdout: shape_unstructured_stream(stdout, truncated, exit_code),
        stderr: shape_unstructured_stream(stderr, truncated, exit_code),
        family,
        kind: if fallback_reason.is_some() {
            "raw_passthrough_shaped"
        } else {
            "line_shaper"
        },
        fallback_reason,
    }
}

fn shell_output_family(command: &str) -> &'static str {
    let command = collapse_whitespace(command);
    let segments = shell_segments(&command);
    let prefixes = segments
        .iter()
        .map(|segment| shell_command_prefix(segment))
        .collect::<Vec<_>>();
    if prefixes.iter().any(|prefix| prefix == "cargo nextest") {
        "nextest"
    } else if prefixes.iter().any(|prefix| prefix.starts_with("cargo ")) {
        "cargo"
    } else if prefixes.iter().any(|prefix| prefix == "rustc") {
        "rustc"
    } else if prefixes.iter().any(|prefix| prefix == "pytest") {
        "pytest"
    } else if prefixes.iter().any(|prefix| prefix == "jest")
        || segments
            .iter()
            .any(|segment| shell_segment_contains_command(segment, "jest"))
    {
        "jest"
    } else if prefixes.iter().any(|prefix| prefix == "vitest")
        || segments
            .iter()
            .any(|segment| shell_segment_contains_command(segment, "vitest"))
    {
        "vitest"
    } else {
        "shell"
    }
}

fn shell_segment_contains_command(segment: &str, command: &str) -> bool {
    segment.split_whitespace().any(|word| {
        let word = word.trim_matches(|ch| matches!(ch, '\'' | '"' | '(' | ')' | ';'));
        word == command || word.ends_with(&format!("/{command}"))
    })
}

fn structured_family(family: &str) -> bool {
    matches!(
        family,
        "cargo" | "rustc" | "nextest" | "pytest" | "jest" | "vitest"
    )
}

fn structured_shell_output(family: &str, stdout: &str, stderr: &str) -> Option<(String, String)> {
    match family {
        "cargo" | "rustc" => parse_cargo_or_rustc_json(stdout, stderr),
        "nextest" => parse_nextest_json(stdout, stderr),
        "pytest" | "jest" | "vitest" => parse_test_report_json(stdout, stderr, family),
        _ => None,
    }
}

fn parse_cargo_or_rustc_json(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let mut kept = Vec::new();
    let mut plain_lines = Vec::new();
    let mut parsed = 0usize;
    let mut finished = None;
    for line in stdout.lines().chain(stderr.lines()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Cargo emits libtest's plain-text harness output (e.g. "test result:
            // FAILED.", panic backtraces, "FAILED" markers) interleaved with the
            // JSON stream. Preserve those signal lines so shaped output still
            // surfaces test failures.
            if libtest_signal_line(line) {
                plain_lines.push(trim_shaped_block(line.trim_end(), 8_000));
            }
            continue;
        };
        parsed += 1;
        match value.get("reason").and_then(Value::as_str) {
            Some("compiler-message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let level = message
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("note");
                if !matches!(level, "error" | "warning" | "failure-note") {
                    continue;
                }
                let text = message
                    .get("rendered")
                    .and_then(Value::as_str)
                    .or_else(|| message.get("message").and_then(Value::as_str))
                    .unwrap_or("");
                if !text.trim().is_empty() {
                    kept.push(trim_shaped_block(text, 8_000));
                }
            }
            Some("build-finished") => {
                finished = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .map(|success| format!("build-finished success={success}"));
            }
            _ => {}
        }
        if value.get("reason").is_none()
            && let Some(level) = value.get("level").and_then(Value::as_str)
            && matches!(level, "error" | "warning")
        {
            let text = value
                .get("rendered")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("");
            if !text.trim().is_empty() {
                kept.push(trim_shaped_block(text, 8_000));
            }
        }
    }
    // Only claim structured output when at least one JSON line actually
    // parsed. Plain libtest text on its own should fall through to the
    // unstructured shaper, which preserves dedupe markers and noise accounting.
    if parsed == 0 {
        return None;
    }
    if let Some(finished) = finished {
        kept.push(finished);
    }
    kept.extend(plain_lines);
    Some((join_shaped_lines(kept), String::new()))
}

fn libtest_signal_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("test result:") {
        // Filtered cargo runs print "test result: ok. 0 passed; 0 failed;
        // 0 ignored; 0 measured; N filtered out" for every test binary
        // that matched nothing — pure noise. Keep only rows that had
        // actual passes or failures.
        let had_passes = !lower.contains("0 passed");
        let had_fails = !lower.contains("0 failed");
        return had_passes || had_fails;
    }
    lower.starts_with("failures:")
        || lower.starts_with("thread '") && lower.contains("panicked")
        || lower.starts_with("panicked at")
        || lower.contains(" ... failed")
        || lower.starts_with("error: test failed")
        || lower.starts_with("error: ")
        || lower.starts_with("warning: ")
        || lower.starts_with("---- ") && lower.contains(" stdout ----")
}

fn parse_nextest_json(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let mut kept = Vec::new();
    let mut parsed = 0usize;
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut last_summary: Option<Value> = None;
    for line in stdout.lines().chain(stderr.lines()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        parsed += 1;
        let event = value
            .get("type")
            .or_else(|| value.get("event"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let status_lower = status.to_ascii_lowercase();
        let event_lower = event.to_ascii_lowercase();
        let is_per_test_finish = event_lower.contains("test")
            && (event_lower.contains("finish") || event_lower.contains("complete"));
        if is_per_test_finish || !status.is_empty() {
            total += 1;
            if status_lower.contains("pass") || status_lower == "ok" {
                passed += 1;
            } else if status_lower.contains("fail") || status_lower.contains("error") {
                failed += 1;
            } else if status_lower.contains("skip") || status_lower.contains("ignore") {
                skipped += 1;
            }
        }
        if event_lower.contains("summary") || event_lower.contains("run-finished") {
            last_summary = Some(value.clone());
        }
        if line_has_signal(event) || line_has_signal(status) || value_contains_signal(&value) {
            kept.push(trim_shaped_block(&value.to_string(), 8_000));
        }
    }
    if parsed == 0 {
        return None;
    }
    let mut summary_parts = vec!["family=nextest".to_string()];
    if total > 0 {
        summary_parts.push(format!(
            "total={total} passed={passed} failed={failed} skipped={skipped}"
        ));
    }
    if let Some(summary) = last_summary {
        summary_parts.push(trim_shaped_block(&summary.to_string(), 8_000));
    }
    kept.insert(0, summary_parts.join(" "));
    Some((join_shaped_lines(kept), String::new()))
}

fn parse_test_report_json(stdout: &str, stderr: &str, family: &str) -> Option<(String, String)> {
    // jest/pytest/vitest emit a single JSON document on either stdout or
    // stderr. Combining them with a newline produces invalid JSON when both
    // streams have content (e.g. npm warnings on stderr alongside a real
    // report on stdout), so try each stream individually.
    let value = parse_first_valid_json(stdout).or_else(|| parse_first_valid_json(stderr))?;
    let mut kept = Vec::new();
    collect_json_signal_lines(&value, "$", &mut kept);
    let summary = json_test_summary(&value, family);
    if !summary.is_empty() {
        kept.insert(0, summary);
    }
    Some((join_shaped_lines(kept), String::new()))
}

fn parse_first_valid_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    // Fall back to scanning for the first line that parses as JSON, so a
    // header line ("Running tests...") or trailer doesn't defeat the parser.
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
}

fn json_test_summary(value: &Value, family: &str) -> String {
    let mut parts = vec![format!("family={family}")];
    for key in [
        "success",
        "numFailedTests",
        "numPassedTests",
        "numTotalTests",
        "failed",
        "passed",
        "total",
        "exitCode",
    ] {
        if let Some(value) = value.get(key)
            && (value.is_boolean() || value.is_number() || value.is_string())
        {
            parts.push(format!("{key}={value}"));
        }
    }
    if parts.len() == 1 {
        String::new()
    } else {
        parts.join(" ")
    }
}

fn collect_json_signal_lines(value: &Value, path: &str, kept: &mut Vec<String>) {
    match value {
        Value::String(text) if line_has_signal(text) => {
            kept.push(trim_shaped_block(&format!("{path}: {text}"), 8_000));
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_json_signal_lines(item, &format!("{path}[{index}]"), kept);
            }
        }
        Value::Object(entries) => {
            for (key, value) in entries {
                let next = format!("{path}.{key}");
                if line_has_signal(key) && value.is_string() {
                    kept.push(trim_shaped_block(&format!("{next}: {value}"), 8_000));
                }
                collect_json_signal_lines(value, &next, kept);
            }
        }
        _ => {}
    }
}

fn value_contains_signal(value: &Value) -> bool {
    match value {
        Value::String(text) => line_has_signal(text),
        Value::Array(items) => items.iter().any(value_contains_signal),
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| line_has_signal(key) || value_contains_signal(value)),
        _ => false,
    }
}

fn shape_unstructured_stream(text: &str, truncated: bool, exit_code: Option<i32>) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    // Unified-diff output ("git diff", "git show", "diff -u", …) is structured:
    // every line carries signal, blank lines inside hunks are meaningful
    // context, and `+`/`-` markers must survive byte-for-byte. The default
    // shaper would drop blank-context lines as noise and head/tail-cap the
    // body, silently corrupting the diff. Pass it through unchanged so the
    // TUI and model both see the full hunk set.
    if looks_like_unified_diff(text) {
        let mut out = text.trim_end().to_string();
        if truncated {
            out.push_str(
                "\n[raw stream was truncated; recover full bytes via read_tool_output {\"path\": \"<spillover-path>\"}]",
            );
        }
        if let Some(exit_code) = exit_code
            && exit_code != 0
        {
            out.push_str(&format!("\nexit_code={exit_code}"));
        }
        return out;
    }
    const HEAD_BUDGET: usize = 50;
    const TAIL_BUDGET: usize = 50;
    let mut head: Vec<String> = Vec::new();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_BUDGET);
    let mut signal_lines: Vec<String> = Vec::new();
    // Cargo emits each compiler diagnostic twice (once for the `(lib)`
    // target, once for `(lib test)`) with byte-identical bodies. Suppress
    // the duplicate so a single warning doesn't render twice.
    let mut signal_seen: HashSet<String> = HashSet::new();
    let mut dropped = 0usize;
    let mut last_emitted: String = String::new();
    let mut repeats = 0usize;
    let flush_repeats =
        |target: &mut Vec<String>, repeats: &mut usize, tail: &mut VecDeque<String>| {
            if *repeats == 0 {
                return;
            }
            let line = format!("[repeated previous line {} more times]", *repeats);
            if target.len() < HEAD_BUDGET {
                target.push(line);
            } else {
                if tail.len() == TAIL_BUDGET {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            *repeats = 0;
        };
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || line_is_noise(trimmed) {
            dropped += 1;
            continue;
        }
        if trimmed == last_emitted.as_str() {
            repeats += 1;
            dropped += 1;
            continue;
        }
        flush_repeats(&mut head, &mut repeats, &mut tail);
        last_emitted = trimmed.to_string();
        let shaped = trim_shaped_block(trimmed, 2_000);
        if line_has_signal(trimmed) {
            if signal_seen.insert(shaped.clone()) {
                signal_lines.push(shaped);
            } else {
                dropped += 1;
            }
        } else if head.len() < HEAD_BUDGET {
            head.push(shaped);
        } else {
            if tail.len() == TAIL_BUDGET {
                tail.pop_front();
                dropped += 1;
            }
            tail.push_back(shaped);
        }
    }
    flush_repeats(&mut head, &mut repeats, &mut tail);

    let mut kept = head;
    if !signal_lines.is_empty() {
        kept.extend(signal_lines);
    }
    if !tail.is_empty() {
        kept.extend(tail);
    }
    if dropped > 0 {
        kept.push(format!("[dropped {dropped} low-signal lines]"));
    }
    if truncated {
        // Name the recovery tool so the model can pivot without inferring
        // the contract; the literal path lives in the structured
        // `spillover.path` field and the spillover footer appended after
        // shaping (see `append_spillover_footer` in `shell.rs`).
        kept.push(
            "[raw stream was truncated; recover full bytes via read_tool_output {\"path\": \"<spillover-path>\"}]"
                .to_string(),
        );
    }
    if let Some(exit_code) = exit_code
        && exit_code != 0
        && !kept.iter().any(|line| line.contains("exit_code="))
    {
        kept.push(format!("exit_code={exit_code}"));
    }
    join_shaped_lines(kept)
}

/// Cheap unified-diff detector. Looks for a hunk header (`@@ -... @@`), which
/// is the universal marker of every unified-diff-format emitter and never
/// appears as a coincidental substring in conventional command output. The
/// check is line-anchored so a stray `@@` in JSON or prose won't false-positive.
pub(crate) fn looks_like_unified_diff(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("@@ -") && line.contains(" @@"))
}

fn line_is_noise(line: &str) -> bool {
    // Cargo right-aligns its progress prefixes with leading whitespace
    // ("   Compiling foo v1.0", "    Running tests/...", "  Downloading
    // crates ..."), so prefix-match on the trimmed text or these never get
    // dropped.
    let lower = line.trim_start().to_ascii_lowercase();
    if lower.starts_with("test result:") && lower.contains("0 passed") && lower.contains("0 failed")
    {
        // Filtered cargo runs print an empty "0 passed; 0 failed" summary
        // for every test binary that matched nothing. Drop those.
        return true;
    }
    lower.starts_with("downloading ")
        || lower.starts_with("downloaded ")
        || lower.starts_with("compiling ")
        || lower.starts_with("checking ")
        || lower.starts_with("building ")
        || lower.starts_with("fresh ")
        || lower.starts_with("running ")
        || lower.contains("[          ]")
        || lower.contains("[==========]")
        || lower.contains("[----------]")
}

fn line_has_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("warning")
        || lower.contains("fail")
        || lower.contains("panic")
        || lower.contains("status")
        || lower.contains("exit")
        || lower.contains("passed")
        || lower.contains("test result")
        || lower.starts_with("finished ")
}

fn trim_shaped_block(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.trim().to_string();
    }
    let mut output = text.chars().take(max_chars).collect::<String>();
    // Name the recovery tool so the model knows to fetch the full block
    // from the shell spillover tempfile (path lives in the structured
    // `spillover.path` field and the spillover footer below).
    output.push_str(
        "\n[truncated shaped block; recover full block via read_tool_output {\"path\": \"<spillover-path>\"}]",
    );
    output
}

fn join_shaped_lines(lines: Vec<String>) -> String {
    lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
