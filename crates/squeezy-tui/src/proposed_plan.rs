//! Stream-friendly extractor for `<proposed_plan>...</proposed_plan>` blocks.
//!
//! Plan-mode replies (see `crates/squeezy-agent/src/plan_mode.rs`) end with a
//! block of the form `<proposed_plan>...</proposed_plan>`. We strip those
//! blocks out of the live assistant transcript and surface them as distinct
//! log entries so the user can see the final plan at a glance even when
//! the surrounding narration is long.
//!
//! Deltas arrive in arbitrary chunks (e.g. mid-tag splits), so the parser
//! buffers across calls. Each call to [`feed`] returns the bytes that
//! should still flow into the live assistant pane, plus any fully closed
//! plan blocks that should be promoted to log entries.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

pub(crate) const OPEN_TAG: &str = squeezy_agent::PROPOSED_PLAN_OPEN_TAG;
pub(crate) const CLOSE_TAG: &str = squeezy_agent::PROPOSED_PLAN_CLOSE_TAG;

/// Short marker prepended to Build-mode turns 2+ after a Plan→Build
/// handoff, while the same plan is still in effect. The full plan body
/// goes in on turn 1 (see `take_pending_plan_prefix` in `lib.rs`); from
/// turn 2 onward we only re-state existence so the model is reminded the
/// plan still applies without re-paying the body's tokens each turn.
/// `{path}` is replaced at use site with the active plan path.
pub(crate) const BUILD_PLAN_STILL_IN_EFFECT_FORMAT: &str = "[plan still in effect — {path}]\n\n";

/// Workspace-relative directory under which proposed plans are persisted.
/// Each session gets its own subdirectory: `<PLAN_DIR>/<session_id>/`,
/// so concurrent sessions against the same workspace cannot see each
/// other's plans (issue 11). Old flat layouts are migrated to
/// `<PLAN_DIR>/_legacy/` on startup.
pub(crate) const PLAN_DIR: &str = ".squeezy/plans";

/// Sub-directory under `<PLAN_DIR>` that old flat-layout plan files are
/// moved into the first time a session runs after the v3 storage layout
/// landed. Migration is one-shot and silent (no UX prompt).
pub(crate) const LEGACY_PLAN_DIR: &str = "_legacy";

/// File name (inside a per-session subdir) that holds the id of the
/// active plan. Single source of truth so the TUI and the agent agree
/// on which plan is "live" without each rediscovering it from mtime
/// scans (issue 17). Body is just the plan id, no front-matter.
pub(crate) const CURRENT_POINTER_FILE: &str = "current";

/// Fallback session id used when the agent has not yet minted one (e.g.
/// fresh-session boot before the first turn). Keeps the per-session
/// layout intact even in edge cases where `Agent::session_id()` returns
/// `None`. Visible in `.squeezy/plans/<id>/` on disk.
pub(crate) const FALLBACK_SESSION_ID: &str = "unassigned";

/// Stable, body-derived identifier so an unchanged plan reuses its file
/// and a refined plan lands at a new path. 12 hex chars is plenty for
/// per-workspace uniqueness; the `plan-` prefix matches the convention
/// the `plan_patch` tool already uses elsewhere in the codebase.
pub(crate) fn plan_id_for(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.trim().as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("plan-{}", &hex[..12])
}

/// Absolute path to the session's plan directory.
pub(crate) fn session_plan_dir(workspace_root: &Path, session_id: &str) -> PathBuf {
    workspace_root.join(PLAN_DIR).join(session_id)
}

/// Absolute path of a specific plan file under a session's subdirectory.
pub(crate) fn plan_file_for(workspace_root: &Path, session_id: &str, plan_id: &str) -> PathBuf {
    session_plan_dir(workspace_root, session_id).join(format!("{plan_id}.md"))
}

/// Absolute path to the `current` pointer file for a session.
pub(crate) fn current_pointer_for(workspace_root: &Path, session_id: &str) -> PathBuf {
    session_plan_dir(workspace_root, session_id).join(CURRENT_POINTER_FILE)
}

