//! Subagent Hover Preview And Double-Click Jump (§12.8.2): the same pointer
//! grammar the transcript rows use (§12.1.3 hover-intent + §12.1.4 hover-preview
//! + double-click-to-jump), applied to the subagent timeline rows.
//!
//! Hovering a subagent row (after the shared dwell debounce) reveals a quiet,
//! noncommittal preview popover naming the subagent's status, last activity, and
//! metrics — without resizing the pane. A single click selects/pins that row as
//! the active comparison target; a double-click jumps to that subagent's
//! transcript detail, preserving the user's prior conversation + scroll as a
//! return anchor so they can return. Every mouse affordance has a keyboard twin:
//! the `PreviewSubagent` verb reveals the popover on the *selected* row and the
//! `JumpToSubagent` verb performs the jump — both route to the exact same
//! handlers the mouse path reaches, so keyboard/mouse parity holds by
//! construction.
//!
//! **Pure model.** Like the §12.1.3/§12.1.4 leaf modules (`hover_intent`,
//! `hover_preview`), this file owns only the *vocabulary* — the preview content
//! ([`SubagentPreview`]), the previewed subagent's distilled
//! [`SubagentStatus`], the [`SubagentActivationTarget`] destinations, and the
//! [`SubagentReturnAnchor`] return state — plus the geometry (which it borrows
//! from [`crate::hover_preview::popover_rect`]). It does NOT depend on `lib.rs`'s
//! `TuiApp`: the caller distills a `SubagentRecord` into a [`SubagentPreview`],
//! routes activation through the hit-test registry, and reads the popover rect
//! here.
//!
//! **Quiet by construction.** The resting state is `None` on the app (no preview,
//! no return anchor), so a session that never hovers/previews a subagent pays
//! nothing — zero idle redraw, no allocation.

use crate::hover_preview::{PreviewSource, clamp_line};

/// The previewed subagent's run state, distilled to exactly the distinctions the
/// preview header cares about (a 1:1 mirror of `SubagentLifecycle`, kept here so
/// the pure model needn't reach back into `lib.rs`). ASCII labels so meaning
/// never depends on color or a private-use glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubagentStatus {
    /// Still running.
    Running,
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
    /// Refused before a lease was acquired (concurrency cap hit).
    Capped,
}

impl SubagentStatus {
    /// A short, screen-reader-friendly status word for the popover header.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Done => "done",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Capped => "capped",
        }
    }

    /// Whether this subagent has a transcript worth jumping into. A capped
    /// subagent never acquired a lease and produced no transcript, so a
    /// double-click / jump verb on it is a read-only no-op rather than a jump to
    /// an empty pane — exactly the spec's "missing-metrics / capped" edge.
    pub(crate) fn has_transcript(self) -> bool {
        !matches!(self, SubagentStatus::Capped)
    }
}

/// Where a double-click / jump verb on a subagent row leads. The spec enumerates
/// timeline, transcript-detail, compare, and latest-important-event destinations;
/// the two wired today are the timeline pane (the resting home of the rows) and
/// the transcript detail overlay (the jump target). The remaining variants are
/// the substrate the compare/attention affordances fill later; the classifier
/// ([`activation_target`]) is total over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubagentActivationTarget {
    /// The subagent timeline pane row itself (a select/pin, not a jump).
    TimelinePane,
    /// The subagent's transcript / detail pane — the double-click jump
    /// destination, opened as the full-screen transcript overlay on the
    /// activated conversation source.
    TranscriptDetail,
}

/// The return state captured the instant a jump opens a subagent's transcript, so
/// a back command can restore the user's prior reading position. Stored as a
/// pure-data triple (the prior selected row, whether the prior active source was
/// the main conversation, and the prior main-view scroll offset) so this module
/// needn't depend on `lib.rs`'s `ConversationSource`/`ScrollState`. The caller
/// re-applies it through its own setters on return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubagentReturnAnchor {
    /// The pane row that was selected before the jump (0 = `main`), so a return
    /// re-seats the cursor where it was.
    pub(crate) prior_selected: usize,
    /// Whether the prior *active* conversation source was the main conversation
    /// (vs. some subagent). On return the caller restores the main source when
    /// this is `true`.
    pub(crate) prior_was_main: bool,
}

impl SubagentReturnAnchor {
    pub(crate) fn new(prior_selected: usize, prior_was_main: bool) -> Self {
        Self {
            prior_selected,
            prior_was_main,
        }
    }
}

