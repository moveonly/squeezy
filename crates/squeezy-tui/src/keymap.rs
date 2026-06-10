//! TUI key rebinding: maps user-supplied key specs in `[tui.keymap]`
//! to a small set of named actions and resolves them at runtime.
//!
//! The audit (`tui-003`) flagged the hardcoded `Ctrl+T` / `Ctrl+P` /
//! `Ctrl+Y` / `PageUp` / etc. bindings as unaccessible to users who
//! collide with their host terminal (tmux Ctrl+T) or use non-QWERTY
//! layouts. The substrate here lets the user write
//!
//! ```toml
//! [tui.keymap]
//! transcript_overlay = "Ctrl+o"
//! page_up = "Alt+k"
//! ```
//!
//! and have those override the compiled-in defaults. `/keymap` lists
//! the current resolution so the user can verify what's bound.
//!
//! Scope is deliberately narrow: only the auxiliary actions (scroll,
//! overlay, copy-last, restore-prompt, …) are rebindable. Composer
//! basics (Enter, Esc, Backspace, character input) stay hardcoded
//! because rebinding them breaks every workflow.
//!
//! Unknown action slugs or unparseable specs are kept and surfaced
//! via `/keymap` so the user sees the validation problem instead of
//! a silent miss.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyModifiers};

