//! Large Paste Staging (§12.6.3).
//!
//! Bracketed paste already routes a pasted block into the composer along a
//! graduated set of paths (small pastes type inline; a paste over
//! [`crate::LARGE_PASTE_CHAR_THRESHOLD`] collapses into a `[Pasted text #N]`
//! attachment; a *structured* paste below that threshold opens the
//! transform menu (§12.6.2); a paste over
//! [`crate::paste_preview::VERY_LARGE_PASTE_CHAR_THRESHOLD`] parks in the
//! confirm/cancel safety preview (§11G.6)). What none of those offer is a
//! *staging area* for a genuinely **huge** paste — many screens of text — where
//! the user can see byte/line/token estimates, the classified type, a bounded
//! preview, and any warnings, then choose from a richer action set (insert,
//! quote, code block, temp file, attach, queue, copy preview, cancel) before a
//! single byte enters the composer or context.
//!
//! This module owns the pure model for that staging view; it *complements* the
//! §11G.6 preview by sitting above it (a still-larger threshold) and offering
//! the full action menu rather than a binary confirm/cancel:
//!
//!   - [`is_huge_paste`] decides whether a normalized paste is large enough to
//!     stage at all (anything smaller flows through the existing preview /
//!     transform / inline paths untouched).
//!   - [`PasteEstimates`] captures the byte/char/line counts and a coarse token
//!     estimate the staging header shows.
//!   - [`StagingWarning`] enumerates the inert-display hazards the staging view
//!     surfaces (terminal control bytes, NUL bytes, very long lines) so a huge
//!     dump never silently injects escape sequences when committed.
//!   - [`StagingAction`] is the ordered set of actions the staging menu offers,
//!     and [`StagedPaste`] holds the captured text plus its precomputed
//!     estimates / warnings / classification and offers a bounded, sanitized
//!     preview body.
//!   - [`PasteStaging`] is the selectable overlay state: the staged paste, the
//!     action list, and the cursor. `lib.rs` owns the state slot, the render
//!     call, the temp-file/queue/clipboard wiring, and the keyboard/mouse
//!     handlers; this module owns the math, the classification, the warnings,
//!     and the sanitized text.
//!
//! It is deliberately terminal-free and IO-free: every branch (the estimates,
//! the warnings, the sanitized preview, the action cursor, the pure
//! text-producing actions) is unit-testable without a `Frame` or the
//! filesystem. The IO-bound actions (temp file, queue, clipboard) are resolved
//! by `lib.rs`, which holds the workspace and the sinks; this module only tells
//! it *which* action the user chose and hands back the text to act on.

use crate::paste_transform::{self, PasteKind, PasteTransform};

/// Pastes at or below this many characters never stage — they keep their
/// existing behavior (inline / attachment / transform menu / §11G.6 preview).
/// Set well above [`crate::paste_preview::VERY_LARGE_PASTE_CHAR_THRESHOLD`]
/// (10_000) so the two thresholds do not fight: 10k..=50k still routes to the
/// confirm/cancel preview, only a genuinely *huge* paste (>50k chars, many
/// screens of text) earns the full staging area with its richer action set.
pub(crate) const HUGE_PASTE_CHAR_THRESHOLD: usize = 50_000;

/// Largest number of preview body lines the staging view renders. A huge paste
/// can be tens of thousands of lines; showing a bounded head window keeps the
/// staging view a fixed size and the render cost constant regardless of paste
/// size. Lines past this are summarized by the "+N more lines" marker.
pub(crate) const STAGING_PREVIEW_MAX_LINES: usize = 14;

/// A line longer than this many characters is flagged by
/// [`StagingWarning::LongLines`]: a single multi-kilobyte line can stall soft
/// wrapping/search, so the staging view warns before the user commits it.
const LONG_LINE_CHAR_THRESHOLD: usize = 2_000;

/// Average characters per token for the coarse token estimate. Tokenizers vary,
/// but ~4 chars/token is the well-worn rule of thumb for English text and is
/// honest as a ballpark — the header labels it "~N tokens" so it never reads as
/// exact.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