/// Read the active plan id from the `current` pointer file, if any.
/// Trims trailing whitespace and rejects empty contents.
///
/// Consumers land in PR-E (the `/plans` slash command) and PR-F (the
/// styled plan card status bar segment); the helper is exposed now so
/// pointer semantics are covered by PR-D's tests.
#[allow(dead_code)]
pub(crate) fn read_current_plan_id(workspace_root: &Path, session_id: &str) -> Option<String> {
    let pointer = current_pointer_for(workspace_root, session_id);
    let raw = fs::read_to_string(&pointer).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Write the `current` pointer file atomically (best-effort: write to
/// `<pointer>.tmp` then rename). The directory is created if missing.
fn write_current_pointer(workspace_root: &Path, session_id: &str, plan_id: &str) -> io::Result<()> {
    let dir = session_plan_dir(workspace_root, session_id);
    fs::create_dir_all(&dir)?;
    let pointer = current_pointer_for(workspace_root, session_id);
    let tmp = pointer.with_extension("tmp");
    fs::write(&tmp, format!("{plan_id}\n"))?;
    fs::rename(&tmp, &pointer)
}

/// Maximum number of plan files kept under each session's plan dir.
/// Pruning runs at session start so the directory cannot grow unbounded
/// across many turns; the cap is high enough that recent plans always
/// survive a normal day of work.
pub(crate) const PLAN_RETENTION_LIMIT: usize = 20;

/// Trim a session's plan dir to at most [`PLAN_RETENTION_LIMIT`] markdown
/// files, keeping the newest by mtime. Returns the number of files
/// deleted; `0` when the dir is missing, empty, or already under the
/// limit. Read errors are silently treated as "nothing to prune" so a
/// permissions issue can never crash session startup. The `current`
/// pointer file is always preserved regardless of mtime. Plan ids in
/// `protected` are also kept regardless of mtime — used for git-aware
/// retention (PR-H, issue 13).
pub(crate) fn prune_plan_dir(
    workspace_root: &Path,
    session_id: &str,
    protected: &HashSet<String>,
) -> usize {
    let dir = session_plan_dir(workspace_root, session_id);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut plans: Vec<(std::time::SystemTime, PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let plan_id = path.file_stem()?.to_string_lossy().to_string();
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some((modified, path, plan_id))
        })
        .collect();
    if plans.len() <= PLAN_RETENTION_LIMIT {
        return 0;
    }
    plans.sort_by_key(|(modified, _, _)| std::cmp::Reverse(*modified));
    let mut deleted = 0;
    // Newest PLAN_RETENTION_LIMIT survive unconditionally; older plans
    // are deleted unless the id appears in `protected`.
    for (_, path, plan_id) in plans.into_iter().skip(PLAN_RETENTION_LIMIT) {
        if protected.contains(&plan_id) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

/// Collect plan ids (`plan-<hex>`) that appear anywhere in the last
/// `days` of `git log -p` output for the given workspace. Used as the
/// `protected` set for [`prune_plan_dir`] so plan files referenced in
/// recent commit messages or diffs survive retention even when older
/// than [`PLAN_RETENTION_LIMIT`] siblings (PR-H, issue 13).
///
/// Returns an empty set when git is unavailable, the workspace is not
/// a git repo, or the command fails — pruning then falls back to its
/// previous mtime-only behaviour.
pub(crate) fn git_referenced_plan_ids(workspace_root: &Path, days: u32) -> HashSet<String> {
    let since = format!("{days} days ago");
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("log")
        .arg("-p")
        .arg("--no-color")
        .arg("--since")
        .arg(&since)
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    extract_plan_ids(&text)
}

/// Scan free-form text for `plan-<hex>` tokens. Used by the git-aware
/// pruning helper to detect plan ids referenced in commit messages and
/// diff bodies. The hex tail must be 1+ lowercase hex chars; trailing
/// punctuation is ignored.
pub(crate) fn extract_plan_ids(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut idx = 0;
    let bytes = text.as_bytes();
    while idx < bytes.len() {
        let Some(pos) = text[idx..].find("plan-") else {
            break;
        };
        let start = idx + pos;
        // Reject when preceded by an ASCII alphanumeric (avoid matching
        // mid-identifier `applan-foo`).
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() {
                idx = start + "plan-".len();
                continue;
            }
        }
        let tail_start = start + "plan-".len();
        let mut tail_end = tail_start;
        while tail_end < bytes.len()
            && (bytes[tail_end].is_ascii_hexdigit() && !bytes[tail_end].is_ascii_uppercase())
        {
            tail_end += 1;
        }
        if tail_end > tail_start {
            out.insert(text[start..tail_end].to_string());
        }
        idx = tail_end.max(start + "plan-".len());
    }
    out
}

/// One-shot migration of pre-v3 flat-layout plan files. Any `*.md` file
/// directly under `<workspace>/.squeezy/plans/` (i.e. NOT inside a
/// session subdir) is moved to `<workspace>/.squeezy/plans/_legacy/`.
/// Subdirectories are left alone. Returns the number of files moved;
/// `0` when there is nothing to migrate or the dir does not exist.
pub(crate) fn migrate_legacy_plans(workspace_root: &Path) -> usize {
    let dir = workspace_root.join(PLAN_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let legacy_dir = dir.join(LEGACY_PLAN_DIR);
    let mut moved = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Only top-level .md files; ignore subdirectories (session dirs
        // and the legacy dir itself).
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        if fs::create_dir_all(&legacy_dir).is_err() {
            return moved;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let dest = legacy_dir.join(file_name);
        if fs::rename(&path, &dest).is_ok() {
            moved += 1;
        }
    }
    moved
}

/// Metadata baked into a persisted plan file's YAML front-matter (PR-D).
/// All fields are optional except those that the persist helper can
/// derive itself (plan id, session id, created timestamp).
#[derive(Debug, Default, Clone)]
pub(crate) struct PlanMeta {
    /// Identifier of the plan this one refines, set by the TUI when the
    /// active plan is being replaced. Mirrors clear-code's parent-of
    /// pointer; used by the styled card (PR-F) to render diffs.
    pub parent_plan_id: Option<String>,
    /// Free-form model id (e.g. `gpt-5-codex`). Captured at persist time
    /// for retrospective debugging — not used by any runtime gate.
    pub model: Option<String>,
}

/// Persist a proposed plan body under the session's plan dir at
/// `<workspace>/.squeezy/plans/<session_id>/<plan_id>.md`, and update
/// the session's `current` pointer to the new plan id. Returns the
/// plan id and the absolute path. The file gets a YAML front-matter
/// block (plan id, session id, optional parent / model, objective and
/// created timestamp) followed by the verbatim body so editors,
/// `read_file`, and `apply_patch` all round-trip cleanly.
pub(crate) fn persist_plan(
    workspace_root: &Path,
    session_id: &str,
    body: &str,
    meta: &PlanMeta,
) -> io::Result<(String, PathBuf)> {
    let plan_id = plan_id_for(body);
    let path = plan_file_for(workspace_root, session_id, &plan_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body_trimmed = body.trim_end().to_string();
    let front_matter = render_front_matter(&plan_id, session_id, meta, &body_trimmed);
    let mut contents = front_matter;
    contents.push_str(&body_trimmed);
    contents.push('\n');
    fs::write(&path, contents)?;
    // Best-effort pointer update; persistence is still considered
    // successful if the pointer write fails (the file itself landed).
    let _ = write_current_pointer(workspace_root, session_id, &plan_id);
    Ok((plan_id, path))
}

/// Build the YAML front-matter block for [`persist_plan`]. Always
/// includes `plan_id`, `session_id`, `objective` (first non-empty line
/// of the body, truncated to [`OBJECTIVE_TRUNCATE`] chars) and
/// `created` (RFC3339). `parent_plan_id` and `model` are emitted only
/// when set on `meta`.
fn render_front_matter(plan_id: &str, session_id: &str, meta: &PlanMeta, body: &str) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("plan_id: {plan_id}\n"));
    out.push_str(&format!("session_id: {}\n", yaml_scalar(session_id)));
    if let Some(parent) = meta.parent_plan_id.as_deref() {
        out.push_str(&format!("parent_plan_id: {parent}\n"));
    }
    if let Some(model) = meta.model.as_deref() {
        out.push_str(&format!("model: {}\n", yaml_scalar(model)));
    }
    out.push_str(&format!(
        "objective: {}\n",
        yaml_scalar(&derive_objective(body))
    ));
    out.push_str(&format!("created: {}\n", current_rfc3339()));
    out.push_str("---\n");
    out
}

/// Cap on the `objective:` front-matter field. 80 chars is enough for a
/// human-readable summary in `/plans list` (PR-E) while keeping the
/// front-matter compact.
const OBJECTIVE_TRUNCATE: usize = 80;

/// Pick a one-line objective from the plan body: the first non-empty
/// non-`#` line, with leading list/quote markers stripped and a hard
/// cap at [`OBJECTIVE_TRUNCATE`] codepoints (ellipsised when exceeded).
fn derive_objective(body: &str) -> String {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let cleaned = strip_leading_marker(line);
        if cleaned.is_empty() {
            continue;
        }
        let mut iter = cleaned.chars();
        let truncated: String = (&mut iter).take(OBJECTIVE_TRUNCATE).collect();
        return if iter.next().is_some() {
            format!("{}…", truncated.trim_end())
        } else {
            truncated
        };
    }
    String::new()
}

/// Strip common markdown list/quote/heading prefixes so the objective
/// reads like prose, not a marked-up first step.
fn strip_leading_marker(line: &str) -> String {
    let mut s = line.trim_start();
    // Block-quote.
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    // Heading (`#`, `##`, ...).
    while let Some(rest) = s.strip_prefix('#') {
        s = rest.trim_start();
    }
    // Numbered list (`1.`, `12)`).
    if let Some(idx) = s
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        && idx > 0
        && let Some(rest) = s[idx..]
            .strip_prefix('.')
            .or_else(|| s[idx..].strip_prefix(')'))
    {
        s = rest.trim_start();
    }
    // Bullet markers.
    for marker in ["- [ ] ", "- [x] ", "* ", "- ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            s = rest.trim_start();
            break;
        }
    }
    s.to_string()
}

/// Lightweight YAML scalar quoter. The front-matter values we emit are
/// short single-line strings, so quoting only kicks in when the value
/// contains a character the YAML 1.1 plain-scalar grammar would
/// misread (`:`, `#`, leading/trailing whitespace, a leading reserved
/// indicator). Quoted output uses single quotes with `''` escaping —
/// the safest YAML-1.1 form for arbitrary printable strings.
fn yaml_scalar(value: &str) -> String {
    let needs_quote = value.is_empty()
        || value.starts_with(|c: char| {
            matches!(
                c,
                '!' | '&' | '*' | '?' | '|' | '>' | '\'' | '"' | '%' | '@' | '`' | '#'
            ) || c.is_whitespace()
        })
        || value.ends_with(char::is_whitespace)
        || value.contains(':')
        || value.contains('#')
        || value.contains('\n');
    if !needs_quote {
        return value.to_string();
    }
    let escaped = value.replace('\'', "''");
    format!("'{escaped}'")
}

/// RFC3339 timestamp without bringing in a calendar crate. We render
/// UTC with second precision so plans sort lexicographically.
fn current_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_seconds_utc(now)
}