/// A named action a user can rebind. The slug used in
/// `[tui.keymap]` matches `Action::slug()` exactly so the config-file
/// surface stays stable as variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Action {
    /// Open / close the full-screen config browser (`F11` default).
    ToggleConfigScreen,
    /// Open / close the transcript overlay (`Ctrl+T` default).
    ToggleTranscriptOverlay,
    /// Open incremental transcript search (`/` default). Searches the active
    /// surface (main view, or the Ctrl+T overlay when it is open).
    OpenSearch,
    /// Expand or collapse the live task panel (`Ctrl+P` default).
    ToggleTaskPanel,
    /// Copy the last assistant response to the system clipboard
    /// (`Ctrl+Y` default).
    CopyLastAssistant,
    /// Copy the current/focused transcript entry to the clipboard
    /// (`Alt+c` default).
    CopyFocusedEntry,
    /// Copy the current/nearest tool output to the clipboard
    /// (`Alt+o` default).
    CopyCurrentToolOutput,
    /// Copy the fenced code block under the cursor to the clipboard
    /// (`Alt+k` default).
    CopyCodeBlock,
    /// Code-Aware Copy/Export (§12.5.5): copy EVERY fenced code block of the
    /// focused entry (or, when it holds none, the whole transcript) to the
    /// clipboard — languages preserved, UI rails stripped (`Alt+j` default).
    CopyAllCode,
    /// Copy the rows visible in the main viewport to the clipboard
    /// (`Alt+v` default).
    CopyViewport,
    /// Copy the entire transcript to the clipboard (`Alt+a` default).
    CopyFullTranscript,
    /// Copy the active visual selection to the clipboard (`Alt+y`
    /// default). Convenience only — every copy chord already prefers an
    /// active selection when one is present; this is the explicit verb.
    CopySelection,
    /// Quote the active visual selection into the composer as a Markdown
    /// blockquote (`>` default; §11.1 quote-to-compose). Only fires while a
    /// main-view selection is active; otherwise the `>` keystroke falls
    /// through to normal composer input.
    QuoteSelectionToCompose,
    /// Multi-Cursor-Like Transcript Selection (§12.1.6): commit the live visual
    /// selection into the disjoint selection set so the next gesture starts a
    /// fresh non-contiguous range (`Alt+d` default). With no live selection it
    /// is a no-op and falls through.
    AddSelectionToSet,
    /// Multi-Cursor-Like Transcript Selection (§12.1.6): copy EVERY committed
    /// disjoint range plus the live one as one combined payload (`Ctrl+Alt+Y`
    /// default), distinct blocks separated by a blank line.
    CopyMultiSelection,
    /// Prompt Snippets From Selection (§12.3.2): save the active main-view visual
    /// selection as a reusable named prompt snippet (`Alt+3` default). With no
    /// live selection it is a no-op and falls through so the key keeps its normal
    /// meaning.
    SaveSnippetFromSelection,
    /// Prompt Snippets From Selection (§12.3.2): toggle the saved-snippets picker
    /// overlay (`Ctrl+Alt+S` default), from which a snippet inserts into the
    /// composer or is staged onto the prompt queue.
    ToggleSnippets,
    /// Restore the most recently cancelled prompt back into the
    /// composer (`Ctrl+R` default).
    RestoreCancelledPrompt,
    /// Scroll the transcript one page up (`PageUp` default).
    ScrollTranscriptPageUp,
    /// Scroll the transcript one page down (`PageDown` default).
    ScrollTranscriptPageDown,
    /// Jump to the top of the transcript when the composer is empty
    /// (`Home` default; falls through to line-start otherwise).
    TranscriptHome,
    /// Jump to the bottom of the transcript when the composer is
    /// empty (`End` default; falls through to line-end otherwise).
    TranscriptEnd,
    /// Jump the transcript to the previous user turn (`Alt+Up` default).
    JumpPrevUserTurn,
    /// Jump the transcript to the next user turn (`Alt+Down` default).
    JumpNextUserTurn,
    /// Jump the transcript to the previous assistant answer (`Alt+Left`
    /// default).
    JumpPrevAssistant,
    /// Jump the transcript to the next assistant answer (`Alt+Right`
    /// default).
    JumpNextAssistant,
    /// Jump the transcript to the previous tool call (`Alt+,` default).
    JumpPrevToolCall,
    /// Jump the transcript to the next tool call (`Alt+.` default).
    JumpNextToolCall,
    /// Jump the transcript to the previous error (`Alt+[` default).
    JumpPrevError,
    /// Jump the transcript to the next error (`Alt+]` default).
    JumpNextError,
    /// Move the focused-entry cursor to the previous transcript entry
    /// (`Ctrl+Up` default). Used by the per-entry fold controls.
    FocusPrevEntry,
    /// Move the focused-entry cursor to the next transcript entry
    /// (`Ctrl+Down` default).
    FocusNextEntry,
    /// Toggle the collapsed state of the focused transcript entry in the
    /// main inline view (`Ctrl+O` default). Paired with the mouse caret click,
    /// which dispatches the same fold toggle.
    ToggleFocusedFold,
    /// Open the focused transcript entry in the Ctrl+T detail overlay
    /// (`Ctrl+Enter` default). Paired with the mouse "open in detail"
    /// affordance; both drive `open_focused_entry_in_detail`.
    OpenFocusedInDetail,
    /// Undo the most recent prompt-queue mutation — delete or reorder
    /// (`u` default). The keyboard twin of the mouse undo affordance. Only
    /// fires while the queue reorder overlay is open; outside it the key
    /// falls through so `u` keeps its normal composer meaning. The other
    /// queue verbs (focus move, item reorder, delete) are consumed inside the
    /// overlay's own modal key handler via `PromptQueueState::dispatch`
    /// (Up/Down, Shift+Up/Down, Delete), keeping the overlay's
    /// before-the-global-keymap consumption pattern; undo is the one genuinely
    /// new verb, so it earns a rebindable action.
    QueueUndo,
    /// Toggle the hidden per-interaction UX latency-budget overlay
    /// (`Ctrl+Alt+L` default; §12.10.1). A deliberately obscure debug chord —
    /// it forces the render-metrics HUD visible and adds a p95/p99-vs-budget
    /// panel for keypress echo, scroll, page jumps, queue drag, paste preview,
    /// copy ack, search jump, and resize redraw. Off in a normal session.
    ToggleLatencyOverlay,
    /// Toggle the hidden dogfood-telemetry `/metrics` snapshot overlay
    /// (`Ctrl+Alt+M` default; §12.10.3). Like the latency overlay, a
    /// deliberately obscure debug chord: it forces the render-metrics HUD
    /// visible and adds a session-long counter snapshot (frames/bytes/cache/
    /// input/storms/copy/terminal-profile/a11y/teardown). Off by default.
    ToggleDogfoodMetrics,
    /// Set a jump mark at the entry currently at the top of the viewport
    /// (`Alt+m` default; §11.2 / 11G.2). Marks are stored by stable entry id,
    /// so they survive a transcript reflow. Paired with `JumpToMark`.
    SetJumpMark,
    /// Jump back to the most recently set jump mark, popping it off the mark
    /// stack (`Alt+'` default; §11.2 / 11G.2). With no marks set, falls back to
    /// showing the recent jump history in the status line.
    JumpToMark,
    /// Toggle the minimap turn rail (`Alt+r` default; §11.2 / 11G.3). A compact
    /// vertical rail in the main view showing user turns, tool calls, errors,
    /// and the current viewport band; clickable to jump when mouse capture is
    /// on. Off by default so an idle session paints nothing extra.
    ToggleMinimap,
    /// Toggle the main view between soft-wrap (every line reflows to the column)
    /// and no-wrap horizontal-scroll (`Alt+w` default; §11.2 / 11G.4). No-wrap
    /// lets wide code/diff blocks and long command output pan left/right instead
    /// of wrapping or being hidden. Paired with `ScrollBlockLeft`/`Right`.
    ToggleSoftWrap,
    /// Pan the no-wrap main view left one step (`Alt+h` default; §11G.4). The
    /// keyboard twin of Shift+wheel-up. A no-op while soft-wrap is on.
    ScrollBlockLeft,
    /// Pan the no-wrap main view right one step (`Alt+l` default; §11G.4). The
    /// keyboard twin of Shift+wheel-down. A no-op while soft-wrap is on.
    ScrollBlockRight,
    /// Cycle the OSC 8 hyperlink mode for rendered URLs/file paths (`Alt+8`
    /// default; §11.5 / 11G.5). Rotates auto (the startup terminal probe) → on
    /// (force click-to-open escapes) → off (force plain text), so a user whose
    /// terminal was mis-detected either way can correct it without restarting.
    ToggleHyperlinks,
    /// Open / close the in-app clipboard-history picker (`Alt+p` default;
    /// §12.6.1). A bounded ring of Squeezy's own recent copies — never the OS
    /// clipboard — that the user can re-copy, pin, delete, or clear from a
    /// fullscreen overlay. Records every copy through the same provider chain and
    /// records nothing at idle, so an unopened picker costs zero.
    ToggleClipboardHistory,
    /// Build a shareable session bundle with the defaults (`Alt+b` default;
    /// §12.6.6) — the keyboard twin of `/bundle`. Renders the transcript through
    /// the export pipeline, assembles a self-contained Markdown artifact
    /// (transcript + manifest + checksum + diagnostics, redacted), writes it
    /// atomically under session storage, and echoes a preview into the transcript
    /// for review before sharing. A one-shot action — it paints nothing at idle.
    BuildSessionBundle,
    /// Open the composer text in the user's `$VISUAL`/`$EDITOR` (`Alt+e` default;
    /// §12.6.5 External Editor Handoff). Suspends the alt-screen, hands a temp
    /// file to the editor, and re-imports the saved buffer through an
    /// accept/reopen/discard confirmation. A safe no-op (status hint) when no
    /// editor is configured, and degrades to the same hint off Unix where the
    /// spawn/terminal-restore plumbing is not wired. Records nothing at idle.
    OpenComposerInEditor,
    /// Cycle the main-view Semantic Filter (§12.5.2) forward through its
    /// categories — all → user turns → assistant → tool calls → errors → (per
    /// tool, when more than one) → all (`Alt+f` default). Narrows the inline
    /// transcript to one semantic category in place, the complement of the Ctrl+T
    /// overlay's local `f` filter. Paired with a click on the active-filter badge,
    /// which dispatches the same forward cycle. A main-surface action; off (`All`)
    /// by default so an idle session paints nothing extra.
    CycleSemanticFilter,
    /// Open / close the Local Transcript Index overlay (`Alt+i` default;
    /// §12.5.1). A fullscreen summary of the in-memory transcript index — entry
    /// counts by category (user turns, tool calls, errors, reasoning, subagents,
    /// …) with keyboard/mouse navigation that jumps the main view to the next
    /// entry in the selected category. The index rebuilds incrementally only on a
    /// transcript revision bump, so an idle session pays nothing.
    ToggleTranscriptIndex,
    /// Open / close the Related-Entry Links overlay (`Alt+g` default; §12.5.3).
    /// Surfaces the links between the focused (or latest) transcript entry and
    /// the entries it relates to — its prompt/reply, the tool calls it triggered,
    /// the error it caused, the follow-up that fixed it, same-tool calls, and
    /// subagent breadcrumbs — with keyboard/mouse navigation that jumps the main
    /// view to the selected related entry. The relation graph rebuilds
    /// incrementally only on a transcript revision bump, so an idle session pays
    /// nothing.
    ToggleRelatedLinks,
    /// Open / close the Duplicate-Output Folds overlay (`Alt+u` default;
    /// §12.5.4). A fullscreen list of detected runs of repeated / near-duplicate
    /// tool outputs, each collapsed to its first member with a count; the raw
    /// content of every folded output is retained for expand, search, and copy.
    /// Keyboard/mouse navigation jumps the main view to the next fold lead and
    /// toggles a span open. The fold model rebuilds incrementally only on a
    /// transcript revision bump, so an idle session pays nothing.
    ToggleDuplicateFolds,
    /// Open / close the Error Lenses overlay (`Alt+x` default; §12.5.6). A
    /// fullscreen list of the actionable error lines detected inside failed tool
    /// outputs — each classified (rustc / cargo / test / permission / network /
    /// panic / sandbox), carrying its message and any extracted `file:line`
    /// location — with keyboard/mouse navigation that jumps the main view to the
    /// failing entry. The lens model rebuilds incrementally only on a transcript
    /// revision bump, so an idle session pays nothing.
    ToggleErrorLens,
    /// Open / close the Transcript Health Markers overlay (`Alt+n` default;
    /// §12.5.7). A fullscreen list of the health/status markers detected on the
    /// transcript — a failed tool, a failed subagent, a failed turn, output
    /// elided to a preview, or a large output blob — each carrying its short
    /// message and severity, with keyboard/mouse navigation that jumps the main
    /// view to the marked entry so the user can see what was hidden. The marker
    /// model rebuilds incrementally only on a transcript revision bump, so an
    /// idle session pays nothing.
    ToggleHealthMarkers,
    /// Open / close the Semantic Turn Outline overlay (`Alt+s` default; §12.2.1).
    /// A navigable structural map of the session — user prompts, assistant
    /// answers, tool runs, errors, reasoning, plans, diffs, and subagent
    /// breadcrumbs — each with a short deterministic title; keyboard/mouse
    /// navigation jumps the main view to the logical transcript row of the
    /// selected node. The outline rebuilds incrementally only on a transcript
    /// revision bump, so an idle session pays nothing.
    ToggleTurnOutline,
    /// Open / close the Collapsible Reasoning/Tool Lanes overlay (`Alt+z`
    /// default; §12.2.2). A per-entry panel that splits the focused transcript
    /// entry into foldable lanes — reasoning, assistant text, tool input, tool
    /// output, system notice, approval, error, and plan — each with a collapse
    /// toggle so whole lanes can be folded away to read the transcript at a
    /// higher altitude. Collapse state is persisted by `(entry_id, lane_id)` in
    /// app state, so a folded lane survives every redraw and resize; an errored
    /// lane always keeps its visible header. The panel rebuilds incrementally
    /// only on a focused-entry / revision / collapse change, so an idle session
    /// pays nothing.
    ToggleLaneFold,
    /// Open / close the Pinned Compare View (`Alt+t` default; §12.2.3). Pins the
    /// focused transcript entry into one pane and shows it side-by-side (or
    /// stacked, on a narrow terminal) against the live transcript — or a second
    /// pinned entry — so old and new content can be compared. Each pane keeps its
    /// own scroll; `Tab` (or a click) flips which pane the keyboard/wheel drives;
    /// `x` toggles a line-based clean-text diff. The view lives inside the Ctrl+T
    /// overlay (it compares against what the overlay shows) and paints nothing at
    /// idle. Reuses the §11G.10 detail-pane split machinery.
    TogglePinnedCompare,
    /// Drop a Reading Position Bookmark at the entry currently at the top of the
    /// viewport (`Alt+;` default; §12.2.4). A bookmark is a durable
    /// reading-position anchor (distinct from a transient jump mark): it is keyed
    /// by the stable transcript entry id, so it survives appends, resize, folds,
    /// and filters until the user deletes it. The first drop with no name is an
    /// anonymous bookmark; the bookmark list overlay can name/rename it later.
    /// Costs nothing until pressed.
    DropBookmark,
    /// Open / close the Reading Position Bookmarks overlay (`Alt+q` default;
    /// §12.2.4). A list of every bookmark in transcript-reading order; the cursor
    /// (↑↓/kj, plus n/p for next/previous) selects one and Enter jumps the main
    /// view to its anchored entry. `r` renames, `d`/Delete deletes, Esc/Alt+q
    /// closes. The list lives in app state (not recomputed from cells) so it is
    /// stable across redraws and resize; an idle session that never opens it pays
    /// nothing.
    ToggleBookmarks,
    /// Open / close the Session Timeline overlay (`Alt+9` default; §12.2.6). A
    /// compact chronological event view of the session — prompts, turns, tool
    /// runs, approvals, edits, errors, and other high-signal state changes —
    /// rendered as a rail/list and grouped by turn, each with a short
    /// deterministic label and an ok/failed/pending status. The cursor (↑↓/kj,
    /// plus Enter/→/l to jump) scrolls the main view to the transcript row the
    /// selected event stands for; `f` cycles a per-kind filter. The timeline
    /// rebuilds incrementally only on a transcript revision bump, so an idle
    /// session pays nothing.
    ToggleSessionTimeline,
    /// Annotate the focused (or top-visible) transcript entry with a short private
    /// note (`Alt+/` default; §12.2.5). The note is attached to the stable
    /// transcript entry id and stored only in session UI metadata, never in the
    /// model transcript, so it never enters model context. A small inline marker
    /// appears on the annotated entry's row; the note is editable in the
    /// annotations overlay's composer. Costs nothing until pressed.
    AnnotateEntry,
    /// Open / close the Entry Annotations overlay (`Alt+\` default; §12.2.5). A
    /// list of every annotation in transcript-reading order; the cursor (↑↓/kj,
    /// plus n/p for next/previous) selects one and Enter jumps the main view to its
    /// anchored entry. `e` edits the note in the composer, `d`/Delete deletes,
    /// Esc/Alt+\ closes. The list lives in app state (not recomputed from cells) so
    /// it is stable across redraws and resize; an idle session that never opens it
    /// pays nothing.
    ToggleAnnotations,
    /// Mark a "What Changed Since Here?" point and open its delta overlay (`Alt+0`
    /// default; §12.2.7). The anchor is the focused (or top-visible) transcript
    /// entry's stable id; opening the overlay scans every later transcript event
    /// and surfaces the changes observed since — file edits, commands/tests,
    /// errors, checkpoints, approval decisions, and other tool results — as a
    /// grouped, summarized delta. The cursor (↑↓/kj, plus n/p for next/previous)
    /// selects a change and Enter/→/l jumps the main view to the entry it stands
    /// for; `m` re-marks the anchor at the current reading position. The delta
    /// rebuilds incrementally only on an anchor move or a transcript revision bump,
    /// so an idle session pays nothing. Uses honest "observed since" language — it
    /// reports only what this session's transcript recorded, never a full project
    /// history.
    ToggleChangesSince,
    /// Open the Contextual Action Palette (`Alt+Enter` default; §12.1.2) for the
    /// currently focused transcript unit — the focused entry (`Ctrl+↑/↓`) when one
    /// is focused, else the top-visible entry. The palette lists only the actions
    /// that apply to what is under focus (copy, copy code, copy tool output, quote
    /// into composer, annotate, open in detail, expand/collapse, related entries,
    /// jump) and runs the highlighted one with Enter — or a click on its row. Each
    /// action routes to the same handler its own chord already drives, so the menu
    /// never introduces new behavior. Closed is the resting state, so a session
    /// that never opens it pays nothing.
    OpenActionPalette,
    /// Open / close the Universal Command Palette (`Ctrl+Alt+P` default; §12.1.1).
    /// One discoverable, fuzzy-searchable modal that lists every app command — the
    /// rebindable keymap actions plus the slash-command help table — with the
    /// current binding and a short description, and runs the highlighted command
    /// with Enter (keyboard) or a click (mouse). Slash commands that take a
    /// parameter are handed back to the composer as a second step. The palette is
    /// built only on open, so an unopened session pays nothing.
    ToggleCommandPalette,
    /// Open / close the Hover Preview popover (`Alt+1` default; §12.1.4) for the
    /// currently focused transcript unit — the focused entry (`Ctrl+↑/↓`) when one
    /// is focused, else the top-visible entry. The keyboard twin of a stable mouse
    /// hover: it shows a quiet, noncommittal preview (the entry's title + a bounded
    /// excerpt) without stealing focus or changing layout. The same focused entry's
    /// primary action (open in detail) stays on its own `Ctrl+Enter` chord — this
    /// verb only previews. Closed is the resting state, so a session that never
    /// opens it pays nothing.
    ToggleHoverPreview,
    /// Toggle Mouse Hover Intent (`Ctrl+Alt+H` default; §12.1.3). When on (the
    /// default), the transcript card under the pointer — or, when the terminal
    /// reports no mouse motion, the keyboard-focused card — gains a restrained,
    /// debounced emphasis (brighter, bolded header hint) without changing row
    /// heights. A wheel scroll, drag, or active selection suppresses the reveal.
    /// The verb flips the affordance off for users who prefer none; the resting
    /// state paints nothing and schedules no redraw, so it costs nothing idle.
    ToggleHoverIntent,
    /// Show / hide the Clickable Breadcrumbs strip (`Alt+2` default; §12.1.5). A
    /// compact `session ▸ turn ▸ entry` trail (with an `overlay`/`search` suffix)
    /// that orients long sessions. While shown it is keyboard-focusable —
    /// Left/Right move the focused crumb, Enter jumps to it — and each crumb is a
    /// click target; while hidden it paints nothing and schedules no redraw, so it
    /// costs nothing idle.
    ToggleBreadcrumbs,
    /// Rename / label the focused (or top-visible) transcript entry inline
    /// (`Ctrl+Alt+R` default; §12.1.7). Opens a small in-place editor seeded with the
    /// entry's current label (empty for a fresh one); typing edits it, Enter saves,
    /// Esc cancels, a blank save clears it. The label is UI-only metadata that
    /// paints as a small badge on the row and never enters the model transcript.
    /// The resting state stores nothing and paints nothing, so it costs nothing
    /// idle.
    RenameFocusedEntry,
    /// Dismiss the gentle First-Run Interaction Hint currently shown (`Ctrl+Alt+N`
    /// default; §12.1.8). The keyboard twin of clicking the dim hint strip: it
    /// retires the visible hint (latched seen for the session) so it never returns.
    /// When no hint is showing it is a no-op that falls through, so it never steals
    /// a key from the composer or transcript. Once every hint is seen the feature is
    /// quiet and this verb does nothing, costing nothing idle.
    DismissFirstRunHint,
}

