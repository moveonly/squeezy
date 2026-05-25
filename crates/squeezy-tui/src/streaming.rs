//! Streaming controller for assistant text deltas.
//!
//! Splits the in-flight reply into:
//!   * `committed` — bytes that have crossed a `\n` boundary, so they
//!     are safe to send through the markdown/highlighter pipeline.
//!   * `tail`      — bytes since the last `\n`. Painted as plain text
//!     so a half-streamed fence (`` ```ru ``…) doesn't flash with the
//!     wrong style until the closing newline arrives.
//!
//! The controller exposes the read API needed by the existing render
//! path (`text()`, `is_empty()`, `trim_is_empty()`) so it can drop in
//! where the previous `String` lived.

use std::fmt;

#[derive(Debug, Default, Clone)]
pub(crate) struct StreamingController {
    committed: String,
    /// Lines that are inside an unclosed fence — buffered so a fenced
    /// code block doesn't render with the wrong style until its closing
    /// fence arrives.
    held: String,
    /// Bytes since the last `\n` (incomplete current line).
    pending: String,
    in_fence: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamingMutation {
    /// Tail grew; tail-only repaint suffices.
    TailGrew,
    /// One or more committed lines flushed; committed region needs a relayout.
    CommittedGrew,
    /// Nothing changed (e.g. empty delta).
    NoOp,
}

impl StreamingController {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.committed.is_empty() && self.held.is_empty() && self.pending.is_empty()
    }

    pub(crate) fn trim_is_empty(&self) -> bool {
        self.committed.trim().is_empty()
            && self.held.trim().is_empty()
            && self.pending.trim().is_empty()
    }

    /// Render-equivalent full text (committed + held + pending).
    pub(crate) fn text(&self) -> String {
        let mut out =
            String::with_capacity(self.committed.len() + self.held.len() + self.pending.len());
        out.push_str(&self.committed);
        out.push_str(&self.held);
        out.push_str(&self.pending);
        out
    }

    #[allow(dead_code)]
    pub(crate) fn committed(&self) -> &str {
        &self.committed
    }

    /// Returns the live tail region (lines held inside an open fence
    /// plus the in-progress current line). What the renderer paints
    /// "below" the committed/cached region.
    #[allow(dead_code)]
    pub(crate) fn tail(&self) -> String {
        if self.held.is_empty() {
            self.pending.clone()
        } else {
            let mut out = String::with_capacity(self.held.len() + self.pending.len());
            out.push_str(&self.held);
            out.push_str(&self.pending);
            out
        }
    }

    pub(crate) fn clear(&mut self) {
        self.committed.clear();
        self.held.clear();
        self.pending.clear();
        self.in_fence = false;
    }

    /// Append a delta, promoting newline-terminated runs into `committed`
    /// when they sit outside an open fence. Lines inside an unclosed
    /// fence are held until the closing fence arrives.
    pub(crate) fn push_delta(&mut self, delta: &str) -> StreamingMutation {
        if delta.is_empty() {
            return StreamingMutation::NoOp;
        }
        let mut mutation = StreamingMutation::TailGrew;
        for ch in delta.chars() {
            self.pending.push(ch);
            if ch != '\n' {
                continue;
            }
            let line = std::mem::take(&mut self.pending);
            let was_in_fence = self.in_fence;
            let toggled = Self::line_is_fence(&line);
            if toggled {
                self.in_fence = !self.in_fence;
            }
            if was_in_fence {
                // We were inside a fence at the start of this line.
                self.held.push_str(&line);
                if !self.in_fence {
                    // Closing fence: flush held block now.
                    let flushed = std::mem::take(&mut self.held);
                    self.committed.push_str(&flushed);
                    mutation = StreamingMutation::CommittedGrew;
                }
            } else if self.in_fence {
                // Opening fence — buffer the opening line.
                self.held.push_str(&line);
            } else {
                self.committed.push_str(&line);
                mutation = StreamingMutation::CommittedGrew;
            }
        }
        mutation
    }

    /// Drain everything into `committed` and return the final text.
    /// Used on `AssistantCompleted` to flush whatever's in flight.
    #[allow(dead_code)]
    pub(crate) fn finalize(&mut self) -> String {
        if !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            self.committed.push_str(&held);
        }
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.committed.push_str(&pending);
        }
        self.in_fence = false;
        std::mem::take(&mut self.committed)
    }

    fn line_is_fence(line: &str) -> bool {
        // A line counts as a fence toggle if its trimmed body starts with
        // three or more backticks (Markdown CommonMark §4.5).
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") && !trimmed.starts_with("~~~") {
            return false;
        }
        // The remainder must not contain a bare quote/heading mark mid-line
        // (we keep this loose; only the opening prefix matters).
        true
    }
}

impl fmt::Display for StreamingController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.committed)?;
        f.write_str(&self.held)?;
        f.write_str(&self.pending)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod tests;