/// Whether a normalized paste is large enough to warrant the staging area. The
/// caller passes text that has already been newline-normalized (CRLF/CR → LF).
/// Counts characters, not bytes, so a multi-byte-heavy block is judged by what
/// the user perceives as length rather than UTF-8 weight.
pub(crate) fn is_huge_paste(text: &str) -> bool {
    text.chars().count() > HUGE_PASTE_CHAR_THRESHOLD
}

/// Coarse size estimates for the staged paste, shown in the staging header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PasteEstimates {
    /// Character count (what the header reports as "chars").
    pub(crate) chars: usize,
    /// UTF-8 byte length.
    pub(crate) bytes: usize,
    /// Line count: `\n`-separated segments, with a trailing newline NOT adding a
    /// phantom empty line (so `"a\n"` is one line).
    pub(crate) lines: usize,
    /// Coarse token estimate (chars / [`CHARS_PER_TOKEN_ESTIMATE`]). Labelled
    /// `~N` so it never reads as exact.
    pub(crate) tokens: usize,
}

impl PasteEstimates {
    /// Compute the estimates for `text`. `line_count` is passed in (already
    /// computed by the caller) to avoid recounting.
    fn new(text: &str, line_count: usize) -> Self {
        let chars = text.chars().count();
        Self {
            chars,
            bytes: text.len(),
            lines: line_count,
            tokens: chars / CHARS_PER_TOKEN_ESTIMATE,
        }
    }
}

/// An inert-display hazard the staging view surfaces before a huge paste is
/// committed. Display-only: the warning never rewrites the captured text (the
/// chosen action does), it only tells the user what is in the block so a
/// terminal-control dump or a wrapping-killer long line is never committed by
/// surprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagingWarning {
    /// The paste carries ANSI / terminal escape (`ESC`) bytes. Committing raw
    /// would inject control sequences; the staging preview shows them inert and
    /// the Strip-ANSI-flavored actions remove them.
    TerminalControls,
    /// The paste carries NUL (`\0`) bytes — often a sign of binary content
    /// pasted by mistake.
    NulBytes,
    /// The paste has at least one very long line (> [`LONG_LINE_CHAR_THRESHOLD`]
    /// chars) that can stall soft wrapping or search if committed verbatim.
    LongLines,
}

impl StagingWarning {
    /// A short human label for the staging warnings line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StagingWarning::TerminalControls => "terminal control bytes",
            StagingWarning::NulBytes => "NUL bytes",
            StagingWarning::LongLines => "very long lines",
        }
    }
}

/// One action the staging menu can take on the huge paste, in the order the
/// menu lists them. Every variant resolves to a concrete disposition — most
/// produce a string the composer/queue receives; [`StagingAction::TempFile`]
/// asks `lib.rs` to write the text to a platform temp file and insert a path
/// token; [`StagingAction::CopyPreview`] copies the summary to the clipboard
/// and leaves the composer untouched; [`StagingAction::Cancel`] discards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagingAction {
    /// Insert the text into the composer as a `[Pasted text #N]` attachment
    /// (the default disposition for a huge paste — it never types inline).
    Insert,
    /// Quote the text (prefix every line with `> `) then insert it.
    Quote,
    /// Wrap the text in a fenced code block then insert it.
    CodeBlock,
    /// Insert with terminal escape sequences stripped (the sanitized text).
    StripAnsi,
    /// Write the text to a platform temp file and insert a reference to it
    /// instead of the body. Keeps a multi-megabyte block out of the composer.
    TempFile,
    /// Append the text to the prompt queue as its own pending prompt rather than
    /// the current composer line.
    Queue,
    /// Copy the staging summary + preview to the clipboard without inserting.
    CopyPreview,
    /// Discard the staged paste; nothing enters the composer, context, or queue.
    Cancel,
}