/// Format `secs` (Unix epoch seconds, UTC) as RFC3339 `YYYY-MM-DDTHH:MM:SSZ`.
/// Uses the proleptic Gregorian civil-from-days algorithm so we don't
/// pull in `chrono`/`time` just for one timestamp.
fn format_unix_seconds_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm (proleptic Gregorian).
/// Returns `(year, month [1-12], day [1-31])` for the day index, where
/// day 0 = 1970-01-01 UTC.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Strip a YAML front-matter block, if present, from a plan file's
/// contents. Recognises the standard `---\n...\n---\n` envelope at the
/// very start of the input. Returns the body slice; when no front-matter
/// is present (legacy files, partial writes), returns the input
/// untouched.
///
/// Consumed by [`read_plan_body`] and by PR-F's styled card; exposed
/// now so PR-D can lock the round-trip semantics in tests.
#[allow(dead_code)]
pub(crate) fn strip_front_matter(contents: &str) -> &str {
    let Some(after_open) = contents.strip_prefix("---\n") else {
        return contents;
    };
    let Some(close_rel) = after_open.find("\n---") else {
        return contents;
    };
    let after_close = &after_open[close_rel + "\n---".len()..];
    // Allow either `\n---\n<body>` or trailing-only `\n---` (EOF).
    if let Some(stripped) = after_close.strip_prefix('\n') {
        stripped
    } else if after_close.is_empty() {
        ""
    } else {
        // The closer wasn't followed by a newline or EOF; treat the
        // file as having no real front-matter to be safe.
        contents
    }
}

