//! Error Lenses (§12.5.6): detect actionable error lines inside failed tool
//! outputs and surface them as highlighted, navigable, quick-jumpable
//! [`ErrorLens`] entries. A lens carries the entry it lives in, the offending
//! line text, a classified [`ErrorClass`] (rustc / cargo / test / permission /
//! network / panic / sandbox), a [`ErrorSeverity`], a short message, and — when
//! the line names one — an extracted `file:line[:col]` [`ErrorLocation`] for
//! quick-jump.
//!
//! **Structured status wins, detectors fill in.** The caller only feeds the
//! *text* of an entry whose structured status already says it failed (see
//! `entry_is_error` in `lib.rs`), so the brittle regex/substring detectors here
//! never have to decide *whether* an entry failed — only *which lines within it*
//! are the actionable error lines and *what kind* they are. That keeps the
//! false-positive surface small: a success entry never reaches the detector.
//!
//! **Stable ids, never row offsets.** Like the transcript index (§12.5.1), the
//! relation graph (§12.5.3), and the duplicate-fold model (§12.5.4), every lens
//! is keyed by its source `TranscriptEntry::id`, never a width-/fold-dependent
//! row coordinate. An id survives reflow (resize, streaming, collapse,
//! coalescing), so a lens built before a reflow still resolves to the right
//! entry afterwards. Ids whose entry was dropped fall out on the next rebuild.
//!
//! **Zero idle cost, incremental rebuild.** The model carries a `fingerprint`
//! folded over every failed candidate `(id, revision)`. The caller feeds the
//! same fingerprint each refresh via [`ErrorLenses::rebuild_if_stale`]; when it
//! matches the stored one the call returns immediately and touches nothing. The
//! detectors only re-run when the transcript actually changed — exactly the
//! events that move the fingerprint. An idle session pays one cheap `u64`
//! comparison per refresh.
//!
//! This module is deliberately pure: it owns the detection + lens bookkeeping
//! and nothing about geometry, rendering, or input. `lib.rs` collects each
//! failed entry's id, revision, and output text into an [`ErrorCandidate`] and
//! feeds the slice in; this module scans the text into lenses and answers
//! list/navigation queries. That keeps the detection math testable without a
//! terminal.

use std::hash::{Hash, Hasher};

/// The classified kind of an error line. A small, fixed set — one per detector
/// family the spec calls out (rustc/cargo/tests/permission/network/panic/
/// sandbox) plus a `Generic` catch-all for a failed entry whose lines match no
/// specific pattern. Ordered so [`ErrorClass::ALL`] reads the way the overlay
/// groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ErrorClass {
    /// A Rust compiler diagnostic line (`error[E0277]: ...`, `error: ...`).
    Rustc,
    /// A cargo build/driver failure (`error: could not compile ...`).
    Cargo,
    /// A test-runner failure (`test foo ... FAILED`, `assertion failed`,
    /// `panicked at`, `FAIL`/`AssertionError`).
    TestFailure,
    /// A filesystem permission / access-denied error (`Permission denied`,
    /// `EACCES`, `access is denied`).
    Permission,
    /// A network failure (`Connection refused`, `could not resolve host`,
    /// `timed out`, `ETIMEDOUT`).
    Network,
    /// A runtime panic / fatal abort (`thread '...' panicked`, `fatal error`,
    /// `Segmentation fault`).
    Panic,
    /// A sandbox / policy denial (`sandbox`, `operation not permitted`,
    /// `denied by policy`, codesign/notarization-style refusals).
    Sandbox,
    /// A failed entry whose error line matched no specific detector — still
    /// surfaced (the entry *did* fail) but unclassified.
    Generic,
}