impl StagingAction {
    /// The menu label for this action.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StagingAction::Insert => "Insert",
            StagingAction::Quote => "Quote",
            StagingAction::CodeBlock => "Code block",
            StagingAction::StripAnsi => "Strip ANSI",
            StagingAction::TempFile => "Temp file",
            StagingAction::Queue => "Queue",
            StagingAction::CopyPreview => "Copy preview",
            StagingAction::Cancel => "Cancel",
        }
    }

    /// A one-line description shown beside the selected action.
    pub(crate) fn description(self) -> &'static str {
        match self {
            StagingAction::Insert => "Attach the paste to the prompt",
            StagingAction::Quote => "Prefix every line with \"> \", then attach",
            StagingAction::CodeBlock => "Wrap in a fenced ``` block, then attach",
            StagingAction::StripAnsi => "Remove terminal escapes, then attach",
            StagingAction::TempFile => "Write to a temp file, insert a reference",
            StagingAction::Queue => "Append as its own queued prompt",
            StagingAction::CopyPreview => "Copy the summary + preview to the clipboard",
            StagingAction::Cancel => "Discard the paste",
        }
    }

    /// Whether this action ultimately attaches/queues the staged text into the
    /// prompt (`true`) versus side actions that touch the filesystem/clipboard
    /// or discard (`false`). Used only by tests today to assert the grouping.
    #[cfg(test)]
    pub(crate) fn enters_prompt(self) -> bool {
        matches!(
            self,
            StagingAction::Insert
                | StagingAction::Quote
                | StagingAction::CodeBlock
                | StagingAction::StripAnsi
                | StagingAction::Queue
        )
    }
}

/// A huge paste captured for the staging view, plus its precomputed estimates,
/// warnings, and structure classification. Owns the full normalized text so a
/// chosen action applies to exactly what was pasted.
#[derive(Debug, Clone)]
pub(crate) struct StagedPaste {
    /// The full newline-normalized pending text.
    text: String,
    /// Size estimates (chars / bytes / lines / ~tokens).
    estimates: PasteEstimates,
    /// The conservative structure classification (reused from §12.6.2).
    kind: PasteKind,
    /// Whether the text carries any ANSI escape byte (drives the Strip-ANSI
    /// action and the [`StagingWarning::TerminalControls`] warning).
    has_ansi: bool,
    /// The inert-display warnings, in a stable order.
    warnings: Vec<StagingWarning>,
}

impl StagedPaste {
    /// Capture `text` (already newline-normalized) and precompute its estimates,
    /// classification, and warnings.
    pub(crate) fn new(text: String) -> Self {
        let line_count = line_count(&text);
        let estimates = PasteEstimates::new(&text, line_count);
        let kind = paste_transform::classify_text(&text, line_count);
        let has_ansi = text.contains('\x1b');
        let warnings = detect_warnings(&text, has_ansi);
        Self {
            text,
            estimates,
            kind,
            has_ansi,
            warnings,
        }
    }

    /// The size estimates. Read by tests; the production header consumes the
    /// same figures through [`summary`](Self::summary) (same-module field
    /// access), so the accessor is dead outside tests.
    #[cfg(test)]
    pub(crate) fn estimates(&self) -> PasteEstimates {
        self.estimates
    }

    /// Whether the text carries any ANSI escape byte. Drives the Strip-ANSI
    /// action and the [`StagingWarning::TerminalControls`] warning.
    pub(crate) fn has_ansi(&self) -> bool {
        self.has_ansi
    }

    /// The inert-display warnings (possibly empty). Read by tests; the render
    /// path consumes the same data through [`warnings_summary`](Self::warnings_summary).
    #[cfg(test)]
    pub(crate) fn warnings(&self) -> &[StagingWarning] {
        &self.warnings
    }

