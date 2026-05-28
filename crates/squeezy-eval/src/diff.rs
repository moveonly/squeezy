//! `squeezy-eval diff` — compare two run directories.
//!
//! Reads `<run>/{run.json, trace.jsonl, frames.jsonl}` from each side
//! and emits a markdown delta covering totals, per-turn tool-call set
//! difference, frame text diff, and findings delta.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

use serde::Deserialize;

use crate::driver::EvalError;
use crate::findings::Finding;
use crate::frames::{FrameRecord, ToolCallSummary};

#[derive(Debug, Clone, Copy, Default)]
pub enum DiffFormat {
    #[default]
    Markdown,
    Json,
}

impl DiffFormat {
    pub fn parse(s: &str) -> Self {
        match s {
            "json" => DiffFormat::Json,
            _ => DiffFormat::Markdown,
        }
    }
}

pub fn diff_runs(a: &Path, b: &Path, format: DiffFormat) -> Result<String, EvalError> {
    diff_runs_with_schema_check(a, b, format, false)
}

/// Like [`diff_runs`] but with an optional schema-version check.
/// When `schema_check` is true, the diff bails with a clear error if
/// the two runs' trace `schema_version` (v2 vs v3+) differ. Useful
/// in CI where a v2/v3 mix would otherwise produce confusing
/// "missing variant" diffs.
pub fn diff_runs_with_schema_check(
    a: &Path,
    b: &Path,
    format: DiffFormat,
    schema_check: bool,
) -> Result<String, EvalError> {
    let run_a = RunSnapshot::load(a)?;
    let run_b = RunSnapshot::load(b)?;
    if schema_check {
        let sa = read_schema_version(&run_a.dir.join("trace.jsonl")).unwrap_or(2);
        let sb = read_schema_version(&run_b.dir.join("trace.jsonl")).unwrap_or(2);
        if sa != sb {
            return Err(EvalError::Internal(format!(
                "trace schema mismatch: {sa} vs {sb}. Re-run the older side or pass --no-schema-check to diff anyway."
            )));
        }
    }
    match format {
        DiffFormat::Markdown => Ok(render_markdown(&run_a, &run_b)),
        DiffFormat::Json => Ok(render_json(&run_a, &run_b)),
    }
}

/// Read the first non-empty line of a trace.jsonl and parse its
/// `schema_version` field. Returns `None` if the file is empty or
/// malformed; callers should default to schema v2.
fn read_schema_version(path: &Path) -> Option<u32> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).ok()?;
        return value
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
    }
    None
}

struct RunSnapshot {
    dir: std::path::PathBuf,
    manifest: Manifest,
    frames: Vec<FrameRecord>,
    findings: Vec<Finding>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct Manifest {
    #[serde(default)]
    scenario: ManifestScenario,
    #[serde(default)]
    totals: ManifestTotals,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestScenario {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestTotals {
    #[serde(default)]
    trace_events: u64,
    #[serde(default)]
    frames: u64,
    #[serde(default)]
    cost_micro_usd: u64,
    #[serde(default)]
    findings: u64,
}

impl RunSnapshot {
    fn load(dir: &Path) -> Result<Self, EvalError> {
        let manifest_path = dir.join("run.json");
        let manifest_bytes = std::fs::read(&manifest_path)
            .map_err(|err| EvalError::Io(format!("read {manifest_path:?}: {err}")))?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|err| EvalError::Internal(format!("parse {manifest_path:?}: {err}")))?;
        let frames = read_jsonl::<FrameRecord>(&dir.join("frames.jsonl"))?;
        let findings = read_jsonl::<Finding>(&dir.join("findings.jsonl")).unwrap_or_default();
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            frames,
            findings,
        })
    }
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>, EvalError> {
    if !path.exists() {
        return Ok(vec![]);
    }
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let item: T = serde_json::from_str(&line)
            .map_err(|err| EvalError::Internal(format!("parse {path:?}: {err}")))?;
        out.push(item);
    }
    Ok(out)
}

