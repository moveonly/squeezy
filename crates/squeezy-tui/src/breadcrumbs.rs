//! Clickable Breadcrumbs (§12.1.5): a compact breadcrumb trail of the current
//! context/location — `session ▸ turn ▸ entry`, with an `overlay`/`search`
//! suffix when one is active — that orients long sessions without permanent
//! instructional chrome.
//!
//! Each crumb is a clickable AND keyboard-focusable jump/action target: a click
//! (or `Shift+Enter` while the crumb is keyboard-focused) navigates to the
//! location the crumb stands for. The trail is *derived from model state each frame*
//! ([`BreadcrumbModel::build`]) — never cached across frames — so it can never go
//! stale after a resize, a scroll, a fold, or a Ctrl+T toggle (the spec's "derive
//! from model each frame" risk mitigation).
//!
//! **Pure, terminal-free, id-keyed.** Like `turn_outline.rs` and the rest of the
//! leaf model modules, this owns no geometry, rendering, or input. `lib.rs`
//! gathers the live focus / tail / overlay / search facts into a
//! [`BreadcrumbContext`] and feeds it in; this turns those facts into an ordered
//! list of [`Crumb`]s, each carrying a stable [`BreadcrumbTarget`] (an entry id,
//! the transcript home/tail, the open overlay, or the active search), never a
//! screen coordinate. The renderer registers each crumb's rect in the hit-test
//! registry by its 0-based index and the dispatch maps that index back to the
//! crumb's target, so the keyboard path (Left/Right to move the focus,
//! `Shift+Enter` to activate) and the mouse path reach the same navigation by
//! construction.
//!
//! **Zero idle cost.** The trail is built only while the breadcrumb strip is
//! shown (the `Alt+2` focus mode is on); a closed strip builds nothing and the
//! model is never touched on an idle frame.

use unicode_width::UnicodeWidthStr;

/// What activating a crumb navigates to. Every variant maps to an existing jump
/// path in `lib.rs` so a click and the keyboard reach the same handler:
/// `Home`/`Tail` reuse the transcript home/jump-to-latest verbs, `Entry` reuses
/// the id-keyed `jump_to_entry_id` path (so it survives reflow), `CloseOverlay`
/// reuses the Ctrl+T toggle, and `Search` re-opens the incremental search the
/// trail is reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreadcrumbTarget {
    /// The session root: scroll the transcript to the top (home).
    Home,
    /// The live tail: jump the transcript to the latest (newest) row.
    Tail,
    /// A specific transcript entry, addressed by its stable `TranscriptEntry::id`
    /// so the jump survives reflow/resize/fold (never a row offset).
    Entry(u64),
    /// The open Ctrl+T transcript overlay: activating the crumb closes it,
    /// returning to the main surface.
    CloseOverlay,
    /// The active incremental search: activating the crumb re-opens the search
    /// mini-buffer so the user can refine the query.
    Search,
}

/// One segment of the breadcrumb trail. `label` is the short, bounded,
/// secret-free display text; `target` is where activating it navigates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Crumb {
    pub(crate) label: String,
    pub(crate) target: BreadcrumbTarget,
}

/// The live facts `lib.rs` gathers each frame to build the trail. Deliberately
/// small and copy-cheap: a session label, whether the view is following the tail,
/// the focused entry's id + short kind label (when an entry is focused), whether
/// the Ctrl+T overlay is open, and whether an incremental search is active (plus
/// its query for the crumb label). All already-bounded, secret-free fields.
#[derive(Debug, Clone, Default)]
pub(crate) struct BreadcrumbContext {
    /// Short session label (e.g. a truncated session id), or `None` to fall back
    /// to the generic `"session"` root label.
    pub(crate) session_label: Option<String>,
    /// True when the main view is pinned to the live tail (newest row visible and
    /// no entry focused) — the trail then shows a `tail` turn crumb.
    pub(crate) following_tail: bool,
    /// The focused transcript entry's stable id + short kind label
    /// (`"user"`/`"assistant"`/`"tool"`/…), or `None` when nothing is focused.
    pub(crate) focused_entry: Option<(u64, String)>,
    /// True while the Ctrl+T transcript overlay is open.
    pub(crate) overlay_open: bool,
    /// The active incremental-search query, or `None` when search is closed.
    pub(crate) search_query: Option<String>,
}

/// Largest number of characters retained in a crumb `label`. One short token:
/// long enough to disambiguate, short enough that the trail does not blow past
/// the status row even with several crumbs.
const LABEL_CAP: usize = 24;

/// The glyph painted between crumbs. A right-pointing small triangle, the
/// conventional breadcrumb separator.
pub(crate) const SEPARATOR: &str = " \u{25b8} ";

