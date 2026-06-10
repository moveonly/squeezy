//! Hover Preview And Double-Click Activation (§12.1.4): the shared pointer
//! contract for every interactive transcript row/item.
//!
//! Every interactive target obeys the same contract: hover (or the keyboard
//! preview verb on the focused/top-visible entry) gives a *quiet, noncommittal*
//! preview popover; a single click selects/focuses; a double-click (or the
//! keyboard activate verb) performs the natural primary action. The double-click
//! path routes to the *same* [`interaction::Action`] the keyboard already
//! reaches, so mouse and keyboard reach identical behavior by construction and
//! nothing here mutates the transcript — and double-click never triggers a
//! destructive verb directly (delete/retry/export stay behind explicit commands).
//!
//! **Pure model.** Like the other §12 leaf modules (`action_palette`,
//! `turn_outline`, `change_summary`), this file owns only the *vocabulary*
//! (the per-target [`PointerActivationPolicy`], the [`PreviewKind`]) and the
//! *preview content / geometry* — it does NOT depend on `lib.rs`'s `TuiApp`.
//! The caller classifies the hovered/focused target, asks [`policy_for`] for its
//! contract, builds a [`HoverPreview`] from semantic state (never from rendered
//! terminal cells), and routes activation through the hit-test registry.
//!
//! **Quiet by construction.** The preview is a fixed-size popover anchored near
//! the target row; [`popover_rect`] clamps it inside the frame so it never
//! overflows and the hover styling never changes a row's height/width — exactly
//! the spec's "never changes layout height or steals keyboard focus" contract.

use ratatui::layout::Rect;

use crate::interaction::{Action, TargetKey};

/// The semantic class of a previewable target, distilled to exactly the
/// distinctions the preview body cares about. The caller maps a hovered/focused
/// target onto one of these so the popover header names *what* it is previewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PreviewKind {
    /// A whole transcript entry (message / reasoning / plan / diff / note): the
    /// preview shows the entry's short title + a bounded body excerpt.
    Entry,
    /// A tool result: the preview shows the tool name + a bounded output excerpt.
    ToolOutput,
    /// A file path target: the preview shows the path + its open-in-detail intent.
    Path,
    /// A link / breadcrumb jump target: the preview shows where it leads.
    Link,
}

impl PreviewKind {
    /// A short, screen-reader-friendly noun for the popover header, so the
    /// preview says *what* it is previewing. ASCII only — meaning never depends
    /// on color or a private-use glyph.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            PreviewKind::Entry => "entry",
            PreviewKind::ToolOutput => "tool output",
            PreviewKind::Path => "path",
            PreviewKind::Link => "link",
        }
    }
}

/// The pointer contract for a [`TargetKey`] kind: which gestures apply and what
/// the primary (double-click / activate) and optional secondary verbs route to.
///
/// Built per-kind by [`policy_for`]. The `primary_activate` action is the *same*
/// [`interaction::Action`] the keyboard equivalent dispatches, so a double-click
/// and the keyboard activate verb reach one handler. `primary_activate` is never
/// a destructive verb (delete/clear/retry) — those stay behind explicit command
/// rows, satisfying the spec's "double-click never triggers a destructive action
/// directly" contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerActivationPolicy {
    /// Whether hovering this target reveals a preview popover.
    pub(crate) hover_preview: bool,
    /// Whether a single click selects/focuses this target.
    pub(crate) select: bool,
    /// The natural primary action (double-click / Enter). `None` when the target
    /// has no non-destructive primary verb.
    pub(crate) primary_activate: Option<Action>,
    /// An optional secondary action (e.g. right-click / context). `None` for
    /// every target today; the field is the substrate the contextual-menu
    /// affordance fills later.
    pub(crate) secondary_activate: Option<Action>,
}

impl PointerActivationPolicy {
    /// A target that does nothing on hover/click — the safe default for any key
    /// without an explicit non-destructive contract.
    const fn inert() -> Self {
        Self {
            hover_preview: false,
            select: false,
            primary_activate: None,
            secondary_activate: None,
        }
    }

    /// Whether a double-click on this target should activate its primary verb.
    /// True only when a primary verb exists; a target with no primary verb (or a
    /// purely destructive one, which is never stored here) double-clicks to a
    /// no-op rather than a surprise mutation.
    pub(crate) fn activates_on_double_click(self) -> bool {
        self.primary_activate.is_some()
    }
}