impl ErrorClass {
    /// Every class, in overlay grouping order. Drives the summary readout.
    /// Exhaustive on purpose: a new variant must be added here or it never
    /// appears in the summary.
    pub(crate) const ALL: &'static [ErrorClass] = &[
        ErrorClass::Rustc,
        ErrorClass::Cargo,
        ErrorClass::TestFailure,
        ErrorClass::Permission,
        ErrorClass::Network,
        ErrorClass::Panic,
        ErrorClass::Sandbox,
        ErrorClass::Generic,
    ];

    /// Short, screen-reader-friendly label. ASCII only (no glyphs) to match the
    /// rest of Squeezy's chrome.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ErrorClass::Rustc => "rustc",
            ErrorClass::Cargo => "cargo",
            ErrorClass::TestFailure => "test",
            ErrorClass::Permission => "permission",
            ErrorClass::Network => "network",
            ErrorClass::Panic => "panic",
            ErrorClass::Sandbox => "sandbox",
            ErrorClass::Generic => "error",
        }
    }
}

/// How loud an error line is. `Error` is an outright failure; `Warning` is a
/// diagnostic that the detectors surface but that did not by itself fail the
/// entry (e.g. a `warning:` line inside a failed compile). Ordered so an error
/// sorts ahead of a warning when both appear in the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ErrorSeverity {
    /// A hard failure line.
    Error,
    /// A non-fatal diagnostic surfaced alongside the failure.
    Warning,
}

impl ErrorSeverity {
    /// ASCII label for the readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ErrorSeverity::Error => "error",
            ErrorSeverity::Warning => "warn",
        }
    }
}

/// An extracted `file:line[:col]` source location from an error line. `path` is
/// the display path exactly as it appeared (never normalized — we preserve the
/// original platform meaning), `line`/`col` the 1-based coordinates. `col` is
/// `None` when the line only named `file:line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorLocation {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) col: Option<u32>,
}

impl ErrorLocation {
    /// Compact `path:line` / `path:line:col` rendering for the readout.
    pub(crate) fn display(&self) -> String {
        match self.col {
            Some(col) => format!("{}:{}:{}", self.path, self.line, col),
            None => format!("{}:{}", self.path, self.line),
        }
    }
}

/// One detected actionable error line (§12.5.6). `entry_id` is the stable
/// `TranscriptEntry::id` of the failed entry it lives in (the quick-jump
/// target); `line_index` is its 0-based line offset within that entry's output
/// (for stable ordering and copy); `class`/`severity` classify it; `message` is
/// the trimmed line text (bounded); `location` is the extracted `file:line` when
/// the line named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorLens {
    pub(crate) entry_id: u64,
    pub(crate) line_index: usize,
    pub(crate) class: ErrorClass,
    pub(crate) severity: ErrorSeverity,
    pub(crate) message: String,
    pub(crate) location: Option<ErrorLocation>,
}

/// One failed-entry candidate the caller feeds in. `id` is the stable
/// `TranscriptEntry::id`; `revision` is its content revision (folded into the
/// staleness fingerprint so a mutation re-detects); `text` is the entry's
/// human-visible output to scan for error lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorCandidate {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    pub(crate) text: String,
}

/// Largest number of characters retained in a lens `message`. Long output lines
/// (a giant single-line JSON blob, a base64 dump) would otherwise blow up the
/// overlay row; we keep a generous-but-bounded prefix.
const MESSAGE_CAP: usize = 200;

/// Largest number of lenses kept for a single entry. A pathological failure that
/// prints thousands of "error:" lines should not produce thousands of overlay
/// rows; we keep the first few per entry (the actionable head of the failure).
const LENSES_PER_ENTRY_CAP: usize = 8;

