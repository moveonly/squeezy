//! Paste Transform Menu (§12.6.2).
//!
//! Bracketed paste already routes a pasted block into the composer (small
//! pastes type inline; a paste over [`crate::LARGE_PASTE_CHAR_THRESHOLD`]
//! collapses into a `[Pasted text #N]` attachment token; a paste over
//! [`crate::paste_preview::VERY_LARGE_PASTE_CHAR_THRESHOLD`] parks in the
//! confirm/cancel safety modal). What none of those offer is a *choice of
//! shape*: when a paste is **structured** — multiline, ANSI-laden, or a
//! recognized diff/JSON/code/log block — the user often wants it wrapped a
//! particular way before it lands (quoted, fenced as a code block, or with
//! terminal escapes stripped) rather than dumped verbatim.
//!
//! This module owns the pure model for that choice:
//!
//!   - [`PasteKind`] is the conservative structure classification (diff / JSON /
//!     code / log / multiline / single-line plain). Detection is *display-only*
//!     hinting — it never executes or trusts the content, only labels it.
//!   - [`PastePayload`] captures the normalized pending text plus its summary
//!     stats (chars, lines, bytes, whether it carries ANSI escapes) and the
//!     classified kind.
//!   - [`should_open_transform_menu`] decides whether a payload is "structured"
//!     enough to warrant the menu at all; an ordinary one-line paste flows
//!     straight through untouched.
//!   - [`PasteTransform`] is the set of shapes the menu offers, and
//!     [`apply_transform`] is the single pure function that turns the pending
//!     text into the string the composer receives.
//!   - [`PasteTransformMenu`] is the selectable overlay state: the payload, the
//!     ordered list of offered transforms, and the cursor. `lib.rs` owns the
//!     state slot, the render call, and the keyboard/mouse wiring; this module
//!     owns the math, the classification, and the text.
//!
//! It is deliberately terminal-free: every branch (classification, the
//! transforms, the menu cursor) is unit-testable without a `Frame`.

/// The conservative structure classification of a pending paste. This is a
/// *hint* surfaced in the menu header so the user knows what squeezy thinks it
/// is; it never changes what a transform produces (a `CodeBlock` of a diff and
/// of a log are wrapped identically). Detection treats the content as inert
/// text — recognizing a Windows path or a shell prompt does not make it
/// executable input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteKind {
    /// A unified/`git`-style diff (`diff `/`--- `/`+++ `/`@@ ` markers).
    Diff,
    /// A JSON object or array (trimmed text starts with `{`/`[` and ends with
    /// the matching `}`/`]`).
    Json,
    /// A block that looks like source code (braces/semicolons/indentation
    /// across multiple lines) without matching a more specific kind.
    Code,
    /// A log-like block: multiple lines that mostly carry timestamps or
    /// level tags (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`).
    Log,
    /// Multiple lines that did not match any structured kind above.
    PlainMultiline,
    /// A single line with no structure of interest.
    PlainSingle,
}

impl PasteKind {
    /// A short human label for the menu header, e.g. `"diff"`, `"JSON"`.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PasteKind::Diff => "diff",
            PasteKind::Json => "JSON",
            PasteKind::Code => "code",
            PasteKind::Log => "log",
            PasteKind::PlainMultiline => "multiline text",
            PasteKind::PlainSingle => "text",
        }
    }
}

/// One shape the menu can apply to the pending paste, in the order the menu
/// lists them. Every variant except [`PasteTransform::Cancel`] resolves to a
/// string the composer receives via [`apply_transform`]; `Cancel` discards the
/// paste and leaves the composer untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteTransform {
    /// Insert the text exactly as pasted (newline-normalized only).
    AsIs,
    /// Prefix every line with `> ` (Markdown blockquote).
    Quote,
    /// Wrap the text in a triple-backtick fenced code block. The fence widens
    /// past three backticks if the body itself contains a backtick run, so the
    /// fence can never be closed early by the content.
    CodeBlock,
    /// Strip ANSI/terminal escape sequences, leaving the plain text.
    StripAnsi,
    /// Discard the paste; nothing enters the composer.
    Cancel,
}

impl PasteTransform {
    /// The menu label for this transform.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PasteTransform::AsIs => "As-is",
            PasteTransform::Quote => "Quote",
            PasteTransform::CodeBlock => "Code block",
            PasteTransform::StripAnsi => "Strip ANSI",
            PasteTransform::Cancel => "Cancel",
        }
    }

    /// A one-line description shown beside the selected transform.
    pub(crate) fn description(self) -> &'static str {
        match self {
            PasteTransform::AsIs => "Insert the text exactly as pasted",
            PasteTransform::Quote => "Prefix every line with \"> \"",
            PasteTransform::CodeBlock => "Wrap in a fenced ``` code block",
            PasteTransform::StripAnsi => "Remove terminal escape sequences",
            PasteTransform::Cancel => "Discard the paste",
        }
    }

    /// Whether this transform inserts text (`true`) or discards it (`false`).
    /// `Cancel` is the only non-inserting transform.
    pub(crate) fn inserts(self) -> bool {
        !matches!(self, PasteTransform::Cancel)
    }
}

