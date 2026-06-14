//! Large-paste preview / confirmation (§11.5 / backlog 11G.6).
//!
//! Bracketed paste already routes a pasted block into the composer (small
//! pastes type inline; a paste over [`crate::LARGE_PASTE_CHAR_THRESHOLD`]
//! becomes a collapsed `[Pasted text #N]` attachment token). What was missing
//! is a *visible confirmation* for a **very large** paste before it touches the
//! composer at all — the "paste safety" affordance the spec calls out: a user
//! who fat-fingers a multi-megabyte clipboard into the prompt should see what
//! is about to land and get a chance to cancel.
//!
//! This module owns the pure model for that confirmation:
//!
//!   - [`is_very_large_paste`] decides whether a normalized paste is big enough
//!     to warrant the question at all (anything smaller flows straight through the
//!     existing inline / attachment paths untouched).
//!   - [`PastePreview`] captures the pending text plus its summary stats (chars,
//!     lines, bytes) and produces the bounded preview body the inline question
//!     paints — a head window of the first lines, each clipped to the question
//!     width, with
//!     a "+N more lines" marker so a huge block never tries to render in full.
//!   - [`PasteDecision`] is the confirm/cancel verdict the key/mouse handlers in
//!     `lib.rs` resolve the question with.
//!
//! It is deliberately terminal-free: it depends only on the pending [`String`]
//! and the geometry numbers the caller passes in, so every branch (clipping,
//! the more-lines marker, the singular/plural stat line) is unit-testable
//! without a `Frame`. `lib.rs` owns the state slot, the render call, and the
//! keyboard/mouse wiring; this module owns the math and the text.

/// Pastes at or below this many characters never trigger the confirmation
/// question — they keep their existing behavior (inline insert for small pastes,
/// `[Pasted text #N]` attachment for the merely-large ones). The question is for
/// the *accidental dump* case: a clipboard so large that silently swallowing it
/// into the composer would surprise the user. Set well above
/// [`crate::LARGE_PASTE_CHAR_THRESHOLD`] (1_000) so the two thresholds do not
/// fight: 1k..=10k still auto-attaches, only >10k prompts.
pub(crate) const VERY_LARGE_PASTE_CHAR_THRESHOLD: usize = 10_000;

/// Largest number of preview body lines the inline question renders. A very
/// large paste can be thousands of lines; showing a bounded head window keeps the
/// question a
/// fixed size and the render cost constant regardless of paste size. Lines past
/// this are summarized by the "+N more lines" marker.
pub(crate) const PREVIEW_MAX_LINES: usize = 12;

/// Whether a normalized paste is large enough to warrant the confirmation
/// question. The caller passes text that has already been newline-normalized
/// (CRLF/CR → LF). Counts characters, not bytes, so a multi-byte-heavy block
/// is judged by what the user perceives as length rather than UTF-8 weight.
pub(crate) fn is_very_large_paste(text: &str) -> bool {
    text.chars().count() > VERY_LARGE_PASTE_CHAR_THRESHOLD
}

/// The user's verdict on a pending large paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteDecision {
    /// Insert the pending text into the composer (via the normal large-paste
    /// path: inline if short, attachment token if long).
    Confirm,
    /// Discard the pending text; nothing enters the composer.
    Cancel,
}

/// A pending large paste awaiting the user's confirm/cancel decision.
///
/// Owns the full pending text (so confirming inserts exactly what was pasted)
/// plus the precomputed summary stats the question header shows. The body lines
/// are derived on demand from [`preview_lines`](Self::preview_lines) so the
/// struct stays cheap to hold and the clipping reflows on resize.
#[derive(Debug, Clone)]
pub(crate) struct PastePreview {
    /// The full normalized pending text. Handed back verbatim on confirm.
    text: String,
    /// Character count (what the stat line reports as "chars").
    char_count: usize,
    /// Line count: number of `\n`-separated segments. A block with no trailing
    /// newline still counts its last segment, so "abc" is one line and "a\nb"
    /// is two.
    line_count: usize,
    /// Byte length of the pending text (UTF-8). Surfaced alongside chars so a
    /// user can gauge the real payload size of a multi-byte block.
    byte_count: usize,
}