/// Strip ANSI/VT escape sequences (CSI `\x1b[...m` and a bare two-byte escape)
/// so color/cursor control does not defeat the detectors or pollute the message.
/// Self-contained so the pure module has no dependency on the renderer's
/// stripper.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Truncate `s` to at most [`MESSAGE_CAP`] chars (on a char boundary), appending
/// an ellipsis when it was cut.
fn cap_message(s: &str) -> String {
    if s.chars().count() <= MESSAGE_CAP {
        return s.to_string();
    }
    let prefix: String = s.chars().take(MESSAGE_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// Classify one already-trimmed, ANSI-stripped error line into its
/// [`ErrorClass`] + [`ErrorSeverity`], or `None` when the line is not an
/// actionable error/warning line at all. Pure and case-insensitive on the
/// substrings; the order of the checks is deliberate — the more specific
/// detectors (rustc/cargo/test/panic) run before the broad permission/network/
/// sandbox/generic ones so a `panicked at 'permission denied'` line classifies
/// as a panic, not a bare permission error.
fn classify_line(line: &str) -> Option<(ErrorClass, ErrorSeverity)> {
    let lower = line.to_ascii_lowercase();

    // A `warning:`-prefixed diagnostic is surfaced as a Warning of the matching
    // family. Detect the leading marker first so its severity overrides the
    // error-by-default below.
    let is_warning = lower.starts_with("warning:") || lower.starts_with("warning[");

    // Panic / fatal abort — most specific runtime failure shape.
    if lower.contains("panicked at")
        || lower.starts_with("thread '")
        || lower.contains("fatal error")
        || lower.contains("fatal runtime error")
        || lower.contains("segmentation fault")
        || lower.contains("core dumped")
    {
        return Some((ErrorClass::Panic, ErrorSeverity::Error));
    }

    // Cargo driver failures (a cargo line that is not a bare rustc diagnostic).
    if lower.starts_with("error: could not compile")
        || lower.contains("error: failed to compile")
        || lower.starts_with("error: failed to run")
        || lower.contains("build failed")
        || lower.contains("could not compile")
    {
        return Some((ErrorClass::Cargo, ErrorSeverity::Error));
    }

    // Rust compiler diagnostics: `error[E0277]: ...` or a bare `error: ...`,
    // and the `-->` location continuation line.
    if lower.starts_with("error[")
        || lower.starts_with("error:")
        || line.trim_start().starts_with("-->")
    {
        let sev = if is_warning {
            ErrorSeverity::Warning
        } else {
            ErrorSeverity::Error
        };
        return Some((ErrorClass::Rustc, sev));
    }
    if lower.starts_with("warning[") || lower.starts_with("warning:") {
        return Some((ErrorClass::Rustc, ErrorSeverity::Warning));
    }

    // Test-runner failures.
    if lower.contains("assertion failed")
        || lower.contains("assertionerror")
        || lower.ends_with("... failed")
        || lower.contains("test result: failed")
        || lower.contains(" failed]")
        || lower.starts_with("fail ")
        || lower.starts_with("failures:")
        || lower.contains("tests failed")
    {
        return Some((ErrorClass::TestFailure, ErrorSeverity::Error));
    }

    // Sandbox / policy denials (before the broad permission check so a
    // sandbox-flavored denial classifies as sandbox).
    if lower.contains("sandbox")
        || lower.contains("denied by policy")
        || lower.contains("not permitted by sandbox")
        || lower.contains("operation not permitted")
        || lower.contains("code signature")
        || lower.contains("not notarized")
    {
        return Some((ErrorClass::Sandbox, ErrorSeverity::Error));
    }

    // Filesystem permission / access-denied.
    if lower.contains("permission denied")
        || lower.contains("eacces")
        || lower.contains("access is denied")
        || lower.contains("access denied")
        || lower.contains("eperm")
    {
        return Some((ErrorClass::Permission, ErrorSeverity::Error));
    }

    // Network failures.
    if lower.contains("connection refused")
        || lower.contains("could not resolve host")
        || lower.contains("name or service not known")
        || lower.contains("connection timed out")
        || lower.contains("etimedout")
        || lower.contains("econnrefused")
        || lower.contains("network is unreachable")
        || lower.contains("ssl certificate problem")
    {
        return Some((ErrorClass::Network, ErrorSeverity::Error));
    }

    // A bare `warning:` of no specific family is still a surfaced diagnostic.
    if is_warning {
        return Some((ErrorClass::Generic, ErrorSeverity::Warning));
    }

    None
}

/// Extract a `file:line[:col]` location from an error line, or `None` when the
/// line names no source position. Conservative on purpose (the spec warns the
/// regex detectors are brittle): the path component must contain a `/`, `\`, or
/// a `.ext` so a bare `42:1` or a `key:value` pair is not mistaken for a
/// location. Handles the rustc `--> path:line:col` continuation, the common
/// `path:line:col:` compiler/test prefix, and a plain `path:line`. Windows
/// `C:\dir\file.rs:10:5` is handled by scanning from the right so the drive
/// colon is never taken as the separator.
fn extract_location(line: &str) -> Option<ErrorLocation> {
    let trimmed = line.trim();
    // rustc continuation: `--> src/lib.rs:10:5`.
    let body = trimmed
        .strip_prefix("-->")
        .map(str::trim)
        .unwrap_or(trimmed);
    // Take the first whitespace-delimited token that looks like a path:line —
    // a location is always a single unbroken token.
    for token in body.split_whitespace() {
        // Trim surrounding punctuation a tool may wrap the location in — a
        // trailing `:` (the common `path:line:col:` prefix shape) and trailing
        // `,`/`)`/quotes — so the right-anchored numeric split sees clean digits.
        let token =
            token.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | '"' | '\'' | ':'));
        if let Some(loc) = parse_location_token(token) {
            return Some(loc);
        }
    }
    None
}