/// Read a plan file from disk and return just the body (front-matter
/// stripped). Used by renderers (PR-F) so the styled card never shows
/// YAML metadata to the user. Returns the file's full contents on
/// parse failure so display never silently empties.
#[allow(dead_code)]
pub(crate) fn read_plan_body(path: &Path) -> io::Result<String> {
    let contents = fs::read_to_string(path)?;
    Ok(strip_front_matter(&contents).to_string())
}

/// Summary record returned by [`list_plans`]. Sourced from the on-disk
/// YAML front-matter where present; falls back to mtime / empty
/// objective for legacy files that lack front-matter.
#[derive(Debug, Clone)]
pub(crate) struct PlanListEntry {
    pub plan_id: String,
    /// Absolute path on disk. Held so callers (PR-F renderers) can avoid
    /// recomputing it; the `#[allow]` is here because PR-E only needs
    /// the id + objective.
    #[allow(dead_code)]
    pub path: PathBuf,
    pub modified: SystemTime,
    pub objective: String,
    pub is_active: bool,
}

/// List every plan file under the session's plan dir, newest first.
/// Marks the entry pointed at by `current` as active. Returns an empty
/// vec when the dir is missing or empty (never errors — callers render
/// "no plans" instead).
pub(crate) fn list_plans(workspace_root: &Path, session_id: &str) -> Vec<PlanListEntry> {
    let dir = session_plan_dir(workspace_root, session_id);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let active_id = read_current_plan_id(workspace_root, session_id);
    let mut out: Vec<PlanListEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let plan_id = path.file_stem()?.to_string_lossy().to_string();
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let objective = read_plan_objective(&path).unwrap_or_default();
            let is_active = active_id.as_deref() == Some(plan_id.as_str());
            Some(PlanListEntry {
                plan_id,
                path,
                modified,
                objective,
                is_active,
            })
        })
        .collect();
    out.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    out
}

