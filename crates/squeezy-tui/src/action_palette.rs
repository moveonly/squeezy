//! Contextual Action Palette (§12.1.2): a compact, context-sensitive action menu
//! for the *currently focused* transcript unit — the focused entry when one is
//! focused (`Ctrl+↑/↓`), else the entry at the top of the viewport (the same
//! reading-position anchor the bookmark / jump-mark / annotate verbs use). It
//! opens over the fullscreen surface, lists only the actions that *apply* to what
//! is under focus (copy, copy code, copy tool output, quote into composer,
//! annotate, open in detail, expand/collapse, related entries, jump to top), and
//! runs the highlighted one with Enter — or a click on its row.
//!
//! **Pure model.** Like the other §12 leaf modules (`change_summary`,
//! `session_timeline`, `turn_outline`), this file owns only the *vocabulary* and
//! the *gathering rule*: which [`PaletteAction`]s apply to a given [`UnitKind`],
//! in a stable order, plus a tiny cursor-bearing [`ActionPalette`] state. It does
//! NOT depend on `lib.rs`'s `TuiApp`; the caller classifies the focused entry
//! into a [`UnitKind`], asks [`applicable_actions`] for the list, and routes each
//! action to the *same* handler its keyboard chord already drives — so keyboard
//! and mouse reach identical behavior by construction and nothing here mutates the
//! transcript.
//!
//! **Stable id anchor.** The palette remembers the focused entry by its stable
//! `TranscriptEntry::id`, never a row offset, so a reflow (resize, streaming,
//! collapse) between open and invoke still resolves to the right entry. An entry
//! that drops out of the transcript while the palette is open simply yields no
//! target and the palette reports an honest no-op.

/// The semantic kind of the focused transcript unit, distilled from
/// `TranscriptEntryKind` to exactly the distinctions the action set cares about.
/// A small, fixed set — the caller maps each transcript entry kind onto one of
/// these so the gathering rule stays pure and table-driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UnitKind {
    /// A user prompt message.
    UserMessage,
    /// An assistant message (prose; may carry fenced code).
    AssistantMessage,
    /// A finalized reasoning segment.
    Reasoning,
    /// A tool result / command run (carries tool output).
    ToolResult,
    /// A plan checkpoint card.
    PlanCard,
    /// A `/diff` snapshot card.
    Diff,
    /// An operational log / note / subagent breadcrumb / slash echo.
    Note,
}

impl UnitKind {
    /// A short, screen-reader-friendly noun for the focused unit, painted in the
    /// palette header so the menu says *what* it is acting on. ASCII only (no
    /// glyphs) so the meaning never depends on color or a private-use codepoint.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            UnitKind::UserMessage => "your message",
            UnitKind::AssistantMessage => "assistant message",
            UnitKind::Reasoning => "reasoning",
            UnitKind::ToolResult => "tool result",
            UnitKind::PlanCard => "plan",
            UnitKind::Diff => "diff",
            UnitKind::Note => "note",
        }
    }

    /// Whether this unit can carry fenced code worth a dedicated "copy code"
    /// verb: prose-bearing messages (user or assistant — a user can paste a fenced
    /// snippet), reasoning, and tool output. Plans, diffs (already a structured
    /// diff projection), and one-line notes never do. When the entry turns out to
    /// hold no fenced block, the underlying code-copy verb reports an honest
    /// "nothing to copy", so offering it here is safe.
    fn may_carry_code(self) -> bool {
        matches!(
            self,
            UnitKind::UserMessage
                | UnitKind::AssistantMessage
                | UnitKind::Reasoning
                | UnitKind::ToolResult
        )
    }

    /// Whether this unit is prose worth quoting into the composer: a message or a
    /// reasoning segment. Tool output, plans, diffs, and notes are not "quotable
    /// prose" — quoting them would drop a wall of structured output into the
    /// composer, which is the spec's accidental-mutation risk in disguise.
    fn is_quotable_prose(self) -> bool {
        matches!(
            self,
            UnitKind::UserMessage | UnitKind::AssistantMessage | UnitKind::Reasoning
        )
    }
}