impl PastePreview {
    /// Capture `text` (already newline-normalized) as a pending paste and
    /// precompute its summary stats.
    pub(crate) fn new(text: String) -> Self {
        let char_count = text.chars().count();
        let byte_count = text.len();
        let line_count = line_count(&text);
        Self {
            text,
            char_count,
            line_count,
            byte_count,
        }
    }

    /// The full pending text, consumed on confirm.
    pub(crate) fn into_text(self) -> String {
        self.text
    }

    /// Borrow the pending text (for tests / read-only inspection).
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Precomputed character count. Test-only readout today; the production
    /// header consumes the same figure through [`summary`](Self::summary).
    #[cfg(test)]
    pub(crate) fn char_count(&self) -> usize {
        self.char_count
    }

    /// Number of `\n`-separated lines in the pending paste. Read by the render
    /// path's preview-window math via [`preview_lines`](Self::preview_lines) and
    /// surfaced (with chars/bytes) in [`summary`](Self::summary); exposed
    /// directly for tests.
    #[cfg(test)]
    pub(crate) fn line_count(&self) -> usize {
        self.line_count
    }

    /// Precomputed UTF-8 byte length. Test-only readout today; the production
    /// header consumes the same figure through [`summary`](Self::summary).
    #[cfg(test)]
    pub(crate) fn byte_count(&self) -> usize {
        self.byte_count
    }

    /// A one-line summary of the pending paste for the question header, e.g.
    /// `"89 lines · 3,420 chars · 3,500 bytes"`. Singular/plural aware so a
    /// one-line block reads "1 line".
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · {} · {}",
            count_label(self.line_count, "line", "lines"),
            count_label(self.char_count, "char", "chars"),
            count_label(self.byte_count, "byte", "bytes"),
        )
    }

    /// The bounded preview body the inline question paints: at most [`PREVIEW_MAX_LINES`]
    /// lines, each clipped to `width` columns (by character, with a trailing `…`
    /// when clipped), followed by a `"+N more lines"` marker when the paste has
    /// more lines than the window shows. `width` is the question's inner content
    /// width; a `0` width yields fully-clipped lines but never panics.
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
        for raw in body.split('\n').take(PREVIEW_MAX_LINES) {
            out.push(clip_line(raw, width));
            shown += 1;
        }
        let remaining = self.line_count.saturating_sub(shown);
        if remaining > 0 {
            out.push(format!(
                "… +{} more {}",
                remaining,
                if remaining == 1 { "line" } else { "lines" }
            ));
        }
        out
    }
}

/// Number of `\n`-separated segments in `text`. An empty string is zero lines;
/// any non-empty block has at least one. A trailing newline does not add a
/// phantom empty line (so "a\n" is one line, matching how the composer would
/// show it).
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

/// Clip `raw` to `width` characters, appending a `…` when truncated. Operates on
/// `char`s (not bytes) so multi-byte glyphs are never split. A `width` of 0
/// collapses to just the ellipsis marker for a non-empty line, or empty for an
/// empty line. Tabs/control chars are passed through unchanged — the inline
/// question paragraph widget renders them; this function only bounds length.
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
    // Reserve one column for the ellipsis so the clipped line still fits.
    let keep = width.saturating_sub(1);
    let mut clipped: String = raw.chars().take(keep).collect();
    clipped.push('…');
    clipped
}

/// Format `count` with a thousands separator and the singular/plural unit, e.g.
/// `(1, "line", "lines") -> "1 line"`, `(3_420, "char", "chars") -> "3,420
/// chars"`. Keeps the stat line readable for very large counts.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let unit = if count == 1 { singular } else { plural };
    format!("{} {}", group_thousands(count), unit)
}

/// Insert `,` every three digits from the right. Pure ASCII-digit work; no
/// locale dependency, so the readout is stable across environments.
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
#[path = "paste_preview_tests.rs"]
mod tests;