/// Pull the `objective:` field out of a plan file's front-matter
/// without loading a full YAML parser. Returns `None` when the file is
/// unreadable, has no front-matter, or has no `objective` key.
fn read_plan_objective(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let after_open = contents.strip_prefix("---\n")?;
    let close_rel = after_open.find("\n---")?;
    let block = &after_open[..close_rel];
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("objective: ") {
            return Some(unquote_yaml_scalar(value.trim()));
        }
    }
    None
}

/// Inverse of [`yaml_scalar`] for the small subset we emit: single-
/// quoted strings with `''` escaping, or bare scalars.
fn unquote_yaml_scalar(raw: &str) -> String {
    if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2 {
        raw[1..raw.len() - 1].replace("''", "'")
    } else {
        raw.to_string()
    }
}

/// Error from a plan-id prefix lookup ([`resolve_plan_prefix`]).
#[derive(Debug)]
pub(crate) enum PlanLookupError {
    /// No plan id in the session matches the prefix.
    NotFound,
    /// More than one plan id matches the prefix. Holds the matching ids
    /// so the caller can render them for disambiguation.
    Ambiguous(Vec<String>),
}

/// Resolve a possibly-truncated plan id to an exact one inside the
/// session's plan dir. Accepts the full `plan-<hex>` form, just the
/// hex tail, or any unique prefix of either.
pub(crate) fn resolve_plan_prefix(
    workspace_root: &Path,
    session_id: &str,
    needle: &str,
) -> Result<String, PlanLookupError> {
    let plans = list_plans(workspace_root, session_id);
    let needle = needle.trim();
    if needle.is_empty() {
        return Err(PlanLookupError::NotFound);
    }
    // Exact match wins, no matter how many other prefixes match.
    if let Some(entry) = plans.iter().find(|entry| entry.plan_id == needle) {
        return Ok(entry.plan_id.clone());
    }
    let hex_needle = needle.strip_prefix("plan-").unwrap_or(needle);
    let matches: Vec<String> = plans
        .iter()
        .filter(|entry| {
            let hex = entry
                .plan_id
                .strip_prefix("plan-")
                .unwrap_or(&entry.plan_id);
            entry.plan_id.starts_with(needle) || hex.starts_with(hex_needle)
        })
        .map(|entry| entry.plan_id.clone())
        .collect();
    match matches.len() {
        0 => Err(PlanLookupError::NotFound),
        1 => Ok(matches.into_iter().next().expect("len==1")),
        _ => Err(PlanLookupError::Ambiguous(matches)),
    }
}