/// A contextual action offered for the focused unit. Each variant maps 1:1 to an
/// existing, already-tested handler the keyboard already reaches, so invoking it
/// from the palette is the same behavior as the chord — never a new mutation.
/// Ordered so [`PaletteAction::ALL`] reads top-to-bottom the way a menu flows
/// (the copies first, then quote, then annotate, then the navigation verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PaletteAction {
    /// Copy the whole focused entry (`Alt+c` twin).
    CopyEntry,
    /// Copy every fenced code block of the focused entry (`Alt+j` twin).
    CopyCode,
    /// Copy the focused tool result's output (`Alt+o` twin).
    CopyToolOutput,
    /// Quote the focused prose into the composer.
    QuoteToCompose,
    /// Annotate the focused entry (`Alt+/` twin).
    Annotate,
    /// Open the focused entry in the Ctrl+T detail overlay (`Ctrl+Enter` twin).
    OpenInDetail,
    /// Expand the focused entry if collapsed, else collapse it.
    ToggleFold,
    /// Open the Related-Entry Links overlay for the focused entry (`Alt+g` twin).
    RelatedLinks,
    /// Jump the main view so the focused entry sits at the top.
    JumpToEntry,
}

impl PaletteAction {
    /// Every action variant, in stable menu order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in any palette and the
    /// round-trip / coverage tests fail.
    pub(crate) const ALL: &'static [PaletteAction] = &[
        PaletteAction::CopyEntry,
        PaletteAction::CopyCode,
        PaletteAction::CopyToolOutput,
        PaletteAction::QuoteToCompose,
        PaletteAction::Annotate,
        PaletteAction::OpenInDetail,
        PaletteAction::ToggleFold,
        PaletteAction::RelatedLinks,
        PaletteAction::JumpToEntry,
    ];

    /// Short, screen-reader-friendly label for the action row. ASCII only so the
    /// menu carries meaning without relying on color or a glyph. `collapsed`
    /// flips the fold verb's label so the row reads honestly for the current
    /// state ("expand entry" vs "collapse entry").
    pub(crate) fn label(self, collapsed: bool) -> &'static str {
        match self {
            PaletteAction::CopyEntry => "copy entry",
            PaletteAction::CopyCode => "copy code",
            PaletteAction::CopyToolOutput => "copy tool output",
            PaletteAction::QuoteToCompose => "quote into composer",
            PaletteAction::Annotate => "annotate entry",
            PaletteAction::OpenInDetail => "open in detail",
            PaletteAction::ToggleFold => {
                if collapsed {
                    "expand entry"
                } else {
                    "collapse entry"
                }
            }
            PaletteAction::RelatedLinks => "related entries",
            PaletteAction::JumpToEntry => "jump to this entry",
        }
    }
}

/// The actions that apply to a focused unit of `kind`, in [`PaletteAction::ALL`]
/// order. Pure and total over the kind set:
///
/// - **copy entry**, **annotate**, **toggle fold**, **related entries**, and
///   **jump** apply to every unit (every entry has text, can be annotated,
///   folded, related to others, and jumped to).
/// - **copy code** appears only for prose/tool units that can carry fenced code.
/// - **copy tool output** appears only for a tool result.
/// - **quote into composer** appears only for quotable prose (a message or
///   reasoning segment).
/// - **open in detail** appears only when `has_detail` — the caller's report of
///   whether the entry carries diff/excerpt/bulky output worth the detail pane —
///   so the menu never offers a detail view that would open empty.
pub(crate) fn applicable_actions(kind: UnitKind, has_detail: bool) -> Vec<PaletteAction> {
    PaletteAction::ALL
        .iter()
        .copied()
        .filter(|action| match action {
            PaletteAction::CopyEntry
            | PaletteAction::Annotate
            | PaletteAction::ToggleFold
            | PaletteAction::RelatedLinks
            | PaletteAction::JumpToEntry => true,
            PaletteAction::CopyCode => kind.may_carry_code(),
            PaletteAction::CopyToolOutput => kind == UnitKind::ToolResult,
            PaletteAction::QuoteToCompose => kind.is_quotable_prose(),
            PaletteAction::OpenInDetail => has_detail,
        })
        .collect()
}