    /// A one-line summary for the staging header, e.g.
    /// `"code · 1,204 lines · 61,500 chars · 60,100 bytes · ~15,375 tokens"`.
    /// Singular/plural aware.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · {} · {} · {} · ~{} {}",
            self.kind.label(),
            count_label(self.estimates.lines, "line", "lines"),
            count_label(self.estimates.chars, "char", "chars"),
            count_label(self.estimates.bytes, "byte", "bytes"),
            group_thousands(self.estimates.tokens),
            if self.estimates.tokens == 1 {
                "token"
            } else {
                "tokens"
            },
        )
    }

    /// A one-line warnings summary for the staging view, e.g.
    /// `"warnings: terminal control bytes, very long lines"`, or `None` when the
    /// paste is clean. Display-only; the chosen action decides what to do.
    pub(crate) fn warnings_summary(&self) -> Option<String> {
        if self.warnings.is_empty() {
            return None;
        }
        let joined = self
            .warnings
            .iter()
            .map(|warning| warning.label())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("warnings: {joined}"))
    }

    /// The bounded, *sanitized* preview body the staging view paints: at most
    /// [`STAGING_PREVIEW_MAX_LINES`] lines, each with terminal escape sequences
    /// stripped and each clipped to `width` columns (by character, with a
    /// trailing `…` when clipped), followed by a `"+N more lines"` marker when
    /// the paste has more lines than the window shows.
    ///
    /// Critically, the preview is ALWAYS rendered with escapes stripped — even
    /// for the As-is/Insert action — so a huge dump of terminal controls can
    /// never inject control sequences into the display while it is staged. The
    /// captured text itself is untouched; only the preview is sanitized.
    ///
    /// Returned as owned `String`s so the caller can style them into `Line`s
    /// without borrowing `self` across the render closure.
    pub(crate) fn preview_lines(&self, width: usize) -> Vec<String> {
        // Strip a single trailing newline so `"a\n"` previews as one line, not a
        // phantom empty line — matching `line_count`'s contract and the
        // "+N more lines" math below.
        let body = self.text.strip_suffix('\n').unwrap_or(&self.text);
        let mut out = Vec::new();
        let mut shown = 0usize;
        for raw in body.split('\n').take(STAGING_PREVIEW_MAX_LINES) {
            // Sanitize first (strip escapes + NUL), THEN clip — so a clipped line
            // can never end mid-escape and leak a control byte.
            let sanitized = sanitize_preview_line(raw);
            out.push(clip_line(&sanitized, width));
            shown += 1;
        }
        let remaining = self.estimates.lines.saturating_sub(shown);
        if remaining > 0 {
            out.push(format!(
                "… +{} more {}",
                group_thousands(remaining),
                if remaining == 1 { "line" } else { "lines" }
            ));
        }
        out
    }

    /// The full pending text, consumed when an action takes ownership (e.g. the
    /// temp-file write or the queue append in `lib.rs`).
    pub(crate) fn into_text(self) -> String {
        self.text
    }

    /// Produce the prompt text for an action that enters the composer/queue
    /// ([`StagingAction::Insert`] / [`Quote`](StagingAction::Quote) /
    /// [`CodeBlock`](StagingAction::CodeBlock) /
    /// [`StripAnsi`](StagingAction::StripAnsi) /
    /// [`Queue`](StagingAction::Queue)). Returns `None` for the side actions
    /// (temp file / copy preview / cancel) that `lib.rs` handles specially.
    pub(crate) fn prompt_text_for(&self, action: StagingAction) -> Option<String> {
        match action {
            StagingAction::Insert | StagingAction::Queue => Some(self.text.clone()),
            StagingAction::Quote => Some(paste_transform::apply_transform(
                &self.text,
                PasteTransform::Quote,
            )),
            StagingAction::CodeBlock => Some(paste_transform::apply_transform(
                &self.text,
                PasteTransform::CodeBlock,
            )),
            StagingAction::StripAnsi => Some(paste_transform::apply_transform(
                &self.text,
                PasteTransform::StripAnsi,
            )),
            StagingAction::TempFile | StagingAction::CopyPreview | StagingAction::Cancel => None,
        }
    }

    /// The text to put on the clipboard for [`StagingAction::CopyPreview`]: the
    /// one-line summary, the warnings line (when present), then the sanitized
    /// preview body at a fixed width. Pure so the copy is identical regardless
    /// of the rendered modal width.
    pub(crate) fn copy_preview_text(&self) -> String {
        let mut out = self.summary();
        if let Some(warnings) = self.warnings_summary() {
            out.push('\n');
            out.push_str(&warnings);
        }
        out.push('\n');
        // A fixed, generous width so the copied preview is stable and readable.
        for line in self.preview_lines(120) {
            out.push('\n');
            out.push_str(&line);
        }
        out
    }
}

/// The selectable staging-menu overlay state: the captured paste, the ordered
/// list of offered actions, and the cursor. The offered list is fixed except
/// that Strip ANSI is omitted when the paste has no escapes (so the menu never
/// offers a no-op on clean text).
#[derive(Debug, Clone)]
pub(crate) struct PasteStaging {
    paste: StagedPaste,
    actions: Vec<StagingAction>,
    selected: usize,
}