/// Apply `transform` to `text` (already newline-normalized) and return the
/// string the composer should receive. [`PasteTransform::Cancel`] returns an
/// empty string — the caller checks [`PasteTransform::inserts`] and never
/// inserts on cancel, so the value is unused in that case.
pub(crate) fn apply_transform(text: &str, transform: PasteTransform) -> String {
    match transform {
        PasteTransform::AsIs => text.to_string(),
        PasteTransform::Quote => quote_lines(text),
        PasteTransform::CodeBlock => fence_code_block(text),
        PasteTransform::StripAnsi => strip_ansi(text),
        PasteTransform::Cancel => String::new(),
    }
}

/// Prefix every line with `"> "`. A trailing newline is preserved (so a block
/// that ended in a newline still does); blank lines become a bare `>` with no
/// trailing space so the quote reads cleanly.
fn quote_lines(text: &str) -> String {
    let trailing_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut out = String::with_capacity(text.len() + text.len() / 8 + 2);
    let mut first = true;
    for line in body.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        if line.is_empty() {
            out.push('>');
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// Wrap `text` in a fenced code block. The opening/closing fence is the longest
/// run of backticks in the body plus one (minimum three), so content that
/// itself contains a ``` fence can never close the block early.
fn fence_code_block(text: &str) -> String {
    let max_run = longest_backtick_run(text);
    let fence_len = max_run.max(2) + 1; // at least 3 backticks
    let fence: String = "`".repeat(fence_len);
    let body = text.strip_suffix('\n').unwrap_or(text);
    format!("{fence}\n{body}\n{fence}")
}

/// The length of the longest consecutive run of backticks in `text`. Used to
/// size a fence that the body cannot prematurely close.
fn longest_backtick_run(text: &str) -> usize {
    let mut max = 0usize;
    let mut run = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            max = max.max(run);
        } else {
            run = 0;
        }
    }
    max
}

/// Remove ANSI/terminal escape sequences from `text`, leaving the printable
/// text. Handles CSI (`ESC [ … final`), the common designation escapes
/// (`ESC ( … `), and bare `ESC X` pairs; an unterminated escape at the end is
/// dropped. Mirrors the transcript renderer's `strip_ansi_escape_sequences`
/// so a pasted log shows the same plain text the transcript would.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // CSI: consume params/intermediates up to and including the
                // final byte in the @..~ range.
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            // Designation / charset escapes: ESC plus one intermediate plus
            // one final — drop both.
            Some('(' | ')' | '*' | '+' | '-' | '.' | '/') => {
                let _ = chars.next();
            }
            // Any other ESC X pair (and a trailing bare ESC): drop it.
            Some(_) | None => {}
        }
    }
    out
}

/// A pending paste captured for the transform menu, plus its precomputed
/// summary stats and structure classification. Owns the full normalized text
/// so a chosen transform applies to exactly what was pasted.
#[derive(Debug, Clone)]
pub(crate) struct PastePayload {
    /// The full newline-normalized pending text.
    text: String,
    /// Character count (what the header reports as "chars").
    char_count: usize,
    /// Line count: `\n`-separated segments, with a trailing newline NOT adding
    /// a phantom empty line (so `"a\n"` is one line).
    line_count: usize,
    /// UTF-8 byte length of the pending text. Surfaced through the `#[cfg(test)]`
    /// [`byte_count`](Self::byte_count) accessor only; the production header
    /// reports chars/lines, so the field is dead outside tests.
    #[cfg_attr(not(test), allow(dead_code))]
    byte_count: usize,
    /// Whether the text carries any ANSI/terminal escape (`ESC`) byte. Drives
    /// whether the menu surfaces (and pre-highlights) the Strip ANSI choice.
    has_ansi: bool,
    /// The conservative structure classification.
    kind: PasteKind,
}

impl PastePayload {
    /// Capture `text` (already newline-normalized) and precompute its stats and
    /// classification.
    pub(crate) fn new(text: String) -> Self {
        let char_count = text.chars().count();
        let byte_count = text.len();
        let line_count = line_count(&text);
        let has_ansi = text.contains('\x1b');
        let kind = classify(&text, line_count);
        Self {
            text,
            char_count,
            line_count,
            byte_count,
            has_ansi,
            kind,
        }
    }

