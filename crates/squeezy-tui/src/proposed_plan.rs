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

pub(crate) const OPEN_TAG: &str = "<proposed_plan>";
pub(crate) const CLOSE_TAG: &str = "</proposed_plan>";

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
