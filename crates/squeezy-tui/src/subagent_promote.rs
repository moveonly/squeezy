//! Promote Subagent Result To Prompt (§12.8.4): turn a useful subagent result
//! into reviewed follow-up work. A subagent that finished (a completion summary),
//! failed (a failure diagnostic), or is still running (its latest activity) can be
//! *promoted* — its result distilled into a clean, plain-text prompt and dropped
//! into the composer for editing (when the session is idle) or queued behind the
//! running turn (when one is in flight). The promoted text is **never
//! auto-submitted**: the spec is explicit that this is *reviewed* follow-up, so
//! the result only ever lands as editable composer text or a queued item the user
//! still has to drain.
//!
//! **Pure projection.** Like the §12.8.1/§12.8.2 leaf modules this file owns only
//! the *vocabulary* — the destination ([`PromoteDestination`]), the distilled
//! source ([`PromoteSource`]), and the plain-text projection
//! ([`PromoteSource::project`]). It does NOT depend on `lib.rs`'s `TuiApp`: the
//! caller distills a `SubagentRecord` into a [`PromoteSource`], picks the
//! destination from whether a turn is in flight, and inserts/enqueues the
//! projected string through its own composer/queue primitives.
//!
//! **Idle fills composer; active turn queues.** [`PromoteDestination::for_turn`]
//! maps the single fact the caller knows (is a turn running?) to the destination
//! — idle → [`PromoteDestination::Composer`], running →
//! [`PromoteDestination::Queue`] — so the spec's "idle fills composer; active turn
//! queues prompt" rule lives in one tested place.
//!
//! **Clean plain-text projection.** A subagent's latest line / summary / failure
//! diagnostic can carry terminal decoration (leading bullets, blockquote markers,
//! status prefixes like `subagent failed:`, code-fence backticks, collapsed
//! whitespace). [`PromoteSource::project`] strips that decoration, bounds the body
//! to a sane excerpt ([`PROMOTE_BODY_CAP`] chars on a char boundary), and frames
//! it with a short attribution header naming the subagent and its run state — so
//! the promoted prompt reads as honest, reviewable follow-up work rather than a
//! raw paste of rendered chrome. Nothing here renders terminal cells; it is all
//! plain text the composer/queue then owns.

/// Where a promoted subagent result lands. The spec's two cases: an **idle**
/// session fills the composer (the result becomes editable draft text the user
/// reviews before submitting); an **active** turn queues the prompt (it drains
/// after the running turn, still never auto-submitted because the queue is a
/// user-drained backlog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PromoteDestination {
    /// Fill the composer with the projected prompt as editable draft text. The
    /// idle destination — the user reviews/edits it and submits when ready.
    Composer,
    /// Queue the projected prompt behind the running turn. The active-turn
    /// destination — it drains after the current turn, still user-gated.
    Queue,
}

impl PromoteDestination {
    /// Pick the destination from whether a turn is in flight: idle (`false`)
    /// fills the composer; an active turn (`true`) queues the prompt. The single
    /// place the spec's "idle fills composer; active turn queues prompt" rule
    /// lives.
    pub(crate) fn for_turn(turn_running: bool) -> Self {
        if turn_running {
            PromoteDestination::Queue
        } else {
            PromoteDestination::Composer
        }
    }

    /// A short, screen-reader-friendly verb naming what this destination did, for
    /// the status line (`"filled composer"` / `"queued"`). ASCII only.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            PromoteDestination::Composer => "filled composer",
            PromoteDestination::Queue => "queued",
        }
    }
}

/// The previewed subagent's run state, distilled to exactly the distinctions the
/// promotion header cares about (a mirror of `SubagentLifecycle`, kept here so the
/// pure projection needn't reach back into `lib.rs`). ASCII labels so the header
/// never depends on color or a private-use glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PromoteStatus {
    /// Still running — the promoted body is the latest activity line.
    Running,
    /// Finished successfully — the promoted body is the completion summary.
    Done,
    /// Finished with an error — the promoted body is the failure diagnostic.
    Failed,
    /// Refused before it ran (concurrency cap) — the promoted body is the cap
    /// reason, framed as "this never ran" so the follow-up is honest.
    Capped,
}

impl PromoteStatus {
    /// A short, screen-reader-friendly status word for the attribution header.
    pub(crate) fn label(self) -> &'static str {
        match self {
            PromoteStatus::Running => "running",
            PromoteStatus::Done => "done",
            PromoteStatus::Failed => "failed",
            PromoteStatus::Capped => "capped",
        }
    }

    /// The noun naming what the promoted body *is* for this status — a
    /// completion's `result`, a failure's `failure`, a running agent's latest
    /// `activity`, a cap's `note`. Used to frame the attribution header honestly.
    pub(crate) fn body_noun(self) -> &'static str {
        match self {
            PromoteStatus::Running => "latest activity",
            PromoteStatus::Done => "result",
            PromoteStatus::Failed => "failure",
            PromoteStatus::Capped => "note",
        }
    }
}

/// Largest number of characters retained in the promoted body excerpt. Generous
/// enough to carry a real summary/diagnostic, bounded so a runaway result can
/// never paste a megabyte into the composer (the spec's "excerpt limits").
pub(crate) const PROMOTE_BODY_CAP: usize = 600;

