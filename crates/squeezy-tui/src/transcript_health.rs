//! Transcript Health Markers (§12.5.7): small, explicit status markers that tell
//! the user *what is hidden or degraded* about a transcript entry — a tool that
//! failed, output that was elided to a preview, a large output blob, a failed
//! subagent, or a failed turn — without making them open the entry to find out.
//!
//! A [`HealthMarker`] carries the stable [`TranscriptEntry::id`](crate::TranscriptEntry)
//! it lives on (the quick-jump target), a [`HealthKind`] (the *why*), a
//! [`HealthSeverity`] (how loud), and a short human message. The module is
//! deliberately pure: it owns the detection + marker bookkeeping and nothing
//! about geometry, rendering, or input. `lib.rs` collects each entry's facts
//! into a [`HealthCandidate`] — its id, content revision, kind, structured
//! failure flags, and the *measured* output size / elision — and feeds the slice
//! in; this module turns those facts into markers and answers list/navigation
//! queries. That keeps the classification math testable without a terminal.
//!
//! **Structured facts win, never a regex.** The spec warns that marker noise can
//! be high and that a marker must never reveal hidden secret content. So a
//! candidate carries already-computed booleans/counts (`failed`, `elided`,
//! `output_bytes`, `line_count`, …) measured by the renderer's own pipeline, and
//! the detector here only *decides which markers those facts justify*. It never
//! re-reads the output text, so it cannot leak a secret line and cannot
//! false-positive on output that merely *mentions* "error".
//!
//! **Stable ids, never row offsets.** Like the transcript index (§12.5.1), the
//! relation graph (§12.5.3), the duplicate-fold model (§12.5.4), and the error
//! lenses (§12.5.6), every marker is keyed by its source `TranscriptEntry::id`,
//! never a width-/fold-dependent row coordinate. An id survives reflow (resize,
//! streaming, collapse, coalescing), so a marker built before a reflow still
//! resolves to the right entry afterwards. Ids whose entry was dropped fall out
//! on the next rebuild.
//!
//! **Zero idle cost, incremental rebuild.** The model carries a `fingerprint`
//! folded over every candidate `(id, revision)` plus its elision facts
//! (`elided`, `hidden_lines`, `output_bytes`) — those are derived from the
//! verbosity / preview-cap settings rather than `revision`, so they must be
//! hashed explicitly or a verbosity cycle would not re-detect. The caller feeds the same
//! fingerprint each refresh via [`HealthMarkers::rebuild_if_stale`]; when it
//! matches the stored one the call returns immediately and touches nothing. The
//! detectors only re-run when the transcript actually changed — exactly the
//! events that move the fingerprint. An idle session pays one cheap `u64`
//! comparison per refresh.

use std::hash::{Hash, Hasher};

/// The *why* of a health marker — a small, fixed set, one per category of
/// hidden/degraded state the spec and task call out. Ordered so [`HealthKind::ALL`]
/// reads the way the overlay groups them (failures first, then elision/size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HealthKind {
    /// A tool call whose structured status is not success (error / stale /
    /// denied / cancelled). The entry is collapsed by default, so the failure is
    /// easy to scroll past — the marker keeps it visible.
    ToolFailed,
    /// A subagent breadcrumb (or other log) that reports a failure. Delegated
    /// work that failed is otherwise a quiet one-line note.
    SubagentFailed,
    /// An assistant turn that ended in failure (cancelled / errored). The marker
    /// flags it so a long transcript doesn't bury a dead turn.
    TurnFailed,
    /// A tool output whose preview was elided / head-tail truncated — the user
    /// sees only the head and tail, with `+N lines` hidden in the inline card.
    OutputElided,
    /// A tool output whose total size is large (over [`LARGE_OUTPUT_BYTES`]),
    /// even if it was not elided. A heads-up that the entry carries a heavy blob.
    LargeOutput,
}

impl HealthKind {
    /// Every kind, in overlay grouping order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in the summary.
    pub(crate) const ALL: &'static [HealthKind] = &[
        HealthKind::ToolFailed,
        HealthKind::SubagentFailed,
        HealthKind::TurnFailed,
        HealthKind::OutputElided,
        HealthKind::LargeOutput,
    ];

    /// Short, screen-reader-friendly label. ASCII only (no glyphs) so the marker
    /// carries meaning without relying on color or a private-use codepoint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            HealthKind::ToolFailed => "tool failed",
            HealthKind::SubagentFailed => "subagent failed",
            HealthKind::TurnFailed => "turn failed",
            HealthKind::OutputElided => "output elided",
            HealthKind::LargeOutput => "large output",
        }
    }
}

/// How loud a marker is. `Important` markers are the ones the spec says render as
/// rows (a failure the user must not miss); `Minor` markers render as quieter
/// badges (a heads-up about hidden/large output). Ordered so an important marker
/// sorts ahead of a minor one when both appear on the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum HealthSeverity {
    /// A failure-class marker — rendered as a prominent row.
    Important,
    /// A heads-up marker (elision / size) — rendered as a quiet badge.
    Minor,
}