impl Action {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::ToggleConfigScreen => "toggle_config_screen",
            Self::ToggleTranscriptOverlay => "transcript_overlay",
            Self::OpenSearch => "open_search",
            Self::ToggleTaskPanel => "toggle_task_panel",
            Self::CopyLastAssistant => "copy_last_assistant",
            Self::CopyFocusedEntry => "copy_focused_entry",
            Self::CopyCurrentToolOutput => "copy_tool_output",
            Self::CopyCodeBlock => "copy_code_block",
            Self::CopyAllCode => "copy_all_code",
            Self::CopyViewport => "copy_viewport",
            Self::CopyFullTranscript => "copy_full_transcript",
            Self::CopySelection => "copy_selection",
            Self::QuoteSelectionToCompose => "quote_selection_to_compose",
            Self::AddSelectionToSet => "add_selection_to_set",
            Self::CopyMultiSelection => "copy_multi_selection",
            Self::SaveSnippetFromSelection => "save_snippet_from_selection",
            Self::ToggleSnippets => "toggle_snippets",
            Self::RestoreCancelledPrompt => "restore_cancelled_prompt",
            Self::ScrollTranscriptPageUp => "page_up",
            Self::ScrollTranscriptPageDown => "page_down",
            Self::TranscriptHome => "transcript_home",
            Self::TranscriptEnd => "transcript_end",
            Self::JumpPrevUserTurn => "jump_prev_user_turn",
            Self::JumpNextUserTurn => "jump_next_user_turn",
            Self::JumpPrevAssistant => "jump_prev_assistant",
            Self::JumpNextAssistant => "jump_next_assistant",
            Self::JumpPrevToolCall => "jump_prev_tool_call",
            Self::JumpNextToolCall => "jump_next_tool_call",
            Self::JumpPrevError => "jump_prev_error",
            Self::JumpNextError => "jump_next_error",
            Self::FocusPrevEntry => "focus_prev_entry",
            Self::FocusNextEntry => "focus_next_entry",
            Self::ToggleFocusedFold => "toggle_focused_fold",
            Self::OpenFocusedInDetail => "open_focused_in_detail",
            Self::QueueUndo => "queue_undo",
            Self::ToggleLatencyOverlay => "toggle_latency_overlay",
            Self::ToggleDogfoodMetrics => "toggle_dogfood_metrics",
            Self::SetJumpMark => "set_jump_mark",
            Self::JumpToMark => "jump_to_mark",
            Self::ToggleMinimap => "toggle_minimap",
            Self::ToggleSoftWrap => "toggle_soft_wrap",
            Self::ScrollBlockLeft => "scroll_block_left",
            Self::ScrollBlockRight => "scroll_block_right",
            Self::ToggleHyperlinks => "toggle_hyperlinks",
            Self::ToggleClipboardHistory => "toggle_clipboard_history",
            Self::BuildSessionBundle => "build_session_bundle",
            Self::OpenComposerInEditor => "open_composer_in_editor",
            Self::CycleSemanticFilter => "cycle_semantic_filter",
            Self::ToggleTranscriptIndex => "toggle_transcript_index",
            Self::ToggleRelatedLinks => "toggle_related_links",
            Self::ToggleDuplicateFolds => "toggle_duplicate_folds",
            Self::ToggleErrorLens => "toggle_error_lens",
            Self::ToggleHealthMarkers => "toggle_health_markers",
            Self::ToggleTurnOutline => "toggle_turn_outline",
            Self::ToggleLaneFold => "toggle_lane_fold",
            Self::TogglePinnedCompare => "toggle_pinned_compare",
            Self::DropBookmark => "drop_bookmark",
            Self::ToggleBookmarks => "toggle_bookmarks",
            Self::ToggleSessionTimeline => "toggle_session_timeline",
            Self::AnnotateEntry => "annotate_entry",
            Self::ToggleAnnotations => "toggle_annotations",
            Self::ToggleChangesSince => "toggle_changes_since",
            Self::OpenActionPalette => "open_action_palette",
            Self::ToggleCommandPalette => "toggle_command_palette",
            Self::ToggleHoverPreview => "toggle_hover_preview",
            Self::ToggleHoverIntent => "toggle_hover_intent",
            Self::ToggleBreadcrumbs => "toggle_breadcrumbs",
            Self::RenameFocusedEntry => "rename_focused_entry",
            Self::DismissFirstRunHint => "dismiss_first_run_hint",
        }
    }

    pub(crate) const ALL: &'static [Action] = &[
        Action::ToggleConfigScreen,
        Action::ToggleTranscriptOverlay,
        Action::OpenSearch,
        Action::ToggleTaskPanel,
        Action::CopyLastAssistant,
        Action::CopyFocusedEntry,
        Action::CopyCurrentToolOutput,
        Action::CopyCodeBlock,
        Action::CopyAllCode,
        Action::CopyViewport,
        Action::CopyFullTranscript,
        Action::CopySelection,
        Action::QuoteSelectionToCompose,
        Action::AddSelectionToSet,
        Action::CopyMultiSelection,
        Action::SaveSnippetFromSelection,
        Action::ToggleSnippets,
        Action::RestoreCancelledPrompt,
        Action::ScrollTranscriptPageUp,
        Action::ScrollTranscriptPageDown,
        Action::TranscriptHome,
        Action::TranscriptEnd,
        Action::JumpPrevUserTurn,
        Action::JumpNextUserTurn,
        Action::JumpPrevAssistant,
        Action::JumpNextAssistant,
        Action::JumpPrevToolCall,
        Action::JumpNextToolCall,
        Action::JumpPrevError,
        Action::JumpNextError,
        Action::FocusPrevEntry,
        Action::FocusNextEntry,
        Action::ToggleFocusedFold,
        Action::OpenFocusedInDetail,
        Action::QueueUndo,
        Action::ToggleLatencyOverlay,
        Action::ToggleDogfoodMetrics,
        Action::SetJumpMark,
        Action::JumpToMark,
        Action::ToggleMinimap,
        Action::ToggleSoftWrap,
        Action::ScrollBlockLeft,
        Action::ScrollBlockRight,
        Action::ToggleHyperlinks,
        Action::ToggleClipboardHistory,
        Action::BuildSessionBundle,
        Action::OpenComposerInEditor,
        Action::CycleSemanticFilter,
        Action::ToggleTranscriptIndex,
        Action::ToggleRelatedLinks,
        Action::ToggleDuplicateFolds,
        Action::ToggleErrorLens,
        Action::ToggleHealthMarkers,
        Action::ToggleTurnOutline,
        Action::ToggleLaneFold,
        Action::TogglePinnedCompare,
        Action::DropBookmark,
        Action::ToggleBookmarks,
        Action::ToggleSessionTimeline,
        Action::AnnotateEntry,
        Action::ToggleAnnotations,
        Action::ToggleChangesSince,
        Action::OpenActionPalette,
        Action::ToggleCommandPalette,
        Action::ToggleHoverPreview,
        Action::ToggleHoverIntent,
        Action::ToggleBreadcrumbs,
        Action::RenameFocusedEntry,
        Action::DismissFirstRunHint,
    ];

    pub(crate) fn from_slug(slug: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.slug() == slug)
    }

    /// Short note surfaced by `/keymap` when the default binding is
    /// known to be unreliable across Linux terminals, tmux, or SSH.
    /// Returns `None` for bindings that are broadly portable.
    ///
    /// EXHAUSTIVENESS: enumerating every variant (rather than a `_ => None`
    /// catch-all) means any new `Action` requires an explicit decision here
    /// instead of silently inheriting a "portable" label.
    pub(crate) fn terminal_compat_note(self) -> Option<&'static str> {
        match self {
            // F11 is often consumed by the desktop window manager or
            // remapped by terminal emulators (fullscreen toggle).
            Self::ToggleConfigScreen => Some("terminal-dependent"),
            // Ctrl+T may collide with a custom tmux prefix (the default
            // prefix is Ctrl+B, but Ctrl+T is a common user rebind);
            // Ctrl+P is also a common editor binding. Both are Ctrl
            // chords that the host terminal or tmux may intercept before
            // Squeezy sees them.
            Self::ToggleTranscriptOverlay => Some("terminal-dependent"),
            Self::ToggleTaskPanel => Some("terminal-dependent"),
            // PageUp/PageDown are intercepted by some terminal emulators
            // for their own scrollback; also unreliable over SSH.
            Self::ScrollTranscriptPageUp => Some("terminal-dependent"),
            Self::ScrollTranscriptPageDown => Some("terminal-dependent"),
            // Home/End, Ctrl+Y, and Ctrl+R are broadly portable across
            // Linux terminals — no annotation needed.
            // New copy / navigation / direct-manipulation actions added by the
            // alt-screen renderer work. Alt-based chords, Ctrl+arrows, and
            // Ctrl+Enter are the classically unreliable ones across Linux
            // terminals, tmux, and SSH (Meta/Alt encoding, arrow modifiers, and
            // the Ctrl+Enter vs Enter ambiguity that needs keyboard enhancement).
            Self::CopyFocusedEntry
            | Self::CopyCurrentToolOutput
            | Self::CopyCodeBlock
            // Code-Aware Copy is `Alt+j` — the same Meta/Alt encoding that is
            // unreliable across Linux terminals, tmux, and SSH as the rest of
            // the copy chords.
            | Self::CopyAllCode
            | Self::CopyViewport
            | Self::CopyFullTranscript
            | Self::CopySelection
            // Multi-Cursor-Like Transcript Selection (§12.1.6): add-to-set is
            // `Alt+d` (Meta/Alt encoding) and the combined copy is `Ctrl+Alt+Y`
            // (Ctrl+Alt/Meta) — both the classically-unreliable case across Linux
            // terminals, tmux, and SSH as the rest of the copy/nav family.
            | Self::AddSelectionToSet
            | Self::CopyMultiSelection
            // Prompt Snippets From Selection (§12.3.2): save-from-selection is
            // `Alt+3` (an Alt+digit chord) and the picker toggle is `Ctrl+Alt+S`
            // (a Ctrl+Alt/Meta chord) — both the classically-unreliable Meta/Alt
            // encoding case across Linux terminals, tmux, and SSH as the rest of
            // the nav/copy/overlay family.
            | Self::SaveSnippetFromSelection
            | Self::ToggleSnippets
            | Self::JumpPrevUserTurn
            | Self::JumpNextUserTurn
            | Self::JumpPrevAssistant
            | Self::JumpNextAssistant
            | Self::JumpPrevToolCall
            | Self::JumpNextToolCall
            | Self::JumpPrevError
            | Self::JumpNextError
            | Self::FocusPrevEntry
            | Self::FocusNextEntry
            // Ctrl+Alt+L and Ctrl+Alt+M are Ctrl+Alt (Meta) chords — Alt
            // encoding is the classically unreliable case across Linux
            // terminals, tmux, and SSH.
            | Self::ToggleLatencyOverlay
            | Self::ToggleDogfoodMetrics
            // Jump-mark chords are `Alt`+key (`Alt+m` / `Alt+'`) — the same
            // Meta/Alt encoding that is unreliable across Linux terminals,
            // tmux, and SSH as the copy / jump-nav chords above.
            | Self::SetJumpMark
            | Self::JumpToMark
            // Minimap toggle is `Alt+r` — the same Meta/Alt encoding case.
            | Self::ToggleMinimap
            // Wide-block horizontal-nav chords are `Alt`+key (`Alt+w`/`Alt+h`/
            // `Alt+l`) — the same Meta/Alt encoding that is unreliable across
            // Linux terminals, tmux, and SSH as the copy / jump-nav chords above.
            | Self::ToggleSoftWrap
            | Self::ScrollBlockLeft
            | Self::ScrollBlockRight
            // Hyperlink-mode toggle is `Alt+8` — the same Meta/Alt encoding case.
            | Self::ToggleHyperlinks
            // Clipboard-history picker toggle is `Alt+p` — the same Meta/Alt
            // encoding that is unreliable across Linux terminals, tmux, and SSH.
            | Self::ToggleClipboardHistory
            // Session-bundle build is `Alt+b` — the same Meta/Alt encoding case.
            | Self::BuildSessionBundle
            // External-editor handoff is `Alt+e` — the same Meta/Alt encoding
            // case as the rest of the nav/copy family.
            | Self::OpenComposerInEditor
            // Semantic-filter cycle is `Alt+f` — the same Meta/Alt encoding case.
            | Self::CycleSemanticFilter
            // Transcript-index overlay toggle is `Alt+i` — the same Meta/Alt
            // encoding that is unreliable across Linux terminals, tmux, and SSH.
            | Self::ToggleTranscriptIndex
            // Related-Entry Links overlay toggle is `Alt+g` — the same Meta/Alt
            // encoding case.
            | Self::ToggleRelatedLinks
            // Duplicate-fold overlay toggle is `Alt+u` — the same Meta/Alt
            // encoding that is unreliable across Linux terminals, tmux, and SSH.
            | Self::ToggleDuplicateFolds
            // Error-Lens overlay toggle is `Alt+x` — the same Meta/Alt encoding
            // that is unreliable across Linux terminals, tmux, and SSH.
            | Self::ToggleErrorLens
            // Transcript-Health-Markers overlay toggle is `Alt+n` — the same
            // Meta/Alt encoding case as the rest of the nav/overlay family.
            | Self::ToggleHealthMarkers
            // Semantic-Turn-Outline overlay toggle is `Alt+s` — the same Meta/Alt
            // encoding case as the rest of the nav/overlay family.
            | Self::ToggleTurnOutline
            // Collapsible-Reasoning/Tool-Lanes overlay toggle is `Alt+z` — the
            // same Meta/Alt encoding case as the rest of the nav/overlay family.
            | Self::ToggleLaneFold
            // Pinned-Compare-View overlay toggle is `Alt+t` — the same Meta/Alt
            // encoding case as the rest of the nav/overlay family.
            | Self::TogglePinnedCompare
            // Reading Position Bookmarks (§12.2.4): drop is `Alt+;`, the list
            // overlay is `Alt+q` — both Meta/Alt chords, the same
            // terminal-dependent encoding case as the rest of the nav/overlay
            // family.
            | Self::DropBookmark
            | Self::ToggleBookmarks
            // Session Timeline overlay toggle is `Alt+9` — an Alt+digit chord,
            // the same Meta/Alt encoding that is unreliable across Linux
            // terminals, tmux, and SSH as the rest of the nav/overlay family.
            | Self::ToggleSessionTimeline
            // Entry Annotations (§12.2.5): annotate is `Alt+/`, the list overlay
            // is `Alt+\` — both Meta/Alt chords, the same terminal-dependent
            // encoding case as the rest of the nav/overlay family.
            | Self::AnnotateEntry
            | Self::ToggleAnnotations
            // What Changed Since Here? overlay toggle is `Alt+0` — an Alt+digit
            // chord, the same Meta/Alt encoding that is unreliable across Linux
            // terminals, tmux, and SSH as the rest of the nav/overlay family.
            | Self::ToggleChangesSince
            // Contextual Action Palette (§12.1.2) opens with `Alt+Enter` — an
            // Alt+Enter chord whose Meta-modifier encoding is exactly the
            // terminal-dependent case (Linux terminals / tmux / SSH may swallow or
            // remap it) the renderer plan flags for Alt chords.
            | Self::OpenActionPalette
            // Universal Command Palette toggle is `Ctrl+Alt+P` — a Ctrl+Alt (Meta)
            // chord, the same classically-unreliable encoding across Linux
            // terminals, tmux, and SSH as the `Ctrl+Alt+L`/`Ctrl+Alt+M` debug
            // chords above.
            | Self::ToggleCommandPalette
            // Hover Preview popover toggle is `Alt+1` — an Alt+digit chord, the
            // same Meta/Alt encoding that is unreliable across Linux terminals,
            // tmux, and SSH as the rest of the nav/overlay family.
            | Self::ToggleHoverPreview
            // Mouse Hover Intent toggle is `Ctrl+Alt+H` — a Ctrl+Alt (Meta)
            // chord, the same classically-unreliable encoding across Linux
            // terminals, tmux, and SSH as the `Ctrl+Alt+L`/`Ctrl+Alt+M`/
            // `Ctrl+Alt+P` chords above.
            | Self::ToggleHoverIntent
            // Clickable Breadcrumbs strip toggle is `Alt+2` — an Alt+digit chord,
            // the same Meta/Alt encoding that is unreliable across Linux
            // terminals, tmux, and SSH as the rest of the nav/overlay family.
            | Self::ToggleBreadcrumbs
            // Inline Rename Labels editor is `Ctrl+Alt+R` — a Ctrl+Alt (Meta)
            // chord, the same classically-unreliable encoding across Linux
            // terminals, tmux, and SSH as the `Ctrl+Alt+L`/`Ctrl+Alt+M`/
            // `Ctrl+Alt+P`/`Ctrl+Alt+H` chords.
            | Self::RenameFocusedEntry
            // First-Run Interaction Hint dismissal is `Ctrl+Alt+N` — a Ctrl+Alt
            // (Meta) chord, the same classically-unreliable encoding across Linux
            // terminals, tmux, and SSH as the `Ctrl+Alt+H`/`Ctrl+Alt+P` chords
            // above.
            | Self::DismissFirstRunHint
            | Self::OpenFocusedInDetail => Some("terminal-dependent"),
            // Plain keys and broadly-portable Ctrl chords. `>` is a bare
            // (shifted) printable key — no Alt/Ctrl chord — so it is broadly
            // portable across terminals, tmux, and SSH.
            Self::OpenSearch
            | Self::ToggleFocusedFold
            | Self::QueueUndo
            | Self::TranscriptHome
            | Self::TranscriptEnd
            | Self::CopyLastAssistant
            | Self::QuoteSelectionToCompose
            | Self::RestoreCancelledPrompt => None,
        }
    }

    /// Compiled-in default keybinding for the action. Mirrors what
    /// `handle_key` previously hardcoded, so a fresh install behaves
    /// exactly like the pre-`/keymap` build.
    pub(crate) fn default_binding(self) -> KeyBinding {
        match self {
            Self::ToggleConfigScreen => KeyBinding::new(KeyCode::F(11), KeyModifiers::NONE),
            Self::ToggleTranscriptOverlay => {
                KeyBinding::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
            }
            Self::OpenSearch => KeyBinding::new(KeyCode::Char('/'), KeyModifiers::NONE),
            Self::ToggleTaskPanel => KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            Self::CopyLastAssistant => KeyBinding::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            // Semantic-copy chords use `Alt`+letter to avoid the terminal
            // flow-control / host collisions of bare `Ctrl`-letters (see the
            // jump-navigation note below); the letters c/o/k/v/a are free.
            Self::CopyFocusedEntry => KeyBinding::new(KeyCode::Char('c'), KeyModifiers::ALT),
            Self::CopyCurrentToolOutput => KeyBinding::new(KeyCode::Char('o'), KeyModifiers::ALT),
            Self::CopyCodeBlock => KeyBinding::new(KeyCode::Char('k'), KeyModifiers::ALT),
            // Code-Aware Copy (§12.5.5). `Alt+j` sits next to the single-block
            // `Alt+k`; `j` is free among the semantic-copy Alt letters.
            Self::CopyAllCode => KeyBinding::new(KeyCode::Char('j'), KeyModifiers::ALT),
            Self::CopyViewport => KeyBinding::new(KeyCode::Char('v'), KeyModifiers::ALT),
            Self::CopyFullTranscript => KeyBinding::new(KeyCode::Char('a'), KeyModifiers::ALT),
            Self::CopySelection => KeyBinding::new(KeyCode::Char('y'), KeyModifiers::ALT),
            // Quote-to-compose. `>` is the conventional "quote" glyph; it is a
            // shifted printable key with no Ctrl/Alt chord. The composer
            // fall-through is gated in the dispatch (only fires with an active
            // selection), so binding the bare `>` here never steals normal
            // typing. `>` is captured as `KeyCode::Char('>')`; the lookup folds
            // away an incidental SHIFT (see `KeyBinding::new`).
            Self::QuoteSelectionToCompose => {
                KeyBinding::new(KeyCode::Char('>'), KeyModifiers::NONE)
            }
            // Multi-Cursor-Like Transcript Selection (§12.1.6). `Alt+d`
            // ("disjoint add") commits the live range into the set; `d` is free
            // among the semantic-copy/nav Alt letters. The combined copy uses the
            // obscure `Ctrl+Alt+Y` chord, sitting next to the single-selection
            // copy's `Alt+y` so the family stays mnemonic without colliding.
            Self::AddSelectionToSet => KeyBinding::new(KeyCode::Char('d'), KeyModifiers::ALT),
            Self::CopyMultiSelection => KeyBinding::new(
                KeyCode::Char('y'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // Prompt Snippets From Selection (§12.3.2). `Alt+3` saves the active
            // selection as a snippet — the next free `Alt`+digit after `Alt+1`
            // (hover preview) and `Alt+2` (breadcrumbs); every bare `Alt` letter in
            // the nav/copy/overlay family is taken, so the digit is the free,
            // composer-clear pick. The picker overlay opens with `Ctrl+Alt+S` —
            // `S` recalls "Snippet" and follows the `Ctrl+Alt+letter` style
            // (`Ctrl+Alt+L`/`Ctrl+Alt+M`/`Ctrl+Alt+P`); bare `Alt+s` is already the
            // turn-outline overlay, so the Ctrl+Alt modifier keeps the picker
            // distinct and clear of every composer chord.
            Self::SaveSnippetFromSelection => {
                KeyBinding::new(KeyCode::Char('3'), KeyModifiers::ALT)
            }
            Self::ToggleSnippets => KeyBinding::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::RestoreCancelledPrompt => {
                KeyBinding::new(KeyCode::Char('r'), KeyModifiers::CONTROL)
            }
            Self::ScrollTranscriptPageUp => KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
            Self::ScrollTranscriptPageDown => {
                KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE)
            }
            Self::TranscriptHome => KeyBinding::new(KeyCode::Home, KeyModifiers::NONE),
            Self::TranscriptEnd => KeyBinding::new(KeyCode::End, KeyModifiers::NONE),
            // Jump navigation defaults use `Alt`+key chords. Single-`Ctrl`
            // letters are deliberately avoided (terminal flow-control / host
            // collisions); `normalise_control_byte` canonicalises `META`→`ALT`
            // so these match regardless of the terminal protocol level.
            Self::JumpPrevUserTurn => KeyBinding::new(KeyCode::Up, KeyModifiers::ALT),
            Self::JumpNextUserTurn => KeyBinding::new(KeyCode::Down, KeyModifiers::ALT),
            Self::JumpPrevAssistant => KeyBinding::new(KeyCode::Left, KeyModifiers::ALT),
            Self::JumpNextAssistant => KeyBinding::new(KeyCode::Right, KeyModifiers::ALT),
            Self::JumpPrevToolCall => KeyBinding::new(KeyCode::Char(','), KeyModifiers::ALT),
            Self::JumpNextToolCall => KeyBinding::new(KeyCode::Char('.'), KeyModifiers::ALT),
            Self::JumpPrevError => KeyBinding::new(KeyCode::Char('['), KeyModifiers::ALT),
            Self::JumpNextError => KeyBinding::new(KeyCode::Char(']'), KeyModifiers::ALT),
            // Per-entry fold cursor. `Alt`+arrow is already the user/assistant
            // jump nav, so the fold cursor uses `Ctrl`+arrow. `Ctrl+O` toggles
            // the focused entry's fold — the keyboard twin of the mouse caret
            // click — and `Ctrl+Enter` opens the focused entry in the Ctrl+T
            // detail overlay (both free chords in the composer).
            Self::FocusPrevEntry => KeyBinding::new(KeyCode::Up, KeyModifiers::CONTROL),
            Self::FocusNextEntry => KeyBinding::new(KeyCode::Down, KeyModifiers::CONTROL),
            Self::ToggleFocusedFold => KeyBinding::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Self::OpenFocusedInDetail => KeyBinding::new(KeyCode::Enter, KeyModifiers::CONTROL),
            // Prompt-queue undo. Binds to bare `u`, claimed modally by the open
            // reorder overlay; `dispatch_keymap_action` gates it on the overlay
            // being open, so outside the overlay `u` keeps its composer meaning.
            // Bare `u` is safe to bind here precisely because of that gate.
            Self::QueueUndo => KeyBinding::new(KeyCode::Char('u'), KeyModifiers::NONE),
            // Hidden latency-budget overlay toggle. `Ctrl+Alt+L` is a
            // deliberately obscure debug chord — never a normal composer
            // keystroke — so the overlay stays out of the way while remaining
            // reachable at runtime (alongside the `SQUEEZY_LATENCY_OVERLAY`
            // env opt-in).
            Self::ToggleLatencyOverlay => KeyBinding::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // Hidden dogfood-telemetry overlay toggle. `Ctrl+Alt+M` is a
            // deliberately obscure debug chord — never a normal composer
            // keystroke — so the `/metrics` snapshot stays out of the way while
            // remaining reachable at runtime (alongside the
            // `SQUEEZY_DOGFOOD_METRICS` env opt-in). `m` (not the Enter-
            // colliding bare `Ctrl+M`) carries the explicit Alt modifier, so it
            // is distinct from carriage return.
            Self::ToggleDogfoodMetrics => KeyBinding::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // Jump marks (§11.2 / 11G.2). `Alt`+key chords matching the rest of
            // the navigation/copy family: `Alt+m` ("mark") sets, `Alt+'`
            // (the vi mark-jump key) jumps back. Bare `Alt` letters/punctuation
            // c/o/k/v/a/y/,/./[/] are taken; m and ' are free.
            Self::SetJumpMark => KeyBinding::new(KeyCode::Char('m'), KeyModifiers::ALT),
            Self::JumpToMark => KeyBinding::new(KeyCode::Char('\''), KeyModifiers::ALT),
            // Minimap turn rail (§11.2 / 11G.3). `Alt+r` ("rail") matches the
            // navigation/copy family's `Alt`+key style. Bare `Alt` letters
            // c/o/k/v/a/y/m/' and punctuation ,/./[/] are taken; r is free
            // (`Ctrl+R` is the cancelled-prompt restore, a distinct chord).
            Self::ToggleMinimap => KeyBinding::new(KeyCode::Char('r'), KeyModifiers::ALT),
            // Wide-block horizontal navigation (§11.2 / 11G.4). `Alt`+key chords
            // matching the rest of the navigation/copy family: `Alt+w` ("wrap")
            // toggles soft-wrap, `Alt+h`/`Alt+l` (the vi left/right keys) pan the
            // no-wrap view. Bare `Alt` letters c/o/k/v/a/y/m/r are taken; w/h/l are
            // free.
            Self::ToggleSoftWrap => KeyBinding::new(KeyCode::Char('w'), KeyModifiers::ALT),
            Self::ScrollBlockLeft => KeyBinding::new(KeyCode::Char('h'), KeyModifiers::ALT),
            Self::ScrollBlockRight => KeyBinding::new(KeyCode::Char('l'), KeyModifiers::ALT),
            // Hyperlink-mode cycle (§11.5 / 11G.5). `Alt+8` — the `8` recalls
            // "OSC 8". Bare `Alt` letters in the nav/copy family are taken;
            // `Alt`+digit is free and mnemonic.
            Self::ToggleHyperlinks => KeyBinding::new(KeyCode::Char('8'), KeyModifiers::ALT),
            // Clipboard-history picker (§12.6.1). `Alt+p` — `p` recalls "paste
            // history". Bare `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/
            // r/w/h/l) are taken; `p` is free (`Ctrl+P` is the task-panel toggle,
            // a distinct chord).
            Self::ToggleClipboardHistory => KeyBinding::new(KeyCode::Char('p'), KeyModifiers::ALT),
            // Session bundle (§12.6.6). `Alt+b` — `b` recalls "bundle". Bare
            // `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/r/w/h/l/p) are
            // taken; `b` is free.
            Self::BuildSessionBundle => KeyBinding::new(KeyCode::Char('b'), KeyModifiers::ALT),
            // External Editor Handoff (§12.6.5). `Alt+e` — `e` recalls "edit".
            // Bare `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/r/w/h/l/p)
            // are taken; `e` is free.
            Self::OpenComposerInEditor => KeyBinding::new(KeyCode::Char('e'), KeyModifiers::ALT),
            // Main-view Semantic Filter cycle (§12.5.2). `Alt+f` — `f` recalls
            // "filter". Bare `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/
            // r/w/h/l/p/b/e) are taken; `f` is free.
            Self::CycleSemanticFilter => KeyBinding::new(KeyCode::Char('f'), KeyModifiers::ALT),
            // Local Transcript Index (§12.5.1). `Alt+i` — `i` recalls "index".
            // Bare `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/r/w/h/l/p/
            // b/e/f) are taken; `i` is free.
            Self::ToggleTranscriptIndex => KeyBinding::new(KeyCode::Char('i'), KeyModifiers::ALT),
            // Related-Entry Links (§12.5.3). `Alt+g` — `g` recalls "graph" /
            // "go to related". Bare `Alt` letters in the nav/copy family
            // (c/o/k/v/a/y/m/r/w/h/l/p/b/e/f/i) are taken; `g` is free.
            Self::ToggleRelatedLinks => KeyBinding::new(KeyCode::Char('g'), KeyModifiers::ALT),
            // Duplicate-Output Folds (§12.5.4). `Alt+u` — `Alt+d` is the
            // composer's delete-word-forward shortcut and `Alt+g` is the
            // Related-Entry Links toggle; bare `Alt` letters in the nav/copy
            // family (c/o/k/v/a/y/m/r/w/h/l/p/b/e/f/i/g) are taken, so `u` is the
            // free letter that stays clear of the composer chords.
            Self::ToggleDuplicateFolds => KeyBinding::new(KeyCode::Char('u'), KeyModifiers::ALT),
            // Error Lenses (§12.5.6). `Alt+x` — `x` recalls "eXamine the error".
            // Bare `Alt` letters in the nav/copy family (c/o/k/v/a/y/m/r/w/h/l/p/
            // b/e/f/i/g/u) are taken; `x` is free and stays clear of the composer
            // chords.
            Self::ToggleErrorLens => KeyBinding::new(KeyCode::Char('x'), KeyModifiers::ALT),
            // Transcript Health Markers (§12.5.7). `Alt+n` — `n` recalls
            // "notices" / "health". Bare `Alt` letters in the nav/copy family
            // (c/o/k/v/a/y/m/r/w/h/l/p/b/e/f/i/g/u/x) are taken; `n` is free and
            // stays clear of the composer chords.
            Self::ToggleHealthMarkers => KeyBinding::new(KeyCode::Char('n'), KeyModifiers::ALT),
            // Semantic Turn Outline (§12.2.1). `Alt+s` ("structure"/"semantic
            // outline"); `s` is free among the overlay Alt letters.
            Self::ToggleTurnOutline => KeyBinding::new(KeyCode::Char('s'), KeyModifiers::ALT),
            // Collapsible Reasoning/Tool Lanes (§12.2.2). `Alt+z` ("zoom out" to a
            // higher reading altitude by collapsing lanes); `z` is free among the
            // overlay Alt letters.
            Self::ToggleLaneFold => KeyBinding::new(KeyCode::Char('z'), KeyModifiers::ALT),
            // Pinned Compare View (§12.2.3). `Alt+t` — `t` recalls "Two panes" /
            // "compare Transcript". `Alt+d` (the other free letter) collides with
            // the composer's delete-word-forward; among the remaining free Alt
            // letters `t` is the mnemonic pick. Distinct from `Ctrl+T` (the
            // transcript-overlay toggle); the modifier disambiguates them.
            Self::TogglePinnedCompare => KeyBinding::new(KeyCode::Char('t'), KeyModifiers::ALT),
            // Reading Position Bookmarks (§12.2.4). `Alt+;` drops a bookmark ("`;`"
            // is a free, easy-to-reach punctuation key — `Alt+t` is now the Pinned
            // Compare View toggle); `Alt+q` opens the bookmark list overlay (`q`
            // for the "quick-jump" list). Bare `Alt` letters in the nav/copy/
            // overlay family (c/o/k/v/a/y/m/r/w/h/l/p/b/e/f/i/g/u/x/n/s/z/t) are
            // taken; `q` and `;` are free.
            Self::DropBookmark => KeyBinding::new(KeyCode::Char(';'), KeyModifiers::ALT),
            Self::ToggleBookmarks => KeyBinding::new(KeyCode::Char('q'), KeyModifiers::ALT),
            // Session Timeline (§12.2.6). `Alt+9` — every bare `Alt` letter in the
            // nav/copy/overlay family is taken, so the timeline takes the next
            // free `Alt`+digit after `Alt+8` (hyperlinks). `9` is mnemonic-free
            // but unambiguous and stays clear of every composer chord.
            Self::ToggleSessionTimeline => KeyBinding::new(KeyCode::Char('9'), KeyModifiers::ALT),
            // Entry Annotations (§12.2.5). `Alt+/` annotates the focused entry
            // (`/` is a free punctuation key — the "note" slash; the bare Alt
            // letters c/o/k/v/a/y/m/r/w/h/l/p/b/e/f/i/g/u/x/n/s/z/t/q are taken),
            // and `Alt+\` opens the annotations list overlay (the adjacent free
            // punctuation key).
            Self::AnnotateEntry => KeyBinding::new(KeyCode::Char('/'), KeyModifiers::ALT),
            Self::ToggleAnnotations => KeyBinding::new(KeyCode::Char('\\'), KeyModifiers::ALT),
            // What Changed Since Here? (§12.2.7). `Alt+0` — the next free `Alt`+digit
            // after `Alt+8` (hyperlinks) and `Alt+9` (session timeline); it sits
            // beside the timeline chord it complements (mark a point, review the
            // delta) and every bare `Alt` letter in the nav/copy/overlay family is
            // taken. Mnemonic-free but unambiguous and clear of every composer chord.
            Self::ToggleChangesSince => KeyBinding::new(KeyCode::Char('0'), KeyModifiers::ALT),
            // Contextual Action Palette (§12.1.2). `Alt+Enter` — the classic
            // "act on the focused thing" chord, free of every composer key (plain
            // Enter submits, `Ctrl+Enter` opens detail, `Alt+Enter` is the next
            // natural Enter-family chord) and of every bare `Alt` letter/digit/
            // punctuation already claimed by the nav/copy/overlay family. Mnemonic
            // and unambiguous: a context menu for what is under focus.
            Self::OpenActionPalette => KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
            // Universal Command Palette (§12.1.1). `Ctrl+Alt+P` — `P` recalls
            // "Palette" and follows the existing `Ctrl+Alt+letter` debug-chord
            // style (`Ctrl+Alt+L`/`Ctrl+Alt+M`). It is free: bare `Ctrl+P` is the
            // task-panel toggle and bare `Alt+p` is the clipboard-history picker,
            // so the Ctrl+Alt modifier keeps the palette distinct from both while
            // staying clear of every composer chord.
            Self::ToggleCommandPalette => KeyBinding::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // Hover Preview popover (§12.1.4). `Alt+1` — the first free `Alt`+digit
            // (`Alt+8`/`Alt+9`/`Alt+0` are hyperlinks / session timeline / changes-
            // since); every bare `Alt` letter in the nav/copy/overlay family is
            // taken. The `1` reads as "level-1 / quick peek". It stays clear of every
            // composer chord and of the `Alt+Enter` action palette / `Ctrl+Enter`
            // detail chords.
            Self::ToggleHoverPreview => KeyBinding::new(KeyCode::Char('1'), KeyModifiers::ALT),
            // Mouse Hover Intent toggle (§12.1.3). `Ctrl+Alt+H` — `H` recalls
            // "Hover" and follows the existing `Ctrl+Alt+letter` style
            // (`Ctrl+Alt+L`/`Ctrl+Alt+M`/`Ctrl+Alt+P`). It is free: bare `Ctrl+H`
            // is classically ambiguous with Backspace, and every bare `Alt`
            // letter is already claimed by the nav/copy/overlay family, so the
            // Ctrl+Alt modifier keeps the toggle distinct and clear of every
            // composer chord.
            Self::ToggleHoverIntent => KeyBinding::new(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // Clickable Breadcrumbs (§12.1.5). `Alt+2` — the next free `Alt`+digit
            // after `Alt+1` (hover preview); `Alt+8`/`Alt+9`/`Alt+0` are
            // hyperlinks / session timeline / changes-since, and every bare `Alt`
            // letter in the nav/copy/overlay family is taken. The `2` reads as
            // "level-2 / where am I"; it stays clear of every composer chord and of
            // the `Alt+Enter` action-palette / `Ctrl+Enter` detail chords.
            Self::ToggleBreadcrumbs => KeyBinding::new(KeyCode::Char('2'), KeyModifiers::ALT),
            // Inline Rename Labels (§12.1.7). `Ctrl+Alt+R` — `R` recalls "Rename"
            // and follows the existing `Ctrl+Alt+letter` style
            // (`Ctrl+Alt+L`/`Ctrl+Alt+M`/`Ctrl+Alt+P`/`Ctrl+Alt+H`). It is free:
            // bare `Alt+R` is already the minimap toggle and every other bare `Alt`
            // letter in the nav/copy/overlay family is taken, so the Ctrl+Alt
            // modifier keeps the rename verb distinct and clear of every composer
            // chord and of the `Alt+Enter`/`Ctrl+Enter` chords.
            Self::RenameFocusedEntry => KeyBinding::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            // First-Run Interaction Hint dismissal (§12.1.8). `Ctrl+Alt+N` — `N`
            // recalls "notice / next" and follows the existing `Ctrl+Alt+letter`
            // style (`Ctrl+Alt+L`/`Ctrl+Alt+M`/`Ctrl+Alt+P`/`Ctrl+Alt+H`). It is
            // free: bare `Alt+n` is the health-markers overlay, and the Ctrl+Alt
            // modifier keeps the dismissal distinct and clear of every composer
            // chord. It is a no-op when no hint is showing, so the chord never
            // steals a key from the surface beneath.
            Self::DismissFirstRunHint => KeyBinding::new(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        }
    }
}

/// A normalised `(KeyCode, KeyModifiers)` pair. Modifiers are stored
/// with `SHIFT` stripped from `KeyCode::Char` because the shift bit
/// usually shows up on uppercase letters but not on punctuation
/// (terminal-dependent). Ctrl/Alt letters are lowercased to match
/// `handle_key`'s event normalisation before lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KeyBinding {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl KeyBinding {
    pub(crate) fn new(mut code: KeyCode, modifiers: KeyModifiers) -> Self {
        let mut modifiers = normalise_modifiers(code, modifiers);
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && let KeyCode::Char(ch) = code
            && ch.is_ascii_alphabetic()
        {
            code = KeyCode::Char(ch.to_ascii_lowercase());
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Self { code, modifiers }
    }

    /// Human-facing description: `"Ctrl+T"`, `"PageUp"`, `"Alt+k"`.
    /// Used by `/keymap` so the listing reads back the same syntax
    /// the user typed in the TOML file.
    pub(crate) fn display(&self) -> String {
        format_binding(self.code, self.modifiers)
    }
}

fn normalise_modifiers(code: KeyCode, modifiers: KeyModifiers) -> KeyModifiers {
    let mut out = modifiers;
    if let KeyCode::Char(ch) = code
        // Uppercase letters and shifted ASCII symbols (`>`, `?`, `!`, …) already
        // encode the shift in the produced glyph, so an additional SHIFT
        // modifier is redundant and terminal-dependent — fold it away so a
        // `>`-bound action matches whether or not the terminal also reports
        // SHIFT. Alphanumerics keep SHIFT (a Shift+letter is its own glyph; a
        // Shift+digit is a separate symbol the terminal already reports as that
        // symbol's char).
        && (ch.is_ascii_uppercase() || (ch.is_ascii_graphic() && !ch.is_ascii_alphanumeric()))
    {
        out.remove(KeyModifiers::SHIFT);
    }
    out
}

/// Compiled keymap: `key -> action` table plus the diagnostics needed
/// to render `/keymap` (per-action resolved binding, list of bad
/// overrides). Built once from `AppConfig` at TUI startup.
#[derive(Debug, Clone)]
pub(crate) struct KeymapResolver {
    by_key: HashMap<KeyBinding, Action>,
    bindings: BTreeMap<Action, KeyBinding>,
    /// Slugs that were not recognised as actions, kept verbatim so
    /// `/keymap` can warn instead of silently dropping them.
    pub(crate) unknown_actions: Vec<(String, String)>,
    /// Bindings the user supplied that did not parse as a keyspec,
    /// surfaced via `/keymap`.
    pub(crate) invalid_bindings: Vec<(String, String, String)>,
}

impl KeymapResolver {
    /// Build a resolver from a `[tui.keymap]` table (action_slug ->
    /// keyspec). Invalid entries are kept as diagnostics rather than
    /// hard-failing so a typo in one binding doesn't shadow every
    /// other one.
    pub(crate) fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut bindings: BTreeMap<Action, KeyBinding> = BTreeMap::new();
        for action in Action::ALL.iter().copied() {
            bindings.insert(action, action.default_binding());
        }
        let mut unknown_actions = Vec::new();
        let mut invalid_bindings = Vec::new();
        // Actions the user explicitly rebound. These win the reverse-lookup
        // collision over default-bound actions: an explicit `Alt+k = page_up`
        // override must take effect even if some *default*-bound action also
        // sits on `Alt+k` (otherwise a freshly-added default could silently
        // shadow the user's deliberate choice).
        let mut overridden: std::collections::BTreeSet<Action> = std::collections::BTreeSet::new();
        for (slug, spec) in overrides {
            let Some(action) = Action::from_slug(slug) else {
                unknown_actions.push((slug.clone(), spec.clone()));
                continue;
            };
            match parse_keyspec(spec) {
                Some(binding) => {
                    bindings.insert(action, binding);
                    overridden.insert(action);
                }
                None => {
                    invalid_bindings.push((slug.clone(), spec.clone(), action.slug().to_string()));
                }
            }
        }
        // Build the reverse lookup. Overridden actions are inserted first so an
        // explicit user rebind beats a colliding default; within each tier the
        // alphabetically-earlier action wins so `/keymap` and `lookup` agree on
        // a deterministic pick. The loser keeps its binding visible so
        // `/keymap` can flag the collision. `bindings` (a BTreeMap) iterates in
        // sorted action order, so the first insert per tier is the
        // alphabetically-earliest.
        let mut by_key: HashMap<KeyBinding, Action> = HashMap::new();
        for (action, binding) in &bindings {
            if overridden.contains(action) {
                by_key.entry(*binding).or_insert(*action);
            }
        }
        for (action, binding) in &bindings {
            by_key.entry(*binding).or_insert(*action);
        }
        Self {
            by_key,
            bindings,
            unknown_actions,
            invalid_bindings,
        }
    }

    pub(crate) fn lookup(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        let binding = KeyBinding::new(code, modifiers);
        self.by_key.get(&binding).copied()
    }

    pub(crate) fn binding(&self, action: Action) -> KeyBinding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or_else(|| action.default_binding())
    }

    /// True when more than one action resolves to the same key. Used
    /// by `/keymap` to flag conflicts; the resolver still picks a
    /// single winner via the reverse-lookup insertion order.
    pub(crate) fn collisions(&self) -> Vec<(KeyBinding, Vec<Action>)> {
        let mut groups: HashMap<KeyBinding, Vec<Action>> = HashMap::new();
        for (action, binding) in &self.bindings {
            groups.entry(*binding).or_default().push(*action);
        }
        let mut out: Vec<(KeyBinding, Vec<Action>)> = groups
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .collect();
        // Sort by the display string for deterministic `/keymap`
        // output across runs.
        out.sort_by_key(|entry| entry.0.display());
        for (_, actions) in &mut out {
            actions.sort();
        }
        out
    }
}