/// The pointer contract for `key`. Total over [`TargetKey`]: a transcript entry
/// hovers + selects + activates into the Ctrl+T detail overlay (the same
/// non-destructive verb `Ctrl+Enter` reaches); a code-block sub-row affordance
/// hovers + selects (its copy verb is explicit, not a double-click); every other
/// chrome/queue/clipboard key is left inert here so a double-click on it can
/// never fire a destructive verb — those targets keep their own single-click
/// dispatch and explicit command rows.
///
/// Keeping the destructive verbs (queue/clipboard delete, clear) out of
/// `primary_activate` is the mechanism behind the spec's "double-click never
/// triggers a destructive action directly" guarantee.
pub(crate) fn policy_for(key: TargetKey) -> PointerActivationPolicy {
    match key {
        // A whole transcript entry: hover previews it, a single click focuses it,
        // a double-click opens it in the Ctrl+T detail overlay — the same
        // non-destructive verb the `Ctrl+Enter` keyboard chord reaches.
        TargetKey::Entry(id) => PointerActivationPolicy {
            hover_preview: true,
            select: true,
            primary_activate: Some(Action::OpenEntryInDetail(id)),
            secondary_activate: None,
        },
        // A code-block sub-row affordance: hover previews the row, a click
        // selects it. Its copy verb is an explicit affordance, not a
        // double-click, so no primary verb is stored.
        TargetKey::RowSpan(_, _) => PointerActivationPolicy {
            hover_preview: true,
            select: true,
            primary_activate: None,
            secondary_activate: None,
        },
        // Queue items, clipboard entries, and chrome keys keep their own
        // single-click dispatch (which already includes their destructive verbs
        // behind explicit targets); they are inert under the shared
        // double-click/preview contract so a stray double-click can never fire a
        // delete/clear/reorder by accident.
        TargetKey::QueueItem(_) | TargetKey::ClipboardEntry(_) | TargetKey::Chrome(_) => {
            PointerActivationPolicy::inert()
        }
    }
}

/// Whether the chrome key behind a hover is a *destructive* affordance whose
/// preview/activation must stay behind an explicit click — used by the caller to
/// double-check it never wires a destructive verb onto the double-click path.
/// `cfg(test)`-only: the only consumer is the destructive-safety unit test, so
/// it carries no runtime weight on any platform.
#[cfg(test)]
pub(crate) fn is_destructive_chrome(key: crate::interaction::ChromeKey) -> bool {
    use crate::interaction::ChromeKey;
    matches!(key, ChromeKey::ClipboardDelete | ChromeKey::ClipboardClear)
}

/// Largest number of body lines retained in a preview popover. One short header
/// plus a few excerpt lines: long enough to disambiguate, short enough that the
/// popover never dominates the surface.
pub(crate) const PREVIEW_BODY_LINES: usize = 4;

/// Largest number of characters retained in a single preview line. Bounds the
/// popover width so it stays a quiet, fixed-size affordance.
pub(crate) const PREVIEW_LINE_CAP: usize = 72;

/// How a live preview was requested — so the popover header can read honestly and
/// the caller can suppress the mouse-driven one during scroll/drag/selection
/// while always honoring the keyboard verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewSource {
    /// Revealed by a stable hover (mouse-move intent delay elapsed).
    Hover,
    /// Revealed by the keyboard preview verb on the focused/top-visible entry.
    Keyboard,
}

/// A live hover/keyboard preview: the previewed target's stable entry id, its
/// semantic kind, a short title, a bounded secret-free body excerpt, the primary
/// activation verb (so the popover can name the double-click/Enter action), and
/// how it was requested. Built fresh each time a preview reveals; the resting
/// state is `None` on the app, so a session that never hovers/previews pays
/// nothing.
#[derive(Debug, Clone)]
pub(crate) struct HoverPreview {
    /// Stable `TranscriptEntry::id` of the previewed unit — the anchor, resolved
    /// to a live entry at activation time so a reflow can't stale it.
    pub(crate) entry_id: u64,
    /// The previewed unit's semantic kind (drives the header noun).
    pub(crate) kind: PreviewKind,
    /// A short, deterministic, secret-free one-line title for the unit.
    pub(crate) title: String,
    /// A bounded body excerpt (already line-split, trimmed, and capped by the
    /// caller). May be empty — the popover then shows the title alone.
    pub(crate) body: Vec<String>,
    /// The primary activation verb (double-click / Enter), if the target has one.
    pub(crate) primary: Option<Action>,
    /// How this preview was requested.
    pub(crate) source: PreviewSource,
}