/// Parse one `path:line[:col]` token into an [`ErrorLocation`]. Splits from the
/// right so a Windows drive letter (`C:`) stays part of the path. Requires the
/// path to look like a real path (`/`, `\`, or a dotted extension) so a generic
/// `word:123` is rejected.
fn parse_location_token(token: &str) -> Option<ErrorLocation> {
    // Split into up to three right-anchored numeric tail segments.
    let parts: Vec<&str> = token.rsplitn(3, ':').collect();
    // `parts` is right-to-left: [col_or_line, line_or_path, path?].
    match parts.as_slice() {
        // path:line:col
        [col, line, path] => {
            let col: u32 = col.parse().ok()?;
            let line: u32 = line.parse().ok()?;
            if line == 0 || !looks_like_path(path) {
                return None;
            }
            Some(ErrorLocation {
                path: (*path).to_string(),
                line,
                col: Some(col),
            })
        }
        // path:line
        [line, path] => {
            let line: u32 = line.parse().ok()?;
            if line == 0 || !looks_like_path(path) {
                return None;
            }
            Some(ErrorLocation {
                path: (*path).to_string(),
                line,
                col: None,
            })
        }
        _ => None,
    }
}

/// Heuristic: does `s` look like a file path rather than an arbitrary word? It
/// must contain a directory separator or a dotted extension, and not be empty.
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.contains('/')
        || s.contains('\\')
        || s.rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.len() <= 8)
}

/// Detect the error lenses in one failed entry's `text`. Pure and standalone so
/// it is the unit-testable heart of the feature. Scans line by line, strips
/// ANSI, classifies each line, and emits a lens (with any extracted location)
/// for each actionable line, capped at [`LENSES_PER_ENTRY_CAP`]. Lines that
/// classify as nothing are skipped. The `entry_id` is stamped onto every lens so
/// the quick-jump knows where to land.
pub(crate) fn detect_in_text(entry_id: u64, text: &str) -> Vec<ErrorLens> {
    let mut lenses = Vec::new();
    for (line_index, raw) in text.lines().enumerate() {
        let clean = strip_ansi(raw);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((class, severity)) = classify_line(trimmed) else {
            continue;
        };
        lenses.push(ErrorLens {
            entry_id,
            line_index,
            class,
            severity,
            message: cap_message(trimmed),
            location: extract_location(trimmed),
        });
        if lenses.len() >= LENSES_PER_ENTRY_CAP {
            break;
        }
    }
    lenses
}