impl HealthSeverity {
    /// ASCII label for the readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            HealthSeverity::Important => "important",
            HealthSeverity::Minor => "minor",
        }
    }
}

/// One transcript-entry candidate the caller feeds in. Carries the *facts* the
/// renderer already measured — never the output text — so the detector can
/// classify without re-reading (and without risking a secret leak).
///
/// `id` is the stable `TranscriptEntry::id`; `revision` is its content revision
/// (folded into the staleness fingerprint so a mutation re-detects). The
/// remaining fields are the measured signals: whether the entry's structured
/// status is a failure, whether it is a failed subagent/log, whether it is a
/// failed turn, whether its preview was elided, the elided/hidden line count, and
/// its total output size in bytes. A short `title` (already bounded and
/// secret-free — a tool name, never output) labels the marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthCandidate {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    /// Short, secret-free label for the entry (e.g. the tool name) used in the
    /// marker message. Never raw output.
    pub(crate) title: String,
    /// Structured tool failure: the tool's status is not success.
    pub(crate) tool_failed: bool,
    /// A subagent/log breadcrumb that reports a failure.
    pub(crate) subagent_failed: bool,
    /// An assistant turn that ended in failure.
    pub(crate) turn_failed: bool,
    /// The inline preview was elided / head-tail truncated.
    pub(crate) elided: bool,
    /// Number of lines hidden by the elision (0 when not elided / unknown).
    pub(crate) hidden_lines: usize,
    /// Total output size in bytes (0 when none / unknown).
    pub(crate) output_bytes: u64,
}

/// One detected health marker (§12.5.7). `entry_id` is the stable
/// `TranscriptEntry::id` of the entry it lives on (the quick-jump target);
/// `kind` is the *why*; `severity` is how loud; `message` is a short, bounded,
/// secret-free human description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthMarker {
    pub(crate) entry_id: u64,
    pub(crate) kind: HealthKind,
    pub(crate) severity: HealthSeverity,
    pub(crate) message: String,
}

/// Output at or above this many bytes earns a [`HealthKind::LargeOutput`] marker.
/// 16 KiB is large enough that a non-elided blob of this size is worth flagging
/// as "heavy" but small enough that a routine multi-kilobyte result does not spam
/// the marker list.
pub(crate) const LARGE_OUTPUT_BYTES: u64 = 16 * 1024;

/// Largest number of characters retained in a marker `message`. The message is
/// already built from a bounded title + a small numeric, but cap defensively so a
/// pathological title can never blow up the overlay row.
const MESSAGE_CAP: usize = 120;