impl HoverPreview {
    /// Build a preview for a unit. `title` and `body` are already bounded by the
    /// caller; this clamps the body to [`PREVIEW_BODY_LINES`] and each line to
    /// [`PREVIEW_LINE_CAP`] as a defensive backstop so a careless caller can
    /// never blow the popover's fixed size.
    pub(crate) fn new(
        entry_id: u64,
        kind: PreviewKind,
        title: String,
        body: Vec<String>,
        primary: Option<Action>,
        source: PreviewSource,
    ) -> Self {
        let title = clamp_line(&title);
        let body = body
            .into_iter()
            .take(PREVIEW_BODY_LINES)
            .map(|line| clamp_line(&line))
            .filter(|line| !line.is_empty())
            .collect();
        Self {
            entry_id,
            kind,
            title,
            body,
            primary,
            source,
        }
    }

    /// Whether this preview can be activated (has a non-destructive primary verb).
    pub(crate) fn can_activate(&self) -> bool {
        self.primary.is_some()
    }

    /// Whether the keyboard verb (`Alt+1`) pinned this preview. A keyboard-pinned
    /// peek is sticky: an incidental mouse move that lands on empty space must not
    /// dismiss it (only an explicit key/click does), so it does not vanish out from
    /// under a keyboard-only user the instant the pointer drifts.
    pub(crate) fn is_keyboard(&self) -> bool {
        matches!(self.source, PreviewSource::Keyboard)
    }

    /// A short hint line naming the activation verb, for the popover footer —
    /// honest about whether double-click/Enter does anything here.
    pub(crate) fn activate_hint(&self) -> &'static str {
        if self.primary.is_some() {
            "double-click / Enter to open"
        } else {
            "click to select"
        }
    }
}

/// Collapse `text` to a single trimmed line and cap it at [`PREVIEW_LINE_CAP`]
/// characters (appending an ellipsis when truncated). Whitespace-collapsing
/// keeps a multi-space or newline-laden source from blowing the popover width.
pub(crate) fn clamp_line(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_LINE_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(PREVIEW_LINE_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// The fixed-size popover rect for a preview anchored near `anchor_row` inside
/// `area`, sized to fit the title + body + footer. Clamped so it never overflows
/// `area` on any edge — the geometry guarantee behind the spec's "never changes
/// layout height" and "palette fallback on narrow terminals" contracts. Returns
/// `None` when `area` is too small to host even a 1-row popover.
///
/// Placement prefers *below* the anchor row (so the previewed row stays visible
/// above the popover); it flips *above* when there is no room below.
pub(crate) fn popover_rect(area: Rect, anchor_row: u16, body_lines: usize) -> Option<Rect> {
    if area.width < 4 || area.height < 3 {
        return None;
    }
    // Content height: 1 title + body + 1 footer, plus a 2-row border.
    let content_h = 1 + body_lines.min(PREVIEW_BODY_LINES) + 1;
    let height = (content_h as u16 + 2).min(area.height);
    // Width: cap to a quiet, fixed maximum, clamped to the available area.
    let width = (PREVIEW_LINE_CAP as u16 + 4).min(area.width);

    // Prefer below the anchor; flip above when there is no room below.
    let below_y = anchor_row.saturating_add(1);
    let y = if below_y.saturating_add(height) <= area.y.saturating_add(area.height) {
        below_y
    } else {
        // Place above the anchor, clamped to the top of the area.
        anchor_row.saturating_sub(height).max(area.y)
    };
    // Final clamp so the popover always sits fully inside `area`.
    let max_y = area.y.saturating_add(area.height).saturating_sub(height);
    let y = y.clamp(area.y, max_y.max(area.y));

    // Horizontal: left-anchored to the area, clamped so it never runs off-screen.
    let max_x = area.x.saturating_add(area.width).saturating_sub(width);
    let x = area.x.min(max_x.max(area.x));

    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
#[path = "hover_preview_tests.rs"]
mod tests;