    /// Borrow the pending text (read-only / tests).
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Character count.
    #[cfg(test)]
    pub(crate) fn char_count(&self) -> usize {
        self.char_count
    }

    /// Line count (`\n`-separated segments; trailing newline not counted). Read
    /// by tests and by the off-menu open gate via the summary; the render path
    /// consumes the same figure through [`summary`](Self::summary).
    #[cfg(test)]
    pub(crate) fn line_count(&self) -> usize {
        self.line_count
    }

    /// UTF-8 byte length.
    #[cfg(test)]
    pub(crate) fn byte_count(&self) -> usize {
        self.byte_count
    }

    /// Whether the text carries any ANSI escape byte.
    pub(crate) fn has_ansi(&self) -> bool {
        self.has_ansi
    }

    /// The conservative structure classification. Read by the tests; the open
    /// gate and header consume the same field directly (same-module access) and
    /// surface its label through [`summary`](Self::summary).
    #[cfg(test)]
    pub(crate) fn kind(&self) -> PasteKind {
        self.kind
    }

    /// A one-line summary for the menu header, e.g.
    /// `"diff · 42 lines · 1,200 chars"`. Singular/plural aware.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · {} · {}",
            self.kind.label(),
            count_label(self.line_count, "line", "lines"),
            count_label(self.char_count, "char", "chars"),
        )
    }

    /// Apply `transform` to the pending text. A thin pass-through to
    /// [`apply_transform`] so callers hold only the payload.
    pub(crate) fn apply(&self, transform: PasteTransform) -> String {
        apply_transform(&self.text, transform)
    }

    /// The full pending text, consumed when the menu closes (the caller applies
    /// the chosen transform itself when it needs ownership; the typical path
    /// uses [`apply`](Self::apply)).
    #[cfg(test)]
    pub(crate) fn into_text(self) -> String {
        self.text
    }
}

/// Whether a normalized paste is "structured" enough to open the transform
/// menu. Conservative: an ordinary single-line paste (a path, a URL, one log
/// line) is *not* structured and flows straight through. The menu opens when
/// the paste is multiline OR carries ANSI escapes OR was classified as a
/// recognized structured kind (diff/JSON/code/log).
///
/// The caller has already decided this paste is below the very-large safety
/// threshold (that path owns its own modal); this gate only governs the
/// in-between "structured but not huge" pastes.
pub(crate) fn should_open_transform_menu(payload: &PastePayload) -> bool {
    if payload.has_ansi {
        return true;
    }
    match payload.kind {
        PasteKind::Diff | PasteKind::Json | PasteKind::Code | PasteKind::Log => true,
        PasteKind::PlainMultiline => true,
        PasteKind::PlainSingle => false,
    }
}

/// The selectable transform-menu overlay state: the captured payload, the
/// ordered list of offered transforms, and the cursor. The offered list is
/// fixed except that Strip ANSI is omitted when the payload has no escapes (so
/// the menu never offers a no-op on clean text).
#[derive(Debug, Clone)]
pub(crate) struct PasteTransformMenu {
    payload: PastePayload,
    items: Vec<PasteTransform>,
    selected: usize,
}

impl PasteTransformMenu {
    /// Build the menu for `payload`. The cursor starts on the first item
    /// (As-is) for clean text, or on Strip ANSI when the payload carries
    /// escapes — the most likely intent for a terminal paste.
    pub(crate) fn new(payload: PastePayload) -> Self {
        let mut items = vec![
            PasteTransform::AsIs,
            PasteTransform::Quote,
            PasteTransform::CodeBlock,
        ];
        if payload.has_ansi() {
            items.push(PasteTransform::StripAnsi);
        }
        items.push(PasteTransform::Cancel);
        // Pre-select Strip ANSI for an escape-laden paste; otherwise As-is.
        let selected = if payload.has_ansi() {
            items
                .iter()
                .position(|t| *t == PasteTransform::StripAnsi)
                .unwrap_or(0)
        } else {
            0
        };
        Self {
            payload,
            items,
            selected,
        }
    }

    /// Borrow the captured payload (for the header / preview).
    pub(crate) fn payload(&self) -> &PastePayload {
        &self.payload
    }

    /// The ordered list of offered transforms.
    pub(crate) fn items(&self) -> &[PasteTransform] {
        &self.items
    }

    /// The currently selected index.
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected transform.
    pub(crate) fn selected_transform(&self) -> PasteTransform {
        self.items[self.selected]
    }

    /// Move the cursor up one item, wrapping to the bottom from the top.
    pub(crate) fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move the cursor down one item, wrapping to the top from the bottom.
    pub(crate) fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    /// Move the cursor to `index` if it is in range, returning whether it
    /// moved. Used by the mouse path to select the clicked row.
    pub(crate) fn select(&mut self, index: usize) -> bool {
        if index < self.items.len() {
            self.selected = index;
            true
        } else {
            false
        }
    }