/// Truncate `s` to at most [`MESSAGE_CAP`] chars (on a char boundary), appending
/// an ellipsis when it was cut.
fn cap_message(s: &str) -> String {
    if s.chars().count() <= MESSAGE_CAP {
        return s.to_string();
    }
    let prefix: String = s.chars().take(MESSAGE_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// Human-format a byte count compactly (`"512B"`, `"4.0KB"`, `"2.5MB"`).
/// Self-contained so the pure module has no dependency on the renderer's
/// formatter; only used to build marker messages.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// Detect the health markers justified by one candidate's *facts*. Pure and
/// standalone so it is the unit-testable heart of the feature. Emits at most one
/// failure marker (the most important class wins so a failed tool that is also
/// large does not double-count its failure) plus, independently, an elision or
/// size heads-up. The `title` is stamped into the message so a jump lands with
/// context. Returns markers in severity order (important before minor).
pub(crate) fn detect_for_candidate(candidate: &HealthCandidate) -> Vec<HealthMarker> {
    let mut markers: Vec<HealthMarker> = Vec::new();
    let title = candidate.title.trim();

    // --- Failure markers (Important). One per candidate: a candidate is a tool
    // OR a subagent log OR an assistant turn, never several at once, but the
    // detector is defensive and prefers the most specific failure if facts
    // overlap. ---
    if candidate.tool_failed {
        markers.push(HealthMarker {
            entry_id: candidate.id,
            kind: HealthKind::ToolFailed,
            severity: HealthSeverity::Important,
            message: cap_message(&format!("{title} failed")),
        });
    } else if candidate.subagent_failed {
        markers.push(HealthMarker {
            entry_id: candidate.id,
            kind: HealthKind::SubagentFailed,
            severity: HealthSeverity::Important,
            message: cap_message(&format!("{title} reported a failure")),
        });
    } else if candidate.turn_failed {
        markers.push(HealthMarker {
            entry_id: candidate.id,
            kind: HealthKind::TurnFailed,
            severity: HealthSeverity::Important,
            message: cap_message(&format!("{title} did not finish")),
        });
    }

    // --- Hidden-content markers (Minor). Elision and size are independent of the
    // failure classification — a failed tool can also have elided output — but we
    // surface only one hidden-content heads-up per entry, preferring the more
    // actionable "elided" (content the user cannot see inline) over the bare
    // "large" size heads-up. ---
    if candidate.elided {
        let hidden = candidate.hidden_lines;
        let msg = if hidden > 0 {
            format!("{title}: +{hidden} lines hidden in preview")
        } else {
            format!("{title}: output elided in preview")
        };
        markers.push(HealthMarker {
            entry_id: candidate.id,
            kind: HealthKind::OutputElided,
            severity: HealthSeverity::Minor,
            message: cap_message(&msg),
        });
    } else if candidate.output_bytes >= LARGE_OUTPUT_BYTES {
        markers.push(HealthMarker {
            entry_id: candidate.id,
            kind: HealthKind::LargeOutput,
            severity: HealthSeverity::Minor,
            message: cap_message(&format!(
                "{title}: {} of output",
                format_bytes(candidate.output_bytes)
            )),
        });
    }

    markers
}

/// The computed Transcript-Health model over the transcript's entries (§12.5.7).
///
/// `markers` is the ordered list of detected markers (transcript order by entry,
/// then severity within each entry). `fingerprint` is the staleness tag described
/// in the module docs; `built` distinguishes "empty transcript scanned" from
/// "never built" so a genuinely healthy transcript is not re-scanned every
/// refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct HealthMarkers {
    markers: Vec<HealthMarker>,
    fingerprint: u64,
    built: bool,
}

impl HealthMarkers {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the candidates. Order- and
    /// content-sensitive: id and revision both participate, so an append, a
    /// revision bump, a reorder, or a drop all move the value. Pure and
    /// standalone so the caller can compute it cheaply each refresh and compare
    /// before deciding to recompute. The elision facts (`elided`,
    /// `hidden_lines`, `output_bytes`) are also folded in: they are derived from
    /// the verbosity / preview-cap settings, *not* from `entry.revision`, so a
    /// verbosity cycle that keeps an entry elided while changing its hidden count
    /// must still move the value or the overlay would keep showing a stale
    /// "+N lines hidden in preview".
    pub(crate) fn fingerprint_of<'a>(
        candidates: impl IntoIterator<Item = &'a HealthCandidate>,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for c in candidates {
            c.id.hash(&mut hasher);
            c.revision.hash(&mut hasher);
            c.elided.hash(&mut hasher);
            c.hidden_lines.hash(&mut hasher);
            c.output_bytes.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the marker model from `candidates` **only if** `fingerprint`
    /// differs from the one captured at the last rebuild (or this is the first
    /// build). Returns `true` when a recompute actually ran, `false` when the
    /// cached model was already current (the zero-idle-cost fast path).
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        candidates: &[HealthCandidate],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.markers.clear();
        for candidate in candidates {
            self.markers.extend(detect_for_candidate(candidate));
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

    /// The detected markers in transcript order (entry order, then severity).
    pub(crate) fn markers(&self) -> &[HealthMarker] {
        &self.markers
    }

    /// Number of detected markers.
    pub(crate) fn len(&self) -> usize {
        self.markers.len()
    }

    /// Whether any marker was detected.
    pub(crate) fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// The marker at list index `index`, or `None` when out of range.
    pub(crate) fn get(&self, index: usize) -> Option<&HealthMarker> {
        self.markers.get(index)
    }

    /// Number of markers of `kind`.
    pub(crate) fn count_of(&self, kind: HealthKind) -> usize {
        self.markers.iter().filter(|m| m.kind == kind).count()
    }

    /// The list index of the next marker strictly after `after` (wrapping to the
    /// first when `after` is the last or `None`). `None` only when there are no
    /// markers. Drives forward quick-jump navigation.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        if self.markers.is_empty() {
            return None;
        }
        match after {
            Some(i) if i + 1 < self.markers.len() => Some(i + 1),
            // Last (or out of range): wrap to the first.
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The list index of the previous marker strictly before `before` (wrapping
    /// to the last). `None` only when there are no markers. Drives backward
    /// quick-jump navigation; the overlay walks forward today, so this is
    /// exercised by the unit suite until a "previous marker" verb lands.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        if self.markers.is_empty() {
            return None;
        }
        match before {
            Some(0) | None => Some(self.markers.len() - 1),
            Some(i) if i <= self.markers.len() => Some(i - 1),
            // Out of range: wrap to the last.
            Some(_) => Some(self.markers.len() - 1),
        }
    }

    /// A compact one-line summary of the detected markers for the status line /
    /// overlay header, e.g. `"3 markers \u{00b7} 1 tool failed \u{00b7} 2 output
    /// elided"`. Empty string when nothing was detected.
    pub(crate) fn summary(&self) -> String {
        if self.markers.is_empty() {
            return String::new();
        }
        let total = self.markers.len();
        let total_word = if total == 1 { "marker" } else { "markers" };
        let mut parts = vec![format!("{total} {total_word}")];
        for kind in HealthKind::ALL.iter().copied() {
            let n = self.count_of(kind);
            if n > 0 {
                parts.push(format!("{n} {}", kind.label()));
            }
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "transcript_health_tests.rs"]
mod tests;