/// Parse a `"Ctrl+T"` / `"PageUp"` / `"Alt+k"` keyspec. Returns
/// `None` for anything we can't represent (so `/keymap` can flag it
/// and the default binding stays in effect).
pub(crate) fn parse_keyspec(spec: &str) -> Option<KeyBinding> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut key_part: Option<&str> = None;
    for raw_token in trimmed.split('+') {
        let token = raw_token.trim();
        if token.is_empty() {
            return None;
        }
        if eq_any_ignore_ascii_case(token, &["ctrl", "control"]) {
            modifiers |= KeyModifiers::CONTROL;
        } else if eq_any_ignore_ascii_case(token, &["alt", "meta", "opt", "option"]) {
            modifiers |= KeyModifiers::ALT;
        } else if token.eq_ignore_ascii_case("shift") {
            modifiers |= KeyModifiers::SHIFT;
        } else if eq_any_ignore_ascii_case(token, &["super", "cmd", "win", "windows"]) {
            modifiers |= KeyModifiers::SUPER;
        } else {
            if key_part.is_some() {
                // More than one non-modifier token isn't a valid
                // spec ("Ctrl+a+b" makes no sense).
                return None;
            }
            key_part = Some(token);
        }
    }
    let key = key_part?;
    let code = parse_keycode(key)?;
    Some(KeyBinding::new(code, modifiers))
}