/// The open Contextual Action Palette (§12.1.2): the focused entry's stable id,
/// its kind / collapsed state (so the header and the fold-verb label read
/// honestly), the gathered action list, and the cursor into it. Built fresh each
/// time the palette opens via [`ActionPalette::open`]; the resting state is
/// `None` on the app (the palette closed), so a session that never opens it costs
/// nothing.
#[derive(Debug, Clone)]
pub(crate) struct ActionPalette {
    /// Stable `TranscriptEntry::id` of the focused unit. The action target;
    /// resolved to a live entry at invoke time so a reflow can't stale it.
    pub(crate) entry_id: u64,
    /// The focused unit's kind (drives the header noun).
    pub(crate) kind: UnitKind,
    /// Whether the focused entry is currently collapsed (drives the fold label).
    pub(crate) collapsed: bool,
    /// A short, deterministic, secret-free one-line label for the focused entry
    /// (its first content line / tool name), painted in the header so the menu
    /// shows *which* entry it acts on. Bounded by the caller.
    pub(crate) title: String,
    /// The gathered actions, in menu order.
    actions: Vec<PaletteAction>,
    /// Cursor into `actions`. Clamped to the action count on every move.
    selected: usize,
}

impl ActionPalette {
    /// Open a palette for a focused unit. `actions` is the already-gathered
    /// applicable list (always non-empty in practice — every unit has at least
    /// copy/annotate/fold/related/jump). The cursor parks on the first action.
    pub(crate) fn open(
        entry_id: u64,
        kind: UnitKind,
        collapsed: bool,
        title: String,
        actions: Vec<PaletteAction>,
    ) -> Self {
        Self {
            entry_id,
            kind,
            collapsed,
            title,
            actions,
            selected: 0,
        }
    }

    /// The gathered actions in menu order.
    pub(crate) fn actions(&self) -> &[PaletteAction] {
        &self.actions
    }

    /// Number of actions in the menu.
    pub(crate) fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether the menu is empty (no applicable action). Never true in practice
    /// — every unit offers at least the always-available verbs — but the render
    /// path guards on it so a degenerate gather paints an honest empty state
    /// rather than panicking.
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The current cursor index, clamped to the last action.
    pub(crate) fn selected(&self) -> usize {
        self.selected.min(self.actions.len().saturating_sub(1))
    }

    /// The action under the cursor, or `None` when the menu is empty.
    pub(crate) fn selected_action(&self) -> Option<PaletteAction> {
        self.actions.get(self.selected()).copied()
    }

    /// The action at flattened index `index`, or `None` when out of range. The
    /// mouse path selects by index and runs the cursor's action (not this), so this
    /// is a test/diagnostic accessor — `cfg(test)` to stay lint-clean on every
    /// platform.
    #[cfg(test)]
    pub(crate) fn action_at(&self, index: usize) -> Option<PaletteAction> {
        self.actions.get(index).copied()
    }

    /// Move the cursor to `index`, clamped to the action range. The mouse click
    /// path uses this so a click lands and stays on exactly the clicked row.
    pub(crate) fn select(&mut self, index: usize) {
        if self.actions.is_empty() {
            self.selected = 0;
        } else {
            self.selected = index.min(self.actions.len() - 1);
        }
    }

    /// Move the cursor up one (saturating at the top).
    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected().saturating_sub(1);
    }

    /// Move the cursor down one (clamped to the last action).
    pub(crate) fn move_down(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected() + 1).min(self.actions.len() - 1);
        }
    }
}

#[cfg(test)]
#[path = "action_palette_tests.rs"]
mod tests;