/// Classify what activating a subagent row does given its status. A subagent with
/// a transcript jumps to its [`SubagentActivationTarget::TranscriptDetail`]; a
/// capped one (no transcript) stays on the [`SubagentActivationTarget::TimelinePane`]
/// as a select/pin only, so a double-click never opens an empty detail pane.
pub(crate) fn activation_target(status: SubagentStatus) -> SubagentActivationTarget {
    if status.has_transcript() {
        SubagentActivationTarget::TranscriptDetail
    } else {
        SubagentActivationTarget::TimelinePane
    }
}

/// Largest number of body lines retained in a subagent preview popover. One short
/// header line (status + last activity) plus a metrics line: long enough to
/// disambiguate, short enough that the popover stays a quiet, fixed-size
/// affordance.
pub(crate) const SUBAGENT_PREVIEW_BODY_LINES: usize = 3;

/// A live subagent hover/keyboard preview: the previewed row's 0-based pane index
/// (NOT a screen coordinate, so a reflow re-anchors it), the subagent's name,
/// distilled status, a bounded secret-free last-activity excerpt, an optional
/// metrics summary, and whether activation jumps (vs. is a read-only select). The
/// resting state is `None` on the app, so a session that never previews a
/// subagent pays nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentPreview {
    /// The previewed subagent's 0-based index into the pane's record list — the
    /// stable anchor, resolved to a live record at activation time so a reflow /
    /// prune can't stale it. (Row `index + 1` in the pane, since row 0 is
    /// `main`.)
    pub(crate) index: usize,
    /// The subagent's display name (e.g. `delegate #2`).
    pub(crate) name: String,
    /// The distilled run status (drives the header word + the jump/read-only
    /// footer).
    pub(crate) status: SubagentStatus,
    /// A bounded, secret-free one-line last-activity excerpt. May be empty — the
    /// popover then shows the status header alone.
    pub(crate) last_activity: String,
    /// A short metrics summary (e.g. `tools=4 · bytes=2048`), or `None` when the
    /// subagent has reported none yet (the running / missing-metrics edge).
    pub(crate) metrics: Option<String>,
    /// How this preview was requested (a stable mouse hover vs. the keyboard
    /// verb). A keyboard-pinned peek is sticky against incidental mouse drift,
    /// mirroring the §12.1.4 contract.
    pub(crate) source: PreviewSource,
}

impl SubagentPreview {
    /// Build a preview for the subagent at pane index `index`. `name` and
    /// `last_activity` are clamped to a single bounded line as a defensive
    /// backstop so a careless caller can never blow the popover's fixed size.
    pub(crate) fn new(
        index: usize,
        name: String,
        status: SubagentStatus,
        last_activity: String,
        metrics: Option<String>,
        source: PreviewSource,
    ) -> Self {
        Self {
            index,
            name: clamp_line(&name),
            status,
            last_activity: clamp_line(&last_activity),
            metrics: metrics.map(|m| clamp_line(&m)),
            source,
        }
    }

    /// The bounded body excerpt lines for the popover: the last-activity line
    /// (when non-empty) then the metrics line (when present), capped at
    /// [`SUBAGENT_PREVIEW_BODY_LINES`]. Built off semantic state, never rendered
    /// terminal cells.
    pub(crate) fn body(&self) -> Vec<String> {
        let mut body = Vec::with_capacity(SUBAGENT_PREVIEW_BODY_LINES);
        if !self.last_activity.is_empty() {
            body.push(self.last_activity.clone());
        }
        if let Some(metrics) = &self.metrics {
            body.push(metrics.clone());
        }
        body.truncate(SUBAGENT_PREVIEW_BODY_LINES);
        body
    }

    /// Whether activating this preview (double-click / jump verb) jumps into a
    /// transcript, vs. being a read-only select (a capped subagent has none).
    pub(crate) fn can_jump(&self) -> bool {
        matches!(
            activation_target(self.status),
            SubagentActivationTarget::TranscriptDetail
        )
    }

    /// Whether the keyboard verb pinned this preview. A keyboard-pinned peek is
    /// sticky: an incidental mouse move onto empty space must not dismiss it
    /// (only an explicit key/click does), so it does not vanish out from under a
    /// keyboard-only user the instant the pointer drifts.
    pub(crate) fn is_keyboard(&self) -> bool {
        matches!(self.source, PreviewSource::Keyboard)
    }

    /// A short hint line naming the activation verb, for the popover footer —
    /// honest about whether double-click / jump does anything here.
    pub(crate) fn activate_hint(&self) -> &'static str {
        if self.can_jump() {
            "double-click / jump to open transcript"
        } else {
            "click to select"
        }
    }
}

#[cfg(test)]
#[path = "subagent_preview_tests.rs"]
mod tests;