/// Clean a raw crumb-label source into a bounded one-line token: collapse
/// interior whitespace, trim, and cap to [`LABEL_CAP`] chars on a char boundary
/// (appending an ellipsis when cut). Deterministic and total — an empty source
/// yields an empty string, which the builder never feeds in (it always supplies a
/// non-empty fallback).
pub(crate) fn clean_label(raw: &str) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= LABEL_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(LABEL_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// The computed breadcrumb trail (§12.1.5): an ordered list of crumbs, root-first.
///
/// Built fresh each frame from a [`BreadcrumbContext`] via [`Self::build`] — never
/// cached — so it always reflects the current focus/overlay/search state.
#[derive(Debug, Clone, Default)]
pub(crate) struct BreadcrumbModel {
    crumbs: Vec<Crumb>,
}

impl BreadcrumbModel {
    /// Build the trail from the live context. The order is root-first:
    ///
    /// 1. **session** — always present; the trail's root. Activating it jumps
    ///    home (the session top).
    /// 2. **turn** — `tail` when following the live tail, otherwise omitted in
    ///    favor of a focused entry crumb. Activating `tail` jumps to the latest
    ///    row.
    /// 3. **entry** — the focused entry's kind label, when one is focused.
    ///    Activating it jumps to that entry by its stable id.
    /// 4. **overlay** / **search** — appended as a path suffix when the Ctrl+T
    ///    overlay is open or an incremental search is active.
    ///
    /// Always emits at least the session crumb, so an empty session still shows a
    /// (single-crumb) orienting root rather than nothing.
    pub(crate) fn build(ctx: &BreadcrumbContext) -> Self {
        let mut crumbs = Vec::new();

        // 1. Session root — always present.
        let session_label = ctx
            .session_label
            .as_deref()
            .map(clean_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "session".to_string());
        crumbs.push(Crumb {
            label: session_label,
            target: BreadcrumbTarget::Home,
        });

        // 2/3. Turn / entry. A focused entry wins (it is the precise location);
        // otherwise, following the tail shows a `tail` crumb so the user can see
        // — and click — that the view is pinned to the newest row.
        match &ctx.focused_entry {
            Some((entry_id, kind_label)) => {
                let label = clean_label(kind_label);
                crumbs.push(Crumb {
                    label: if label.is_empty() {
                        "entry".to_string()
                    } else {
                        label
                    },
                    target: BreadcrumbTarget::Entry(*entry_id),
                });
            }
            None if ctx.following_tail => {
                crumbs.push(Crumb {
                    label: "tail".to_string(),
                    target: BreadcrumbTarget::Tail,
                });
            }
            None => {}
        }

        // 4. Overlay / search path suffix.
        if ctx.overlay_open {
            crumbs.push(Crumb {
                label: "overlay".to_string(),
                target: BreadcrumbTarget::CloseOverlay,
            });
        }
        if let Some(query) = &ctx.search_query {
            let trimmed = clean_label(query);
            let label = if trimmed.is_empty() {
                "search".to_string()
            } else {
                format!("search:{trimmed}")
            };
            crumbs.push(Crumb {
                label: clean_label(&label),
                target: BreadcrumbTarget::Search,
            });
        }

        Self { crumbs }
    }

    /// The crumbs, root-first.
    pub(crate) fn crumbs(&self) -> &[Crumb] {
        &self.crumbs
    }

    /// Number of crumbs in the trail (always >= 1 after [`Self::build`]).
    pub(crate) fn len(&self) -> usize {
        self.crumbs.len()
    }

    /// Whether the trail has no crumbs. Only true for a never-built model; a
    /// built trail always carries at least the session root.
    pub(crate) fn is_empty(&self) -> bool {
        self.crumbs.is_empty()
    }

    /// The crumb at list index `index`, or `None` when out of range.
    pub(crate) fn get(&self, index: usize) -> Option<&Crumb> {
        self.crumbs.get(index)
    }

    /// The target of the crumb at `index`, or `None` when out of range. Maps a
    /// crumb index back to its navigation; the production dispatch reads the crumb
    /// via [`Self::get`] (it also needs the label for the status line), so this
    /// thin accessor is exercised only by the unit suite that pins the index →
    /// target mapping.
    #[cfg(test)]
    pub(crate) fn target_at(&self, index: usize) -> Option<BreadcrumbTarget> {
        self.crumbs.get(index).map(|c| c.target)
    }

    /// The list index of the next crumb to the right of `from`, clamped to the
    /// last crumb (no wrap — the trail reads left-to-right and the rightmost crumb
    /// is the deepest/current location). `None` only when the trail is empty.
    pub(crate) fn next_index(&self, from: usize) -> Option<usize> {
        if self.crumbs.is_empty() {
            return None;
        }
        Some((from + 1).min(self.crumbs.len() - 1))
    }

    /// The list index of the previous crumb to the left of `from`, clamped to the
    /// first crumb (no wrap). `None` only when the trail is empty.
    pub(crate) fn prev_index(&self, from: usize) -> Option<usize> {
        if self.crumbs.is_empty() {
            return None;
        }
        Some(from.saturating_sub(1))
    }

    /// The total display width of the trail at full size: every crumb label plus
    /// a separator between each pair. Used by the renderer to decide when middle
    /// truncation is needed. Measured in terminal display cells (not chars or
    /// bytes) via [`UnicodeWidthStr`] so a label with wide (CJK / 2-cell) glyphs
    /// measures the same number of columns the renderer actually paints.
    pub(crate) fn full_width(&self) -> usize {
        if self.crumbs.is_empty() {
            return 0;
        }
        let labels: usize = self
            .crumbs
            .iter()
            .map(|c| UnicodeWidthStr::width(c.label.as_str()))
            .sum();
        let seps = self.crumbs.len().saturating_sub(1) * UnicodeWidthStr::width(SEPARATOR);
        labels + seps
    }
}

#[cfg(test)]
#[path = "breadcrumbs_tests.rs"]
mod tests;