/// The computed Error-Lens model over the transcript's failed outputs (§12.5.6).
///
/// `lenses` is the ordered list of detected lenses (transcript order by entry,
/// then line order within each entry). `fingerprint` is the staleness tag
/// described in the module docs; `built` distinguishes "empty transcript
/// scanned" from "never built" so a genuinely error-free transcript is not
/// re-scanned every refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorLenses {
    lenses: Vec<ErrorLens>,
    fingerprint: u64,
    built: bool,
}

impl ErrorLenses {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the failed candidates. Order- and
    /// content-sensitive: id and revision both participate, so an append, a
    /// revision bump, a reorder, or a drop all move the value. Pure and
    /// standalone so the caller can compute it cheaply each refresh and compare
    /// before deciding to recompute. (The text is *not* hashed — a revision
    /// bump already accompanies any text change, so hashing the id+revision is
    /// both cheaper and sufficient.)
    pub(crate) fn fingerprint_of<'a>(
        candidates: impl IntoIterator<Item = &'a ErrorCandidate>,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for c in candidates {
            c.id.hash(&mut hasher);
            c.revision.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the lens model from `candidates` **only if** `fingerprint`
    /// differs from the one captured at the last rebuild (or this is the first
    /// build). Returns `true` when a recompute actually ran, `false` when the
    /// cached model was already current (the zero-idle-cost fast path).
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        candidates: &[ErrorCandidate],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.lenses.clear();
        for candidate in candidates {
            self.lenses
                .extend(detect_in_text(candidate.id, &candidate.text));
        }
        self.fingerprint = fingerprint;
        self.built = true;
        true
    }

    /// The stored staleness fingerprint from the last rebuild. Test/diagnostic
    /// accessor; production compares inside `rebuild_if_stale`.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The detected lenses in transcript order (entry order, then line order).
    pub(crate) fn lenses(&self) -> &[ErrorLens] {
        &self.lenses
    }

    /// Number of detected lenses.
    pub(crate) fn len(&self) -> usize {
        self.lenses.len()
    }

    /// Whether any lens was detected.
    pub(crate) fn is_empty(&self) -> bool {
        self.lenses.is_empty()
    }

    /// The lens at list index `index`, or `None` when out of range.
    pub(crate) fn get(&self, index: usize) -> Option<&ErrorLens> {
        self.lenses.get(index)
    }

    /// Number of lenses in `class`.
    pub(crate) fn count_of(&self, class: ErrorClass) -> usize {
        self.lenses.iter().filter(|l| l.class == class).count()
    }

    /// The list index of the next lens strictly after `after` (wrapping to the
    /// first when `after` is the last or `None`). `None` only when there are no
    /// lenses. Drives forward quick-jump navigation.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        if self.lenses.is_empty() {
            return None;
        }
        match after {
            Some(i) if i + 1 < self.lenses.len() => Some(i + 1),
            // Last (or out of range): wrap to the first.
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The list index of the previous lens strictly before `before` (wrapping to
    /// the last). `None` only when there are no lenses. Drives backward
    /// quick-jump navigation; the overlay walks forward today, so this is
    /// exercised by the unit suite until a "previous error" verb lands.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        if self.lenses.is_empty() {
            return None;
        }
        match before {
            Some(0) | None => Some(self.lenses.len() - 1),
            Some(i) if i <= self.lenses.len() => Some(i - 1),
            // Out of range: wrap to the last.
            Some(_) => Some(self.lenses.len() - 1),
        }
    }

    /// A compact one-line summary of the detected lenses for the status line /
    /// overlay header, e.g. `"4 errors \u{00b7} 2 rustc \u{00b7} 1 test \u{00b7}
    /// 1 permission"`. Empty string when nothing was detected.
    pub(crate) fn summary(&self) -> String {
        if self.lenses.is_empty() {
            return String::new();
        }
        let total = self.lenses.len();
        let total_word = if total == 1 { "error" } else { "errors" };
        let mut parts = vec![format!("{total} {total_word}")];
        for class in ErrorClass::ALL.iter().copied() {
            let n = self.count_of(class);
            if n > 0 {
                parts.push(format!("{n} {}", class.label()));
            }
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "error_lens_tests.rs"]
mod tests;