fn parse_keycode(token: &str) -> Option<KeyCode> {
    if eq_any_ignore_ascii_case(token, &["enter", "return"]) {
        Some(KeyCode::Enter)
    } else if token.eq_ignore_ascii_case("tab") {
        Some(KeyCode::Tab)
    } else if eq_any_ignore_ascii_case(token, &["backtab", "shift-tab", "shifttab"]) {
        Some(KeyCode::BackTab)
    } else if eq_any_ignore_ascii_case(token, &["esc", "escape"]) {
        Some(KeyCode::Esc)
    } else if token.eq_ignore_ascii_case("space") {
        Some(KeyCode::Char(' '))
    } else if eq_any_ignore_ascii_case(token, &["backspace", "bs"]) {
        Some(KeyCode::Backspace)
    } else if eq_any_ignore_ascii_case(token, &["delete", "del"]) {
        Some(KeyCode::Delete)
    } else if eq_any_ignore_ascii_case(token, &["insert", "ins"]) {
        Some(KeyCode::Insert)
    } else if token.eq_ignore_ascii_case("home") {
        Some(KeyCode::Home)
    } else if token.eq_ignore_ascii_case("end") {
        Some(KeyCode::End)
    } else if eq_any_ignore_ascii_case(token, &["pageup", "pgup"]) {
        Some(KeyCode::PageUp)
    } else if eq_any_ignore_ascii_case(token, &["pagedown", "pgdn"]) {
        Some(KeyCode::PageDown)
    } else if token.eq_ignore_ascii_case("left") {
        Some(KeyCode::Left)
    } else if token.eq_ignore_ascii_case("right") {
        Some(KeyCode::Right)
    } else if token.eq_ignore_ascii_case("up") {
        Some(KeyCode::Up)
    } else if token.eq_ignore_ascii_case("down") {
        Some(KeyCode::Down)
    } else {
        // Function keys: F1..F24.
        if let Some(rest) = token.strip_prefix('f').or_else(|| token.strip_prefix('F')) {
            if let Ok(n) = rest.parse::<u8>()
                && (1..=24).contains(&n)
            {
                return Some(KeyCode::F(n));
            }
            return None;
        }
        // Single character: keep the user's casing so shifted
        // letters round-trip through `display()` cleanly.
        let mut chars = token.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(KeyCode::Char(ch))
    }
}