impl PasteStaging {
    /// Build the staging overlay for `paste`. The cursor starts on the first
    /// action (Insert) for clean text, or on Strip ANSI when the paste carries
    /// escapes — the most likely intent for a terminal dump.
    pub(crate) fn new(paste: StagedPaste) -> Self {
        let mut actions = vec![
            StagingAction::Insert,
            StagingAction::Quote,
            StagingAction::CodeBlock,
        ];
        if paste.has_ansi() {
            actions.push(StagingAction::StripAnsi);
        }
        actions.push(StagingAction::TempFile);
        actions.push(StagingAction::Queue);
        actions.push(StagingAction::CopyPreview);
        actions.push(StagingAction::Cancel);
        // Pre-select Strip ANSI for an escape-laden paste; otherwise Insert.
        let selected = if paste.has_ansi() {
            actions
                .iter()
                .position(|action| *action == StagingAction::StripAnsi)
                .unwrap_or(0)
        } else {
            0
        };
        Self {
            paste,
            actions,
            selected,
        }
    }

    /// Borrow the staged paste (for the header / preview / warnings).
    pub(crate) fn paste(&self) -> &StagedPaste {
        &self.paste
    }

    /// The ordered list of offered actions.
    pub(crate) fn actions(&self) -> &[StagingAction] {
        &self.actions
    }

    /// The currently selected index.
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected action.
    pub(crate) fn selected_action(&self) -> StagingAction {
        self.actions[self.selected]
    }

    /// Move the cursor up one item, wrapping to the bottom from the top.
    pub(crate) fn move_up(&mut self) {
        if self.actions.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.actions.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move the cursor down one item, wrapping to the top from the bottom.
    pub(crate) fn move_down(&mut self) {
        if self.actions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.actions.len();
    }

    /// Move the cursor to `index` if it is in range, returning whether it moved.
    /// Used by the mouse path to select the clicked row.
    pub(crate) fn select(&mut self, index: usize) -> bool {
        if index < self.actions.len() {
            self.selected = index;
            true
        } else {
            false
        }
    }

    /// Consume the staging overlay, returning the staged paste so `lib.rs` can
    /// resolve the selected action with ownership of the text.
    pub(crate) fn into_paste(self) -> StagedPaste {
        self.paste
    }
}

/// Detect the inert-display warnings for `text`. `has_ansi` is passed in
/// (already computed) to avoid a second scan.
fn detect_warnings(text: &str, has_ansi: bool) -> Vec<StagingWarning> {
    let mut warnings = Vec::new();
    if has_ansi {
        warnings.push(StagingWarning::TerminalControls);
    }
    if text.contains('\0') {
        warnings.push(StagingWarning::NulBytes);
    }
    if text
        .split('\n')
        .any(|line| line.chars().count() > LONG_LINE_CHAR_THRESHOLD)
    {
        warnings.push(StagingWarning::LongLines);
    }
    warnings
}

/// Sanitize a single preview line: strip ANSI/terminal escape sequences and
/// drop NUL bytes so the staged preview can never inject control sequences into
/// the display. Reuses the §12.6.2 ANSI stripper so a pasted log previews as the
/// same plain text the transcript would show.
fn sanitize_preview_line(raw: &str) -> String {
    let stripped = paste_transform::apply_transform(raw, PasteTransform::StripAnsi);
    stripped.replace('\0', "")
}

/// Clip `raw` to `width` characters, appending a `…` when truncated. Operates on
/// `char`s (not bytes) so multi-byte glyphs are never split. A `width` of 0
/// collapses to just the ellipsis marker for a non-empty line, or empty for an
/// empty line.
fn clip_line(raw: &str, width: usize) -> String {
    let char_count = raw.chars().count();
    if char_count <= width {
        return raw.to_string();
    }
    if width == 0 {
        return if raw.is_empty() {
            String::new()
        } else {
            "…".to_string()
        };
    }
    let keep = width.saturating_sub(1);
    let mut clipped: String = raw.chars().take(keep).collect();
    clipped.push('…');
    clipped
}

/// Number of `\n`-separated segments in `text`. An empty string is zero lines;
/// a trailing newline does not add a phantom empty line (so `"a\n"` is one
/// line). Mirrors `paste_preview` / `paste_transform`'s line-count contract.
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
#[path = "paste_staging_tests.rs"]
mod tests;