/// Delete a plan file (and clear the `current` pointer if it referenced
/// this plan). Returns the absolute path that was removed so callers
/// can log it. Pointer cleanup is best-effort: a failure there does
/// not roll back the file removal.
pub(crate) fn delete_plan(
    workspace_root: &Path,
    session_id: &str,
    plan_id: &str,
) -> io::Result<PathBuf> {
    let path = plan_file_for(workspace_root, session_id, plan_id);
    fs::remove_file(&path)?;
    if read_current_plan_id(workspace_root, session_id).as_deref() == Some(plan_id) {
        let pointer = current_pointer_for(workspace_root, session_id);
        let _ = fs::remove_file(&pointer);
    }
    Ok(path)
}

/// Rewrite the `current` pointer to designate `plan_id` as the active
/// plan. The plan file must already exist on disk; returns an error
/// when it doesn't so we never aim the pointer at a phantom.
pub(crate) fn set_active_plan(
    workspace_root: &Path,
    session_id: &str,
    plan_id: &str,
) -> io::Result<()> {
    let path = plan_file_for(workspace_root, session_id, plan_id);
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("plan {plan_id} not found in session {session_id}"),
        ));
    }
    write_current_pointer(workspace_root, session_id, plan_id)
}

#[derive(Debug, Default)]
pub(crate) struct ProposedPlanExtractor {
    /// Bytes accumulated inside an unclosed `<proposed_plan>` block.
    inside: Option<String>,
    /// Bytes that *might* be a partial open/close tag straddling a delta
    /// boundary. Always equal to a strict prefix of `OPEN_TAG` or
    /// `CLOSE_TAG` (whichever the current state expects).
    pending_tag: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProposedPlanFeed {
    /// Bytes that should be appended to the live assistant text.
    pub passthrough: String,
    /// Fully-extracted plan bodies (without surrounding tags) closed by
    /// this delta.
    pub completed: Vec<String>,
}

impl ProposedPlanExtractor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// True when a `<proposed_plan>` block is currently open (open tag seen,
    /// close tag not yet seen).
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.inside.is_some()
    }

    /// Feed a streaming delta. The returned `passthrough` should be appended
    /// to the live assistant buffer; `completed` holds finalised plan
    /// bodies (without tags), already trimmed of leading/trailing newlines.
    pub(crate) fn feed(&mut self, delta: &str) -> ProposedPlanFeed {
        let mut out = ProposedPlanFeed::default();
        let mut remaining = delta;
        while !remaining.is_empty() {
            if self.inside.is_some() {
                // Looking for CLOSE_TAG; pending_tag holds a possible partial.
                let mut buf = std::mem::take(&mut self.pending_tag);
                buf.push_str(remaining);
                match buf.find(CLOSE_TAG) {
                    Some(idx) => {
                        let body = &buf[..idx];
                        let inside = self.inside.as_mut().expect("inside guarded above");
                        inside.push_str(body);
                        let completed = std::mem::take(inside).trim().to_string();
                        out.completed.push(completed);
                        self.inside = None;
                        // Resume scanning after the close tag.
                        remaining = remaining_after_match(remaining, &buf, idx + CLOSE_TAG.len());
                    }
                    None => {
                        let safe_len = safe_emit_len(&buf, CLOSE_TAG);
                        let inside = self.inside.as_mut().expect("inside guarded above");
                        inside.push_str(&buf[..safe_len]);
                        self.pending_tag = buf[safe_len..].to_string();
                        remaining = "";
                    }
                }
            } else {
                // Outside any block; looking for OPEN_TAG.
                let mut buf = std::mem::take(&mut self.pending_tag);
                buf.push_str(remaining);
                match buf.find(OPEN_TAG) {
                    Some(idx) => {
                        out.passthrough.push_str(&buf[..idx]);
                        self.inside = Some(String::new());
                        remaining = remaining_after_match(remaining, &buf, idx + OPEN_TAG.len());
                    }
                    None => {
                        let safe_len = safe_emit_len(&buf, OPEN_TAG);
                        out.passthrough.push_str(&buf[..safe_len]);
                        self.pending_tag = buf[safe_len..].to_string();
                        remaining = "";
                    }
                }
            }
        }
        out
    }

    /// Flush any unterminated state. Called when the turn ends.
    /// Returns any text that should still flow into the assistant pane
    /// (an unterminated open tag is treated as plain text — better to
    /// show garbled markers than to silently drop the trailing narration).
    pub(crate) fn finalize(&mut self) -> String {
        // If we are inside an unterminated block, drop it — the audit's
        // contract says exactly one block per turn, so a missing close
        // tag is a model bug. Surface the open marker so a user can spot
        // the issue rather than getting silence.
        let mut leftover = String::new();
        if self.inside.is_some() {
            leftover.push_str(OPEN_TAG);
            let body = self.inside.take().expect("guarded");
            leftover.push_str(&body);
        }
        leftover.push_str(&std::mem::take(&mut self.pending_tag));
        leftover
    }
}