fn render_markdown(a: &RunSnapshot, b: &RunSnapshot) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# squeezy-eval diff");
    let _ = writeln!(out);
    let _ = writeln!(out, "- **A:** `{}`", a.dir.display());
    let _ = writeln!(out, "- **B:** `{}`", b.dir.display());
    let _ = writeln!(out);

    if a.manifest.scenario.id != b.manifest.scenario.id {
        let _ = writeln!(
            out,
            "> ⚠️ scenario id differs: A=`{}` B=`{}`",
            a.manifest.scenario.id, b.manifest.scenario.id
        );
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Totals");
    let _ = writeln!(out, "| metric | A | B | Δ |\n|---|---:|---:|---:|");
    write_metric_row(
        &mut out,
        "trace_events",
        a.manifest.totals.trace_events as i128,
        b.manifest.totals.trace_events as i128,
    );
    write_metric_row(
        &mut out,
        "frames",
        a.manifest.totals.frames as i128,
        b.manifest.totals.frames as i128,
    );
    write_metric_row(
        &mut out,
        "cost_micro_usd",
        a.manifest.totals.cost_micro_usd as i128,
        b.manifest.totals.cost_micro_usd as i128,
    );
    write_metric_row(
        &mut out,
        "findings",
        a.manifest.totals.findings as i128,
        b.manifest.totals.findings as i128,
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Tool-call set delta per turn");
    let pairs = align_frames(&a.frames, &b.frames);
    for pair in &pairs {
        let (a_set, b_set) = (
            tool_set(pair.a.map(|f| &f.tool_calls).unwrap_or(&vec![])),
            tool_set(pair.b.map(|f| &f.tool_calls).unwrap_or(&vec![])),
        );
        let added: Vec<_> = b_set.difference(&a_set).collect();
        let removed: Vec<_> = a_set.difference(&b_set).collect();
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        let _ = writeln!(out, "### {}", pair.label);
        for entry in &removed {
            let _ = writeln!(out, "- `-` {} `{}`", entry.0, short_sha(&entry.1));
        }
        for entry in &added {
            let _ = writeln!(out, "- `+` {} `{}`", entry.0, short_sha(&entry.1));
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Frame text diff");
    for pair in &pairs {
        if let (Some(fa), Some(fb)) = (pair.a, pair.b) {
            if fa.assistant_text == fb.assistant_text {
                continue;
            }
            let _ = writeln!(out, "### {}", pair.label);
            let _ = writeln!(out, "```diff");
            let _ = writeln!(
                out,
                "{}",
                unified_text_diff(&fa.assistant_text, &fb.assistant_text)
            );
            let _ = writeln!(out, "```");
            let _ = writeln!(out);
        } else if pair.a.is_some() {
            let _ = writeln!(out, "### {} — removed in B", pair.label);
        } else if pair.b.is_some() {
            let _ = writeln!(out, "### {} — added in B", pair.label);
        }
    }

    let _ = writeln!(out, "## Findings delta");
    let a_rules: BTreeSet<_> = a.findings.iter().map(|f| f.rule_id.clone()).collect();
    let b_rules: BTreeSet<_> = b.findings.iter().map(|f| f.rule_id.clone()).collect();
    for new_rule in b_rules.difference(&a_rules) {
        let _ = writeln!(out, "- ➕ new: `{new_rule}`");
    }
    for resolved in a_rules.difference(&b_rules) {
        let _ = writeln!(out, "- ✅ resolved: `{resolved}`");
    }
    if a_rules == b_rules {
        let _ = writeln!(out, "- (no change)");
    }
    out
}

fn render_json(a: &RunSnapshot, b: &RunSnapshot) -> String {
    let value = serde_json::json!({
        "a": a.dir.display().to_string(),
        "b": b.dir.display().to_string(),
        "totals": {
            "trace_events": [a.manifest.totals.trace_events, b.manifest.totals.trace_events],
            "frames": [a.manifest.totals.frames, b.manifest.totals.frames],
            "cost_micro_usd": [a.manifest.totals.cost_micro_usd, b.manifest.totals.cost_micro_usd],
            "findings": [a.manifest.totals.findings, b.manifest.totals.findings],
        },
        "findings_a": a.findings.iter().map(|f| &f.rule_id).collect::<Vec<_>>(),
        "findings_b": b.findings.iter().map(|f| &f.rule_id).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

fn write_metric_row(out: &mut String, name: &str, a: i128, b: i128) {
    let _ = writeln!(out, "| {name} | {a} | {b} | {:+} |", b - a);
}

fn tool_set(calls: &[ToolCallSummary]) -> BTreeSet<(String, String)> {
    calls
        .iter()
        .map(|c| (c.name.clone(), c.args_sha256.clone()))
        .collect()
}

fn short_sha(s: &str) -> String {
    s.chars().take(8).collect()
}

struct AlignedPair<'a> {
    label: String,
    a: Option<&'a FrameRecord>,
    b: Option<&'a FrameRecord>,
}

fn align_frames<'a>(a: &'a [FrameRecord], b: &'a [FrameRecord]) -> Vec<AlignedPair<'a>> {
    let mut pairs = Vec::new();
    let max = a.len().max(b.len());
    for i in 0..max {
        pairs.push(AlignedPair {
            label: format!("turn #{}", i + 1),
            a: a.get(i),
            b: b.get(i),
        });
    }
    pairs
}

fn unified_text_diff(old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
    }
    out
}