/// What the caller distills from a `SubagentRecord` to promote it: the subagent's
/// display `name`, its `status`, and the raw `result` text (the completion
/// summary, failure diagnostic, latest activity, or cap reason — whichever the
/// status calls for). The projection cleans + bounds the result; the caller need
/// not pre-clean it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromoteSource {
    /// The subagent's display name (e.g. `delegate #2`).
    pub(crate) name: String,
    /// The distilled run status (drives the header word + the body noun).
    pub(crate) status: PromoteStatus,
    /// The raw result text to promote. Cleaned + bounded here, so a caller can
    /// pass the record's `latest` / summary / diagnostic verbatim.
    pub(crate) result: String,
}

impl PromoteSource {
    pub(crate) fn new(name: String, status: PromoteStatus, result: String) -> Self {
        Self {
            name,
            status,
            result,
        }
    }

    /// Whether this source has any usable body text after cleaning. A running
    /// subagent that has reported nothing yet, or a result that is pure
    /// decoration, has no body — the projection then frames the header alone so a
    /// promote never inserts an empty or misleading prompt. Read by the unit suite
    /// to assert the empty-body edge; production relies on [`Self::project`]
    /// folding the same check internally.
    #[cfg(test)]
    pub(crate) fn has_body(&self) -> bool {
        !clean_body(&self.result).is_empty()
    }

    /// Project the source into a clean, plain-text follow-up prompt: a one-line
    /// attribution header naming the subagent + its run state + what the body is,
    /// then a blank line, then the cleaned + bounded body (when present). Pure so
    /// the projection is unit-testable without a terminal; the caller drops the
    /// returned string into the composer or queue verbatim.
    ///
    /// Deliberately **never auto-actionable**: it is descriptive follow-up text
    /// (`"From <name> (<status> <noun>):"`) the user reviews and turns into a real
    /// instruction, not a command the agent would run as-is.
    pub(crate) fn project(&self) -> String {
        let header = format!(
            "From {} ({} {}):",
            self.name.trim(),
            self.status.label(),
            self.status.body_noun(),
        );
        let body = clean_body(&self.result);
        if body.is_empty() {
            // No usable body (e.g. a running subagent that reported nothing yet):
            // frame the header alone so the promote is honest, never an empty or
            // decoration-only paste.
            header
        } else {
            format!("{header}\n\n{body}")
        }
    }
}

/// Strip terminal/markdown decoration from one raw result line and bound it to
/// [`PROMOTE_BODY_CAP`] chars, returning the cleaned body (empty when the source
/// is pure decoration or blank).
///
/// The cleaning, in order: drop leading status prefixes the renderer prepends
/// (`subagent failed:`, `subagent capped:`), strip per-line markdown decoration
/// (blockquote `>`, list bullets `- * +`, code-fence backtick rows), collapse
/// interior whitespace runs to single spaces, join surviving lines with a single
/// space, trim, then char-bound to the cap (appending an ellipsis when cut). All
/// deterministic and honest — a blank or all-decoration source yields `""`.
pub(crate) fn clean_body(raw: &str) -> String {
    let mut pieces: Vec<String> = Vec::new();
    for line in raw.lines() {
        let cleaned = clean_line(line);
        if !cleaned.is_empty() {
            pieces.push(cleaned);
        }
    }
    let joined = pieces.join(" ");
    let collapsed: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PROMOTE_BODY_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(PROMOTE_BODY_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// Strip the leading decoration from a single line and return its bare text. A
/// pure code-fence row (` ``` `) or an empty line returns `""`.
fn clean_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // A code-fence row carries no content — drop it entirely.
    if trimmed.trim_start_matches('`').is_empty() {
        return String::new();
    }
    let mut text = trimmed;
    // Drop the renderer's status prefixes so the promoted body reads as the
    // result itself, not the chrome the timeline wrapped it in.
    for prefix in ["subagent failed:", "subagent capped:", "subagent:"] {
        if let Some(rest) = text
            .strip_prefix(prefix)
            .or_else(|| strip_ascii_ci_prefix(text, prefix))
        {
            text = rest.trim_start();
        }
    }
    // Strip a leading blockquote marker, then any leading list bullet, so a
    // quoted/bulleted result line promotes as plain prose.
    text = text.trim_start_matches('>').trim_start();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            text = rest.trim_start();
            break;
        }
    }
    // Strip surrounding inline-code backticks left after fence removal.
    text = text.trim_matches('`').trim();
    text.to_string()
}

/// ASCII case-insensitive `strip_prefix`. Used so a `Subagent failed:` prefix is
/// dropped regardless of the renderer's casing, without allocating a lowercased
/// copy of the whole (possibly long) line.
fn strip_ascii_ci_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    // `prefix` is a fixed ASCII status tag, so `prefix.len()` is its char count.
    // Guard the boundary before splitting: a `text` that begins with a multibyte
    // char (so the prefix can't possibly match) must not panic `split_at`.
    if text.len() < prefix.len() || !text.is_char_boundary(prefix.len()) {
        return None;
    }
    let (head, rest) = text.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "subagent_promote_tests.rs"]
mod tests;