    /// Apply the currently selected transform to the payload, returning the
    /// string the composer should receive. Returns `None` for
    /// [`PasteTransform::Cancel`] (nothing should be inserted).
    pub(crate) fn resolve(&self) -> Option<String> {
        let transform = self.selected_transform();
        if transform.inserts() {
            Some(self.payload.apply(transform))
        } else {
            None
        }
    }
}

/// Conservative structure classification of `text`. `line_count` is passed in
/// (already computed by the payload) to avoid recounting.
fn classify(text: &str, line_count: usize) -> PasteKind {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return if line_count > 1 {
            PasteKind::PlainMultiline
        } else {
            PasteKind::PlainSingle
        };
    }
    if looks_like_diff(trimmed) {
        return PasteKind::Diff;
    }
    if looks_like_json(trimmed) {
        return PasteKind::Json;
    }
    if line_count > 1 {
        if looks_like_log(trimmed) {
            return PasteKind::Log;
        }
        if looks_like_code(trimmed) {
            return PasteKind::Code;
        }
        return PasteKind::PlainMultiline;
    }
    PasteKind::PlainSingle
}

/// A unified/`git` diff: a `diff `/`Index: ` header, `--- `/`+++ ` file
/// markers, or an `@@ … @@` hunk header on some line.
fn looks_like_diff(trimmed: &str) -> bool {
    trimmed.lines().any(|line| {
        line.starts_with("diff ")
            || line.starts_with("Index: ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || (line.starts_with("@@ ") && line.contains(" @@"))
    })
}

/// A JSON object/array: trimmed text opens with `{`/`[` and closes with the
/// matching `}`/`]`. A shallow shape check only — it does not validate the
/// body, just recognizes the envelope so the header can label it.
fn looks_like_json(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    matches!(
        (bytes.first(), bytes.last()),
        (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']'))
    )
}

/// A log-like block: more than half of the non-blank lines carry a level tag
/// (`ERROR`/`WARN`/`WARNING`/`INFO`/`DEBUG`/`TRACE`, case-insensitive) or a
/// leading bracketed/ISO-ish timestamp.
fn looks_like_log(trimmed: &str) -> bool {
    const LEVELS: [&str; 6] = ["ERROR", "WARN", "WARNING", "INFO", "DEBUG", "TRACE"];
    let mut total = 0usize;
    let mut hits = 0usize;
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        let upper = line.to_ascii_uppercase();
        let has_level = LEVELS.iter().any(|lvl| upper.contains(lvl));
        let has_timestamp = looks_like_timestamp_prefix(line);
        if has_level || has_timestamp {
            hits += 1;
        }
    }
    total > 0 && hits * 2 > total
}

/// Whether `line` opens with a bracketed `[…]` group or a digit-led `ISO`-ish
/// timestamp (`YYYY-…` / `HH:MM:`). Heuristic, display-only.
fn looks_like_timestamp_prefix(line: &str) -> bool {
    let l = line.trim_start();
    if l.starts_with('[') {
        return true;
    }
    let bytes = l.as_bytes();
    if bytes.len() >= 5
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
    {
        return true; // YYYY-
    }
    if bytes.len() >= 3
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2] == b':'
    {
        return true; // HH:
    }
    false
}

/// A code-ish block: multiline text where several lines carry code punctuation
/// (`{`/`}`/`;`) or consistent leading indentation. Heuristic and last-resort —
/// only reached after diff/JSON/log checks fail.
fn looks_like_code(trimmed: &str) -> bool {
    let mut total = 0usize;
    let mut punct = 0usize;
    let mut indented = 0usize;
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        let t = line.trim_end();
        if t.ends_with('{') || t.ends_with('}') || t.ends_with(';') || t.ends_with(':') {
            punct += 1;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            indented += 1;
        }
    }
    total > 1 && (punct * 3 >= total || indented * 2 >= total)
}

/// Number of `\n`-separated segments in `text`. An empty string is zero lines;
/// a trailing newline does not add a phantom empty line (so `"a\n"` is one
/// line). Mirrors `paste_preview`'s line-count contract.
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count();
    if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Format `count` with a thousands separator and the singular/plural unit.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let unit = if count == 1 { singular } else { plural };
    format!("{} {}", group_thousands(count), unit)
}

/// Insert `,` every three digits from the right. Pure ASCII-digit work; no
/// locale dependency.
fn group_thousands(value: usize) -> String {
    let digits = value.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
#[path = "paste_transform_tests.rs"]
mod tests;