/// Number of bytes from the head of `buf` that can safely flow downstream
/// without losing a partial occurrence of `needle` straddling the
/// boundary. We keep up to `needle.len() - 1` bytes of trailing data in
/// `pending_tag`, but only if those bytes actually match a non-empty
/// prefix of `needle`. This avoids growing `pending_tag` unboundedly when
/// a delta ends with characters that simply happen to overlap part of
/// `needle`.
fn safe_emit_len(buf: &str, needle: &str) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let max_keep = needle.len().saturating_sub(1);
    let mut keep = buf.len().min(max_keep);
    while keep > 0 {
        // `buf.len() - keep` must land on a char boundary for slicing to
        // be safe.
        if buf.is_char_boundary(buf.len() - keep) {
            let tail = &buf[buf.len() - keep..];
            if needle.starts_with(tail) {
                break;
            }
        }
        keep -= 1;
    }
    buf.len() - keep
}

/// Given the original `delta`, the combined `buf` (pending_tag + delta),
/// and the index *into `buf`* just past a match, return the slice of
/// `delta` that follows the match. When the match lay entirely inside
/// the previously-buffered `pending_tag`, this returns the whole `delta`
/// because no bytes of `delta` were consumed yet.
fn remaining_after_match<'a>(delta: &'a str, buf: &str, end_in_buf: usize) -> &'a str {
    let prefix_len = buf.len() - delta.len();
    if end_in_buf <= prefix_len {
        delta
    } else {
        &delta[end_in_buf - prefix_len..]
    }
}

#[cfg(test)]
#[path = "proposed_plan_tests.rs"]
mod tests;