fn eq_any_ignore_ascii_case(token: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| token.eq_ignore_ascii_case(candidate))
}

fn format_binding(code: KeyCode, modifiers: KeyModifiers) -> String {
    let key = format_keycode(code);
    let mut out = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Alt");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Shift");
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Super");
    }
    if out.is_empty() {
        key
    } else {
        out.reserve(key.len() + 1);
        out.push('+');
        out.push_str(&key);
        out
    }
}

fn format_keycode(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(ch) => {
            let upper = ch.to_ascii_uppercase();
            upper.to_string()
        }
        other => format!("{other:?}"),
    }
}

/// Build the `/keymap` transcript card text — sorted list of
/// `action: KeySpec` rows plus a hint about how to override and a
/// validation block for any bad entries.
pub(crate) fn format_keymap_command(resolver: &KeymapResolver) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("Key bindings".to_string());
    lines.push("(override in settings.toml under [tui.keymap])".to_string());
    lines.push(String::new());
    let mut rows: Vec<(String, String, bool, Option<&'static str>)> = Vec::new();
    for action in Action::ALL.iter().copied() {
        let binding = resolver.binding(action);
        let default = action.default_binding();
        rows.push((
            action.slug().to_string(),
            binding.display(),
            binding != default,
            action.terminal_compat_note(),
        ));
    }
    let max_slug = rows.iter().map(|(s, _, _, _)| s.len()).max().unwrap_or(0);
    for (slug, display, is_override, note) in &rows {
        let marker = if *is_override { " (override)" } else { "" };
        let note_str = note.map(|n| format!("  [{n}]")).unwrap_or_default();
        lines.push(format!(
            "{:<width$}  {}{}{}",
            slug,
            display,
            marker,
            note_str,
            width = max_slug
        ));
    }
    lines.push(String::new());
    lines.push(
        "Bindings marked [terminal-dependent] may be intercepted by tmux, SSH, or the terminal"
            .to_string(),
    );
    lines.push("emulator. Use [tui.keymap] in settings.toml to remap to alternatives.".to_string());
    let collisions = resolver.collisions();
    if !collisions.is_empty() {
        lines.push(String::new());
        lines.push("Collisions:".to_string());
        for (binding, actions) in collisions {
            let mut names = String::new();
            for action in actions {
                if !names.is_empty() {
                    names.push_str(", ");
                }
                names.push_str(action.slug());
            }
            lines.push(format!("  {} → {}", binding.display(), names));
        }
    }
    if !resolver.unknown_actions.is_empty() {
        lines.push(String::new());
        lines.push("Unknown action names (ignored):".to_string());
        for (slug, spec) in &resolver.unknown_actions {
            lines.push(format!("  {slug} = {spec:?}"));
        }
    }
    if !resolver.invalid_bindings.is_empty() {
        lines.push(String::new());
        lines.push("Invalid key specs (default kept):".to_string());
        for (slug, spec, _) in &resolver.invalid_bindings {
            lines.push(format!("  {slug} = {spec:?}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "keymap_tests.rs"]
mod tests;
