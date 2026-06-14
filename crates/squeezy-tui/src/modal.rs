//! Shared modal-surface helpers for the startup pickers.
//!
//! Both the resume picker (`resume_picker.rs`) and the startup model picker
//! (`startup_model_picker.rs`) render the same modal mechanic: clear the
//! whole frame, then draw a centered rounded-border block and lay their
//! content into its inner rect. Phase 6 of the alt-screen renderer plan
//! (`docs/internal/TUI_ALT_SCREEN_RENDERER_PLAN.md`) requires both to draw
//! as modal surfaces on the *same* shared fullscreen terminal and to clear
//! once after they close so no ghost rows survive into the next surface.
//!
//! This module owns that mechanic so the two render paths cannot drift:
//!
//! - [`centered`] computes the centered sub-rect (replacing the two private
//!   `centered_area` copies that differed only in their caps).
//! - [`surface`] clears the target area and draws the centered bordered
//!   block, returning the block's inner rect for the caller's content.
//! - [`clear_after_close`] performs the single deliberate clear-on-close so
//!   the picker's block is wiped exactly once when the picker returns,
//!   rather than relying on the next surface to overpaint stale rows.

use std::io;

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Clear},
};

use crate::glyph_mode::GlyphMode;
use crate::render::theme;

/// Stable identity for TUI surfaces that take modal ownership of key and/or
/// render routing. The actual paint functions still live with their feature
/// modules (or, for legacy surfaces, in `lib.rs`); this registry documents the
/// common contract so the repeated "is any modal open?" gates do not drift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceKind {
    ClipboardHistory,
    KeybindingEditor,
    Snippets,
    Templates,
    TranscriptIndex,
    RelatedLinks,
    DuplicateFolds,
    ErrorLens,
    HealthMarkers,
    TurnOutline,
    LaneFold,
    Bookmarks,
    SessionTimeline,
    SubagentTimeline,
    SubagentCompare,
    ReviewBoard,
    ChangesSince,
    ActionPalette,
    ToolActions,
    Scratchpad,
    Annotations,
    CommandPalette,
    EditorHandoff,
    TranscriptOverlay,
    StatusLineSetup,
    ThemeEditor,
    WorkspaceProfile,
    SessionCheckpoint,
    TerminalProfile,
    GestureSettings,
    GlyphMode,
    SmartSplit,
    ConfigScreen,
    PastePreview,
    PasteTransform,
    SlashOverlay,
    RenameEdit,
    PendingApproval,
    PendingMcpElicitation,
    PendingUserInput,
    PendingPlanChoice,
    PendingFeedback,
}

/// Modal-surface contract used by input, macro replay, quick-switch, and render
/// audits. `render_owner` names the function/module that currently paints the
/// surface; `key_owner` names the handler path that owns keystrokes while active.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SurfaceDescriptor {
    pub(crate) kind: SurfaceKind,
    pub(crate) label: &'static str,
    pub(crate) key_owner: &'static str,
    pub(crate) render_owner: &'static str,
    pub(crate) allows_screen_selection: bool,
    pub(crate) consumes_macro_replay: bool,
    pub(crate) blocks_session_quick_switch: bool,
}

/// Documented paint/open precedence for modal render surfaces in
/// `render_surfaces`. Keep this in the same order as the early-return render
/// branches; non-render inline prompts are intentionally listed after the render
/// surfaces because they participate in key/replay/quick-switch gates but do not
/// replace the whole frame.
pub(crate) const RENDER_ORDER: &[SurfaceKind] = &[
    SurfaceKind::ClipboardHistory,
    SurfaceKind::KeybindingEditor,
    SurfaceKind::Snippets,
    SurfaceKind::Templates,
    SurfaceKind::TranscriptIndex,
    SurfaceKind::RelatedLinks,
    SurfaceKind::DuplicateFolds,
    SurfaceKind::ErrorLens,
    SurfaceKind::HealthMarkers,
    SurfaceKind::TurnOutline,
    SurfaceKind::LaneFold,
    SurfaceKind::Bookmarks,
    SurfaceKind::SessionTimeline,
    SurfaceKind::SubagentTimeline,
    SurfaceKind::SubagentCompare,
    SurfaceKind::ReviewBoard,
    SurfaceKind::ChangesSince,
    SurfaceKind::ActionPalette,
    SurfaceKind::ToolActions,
    SurfaceKind::Scratchpad,
    SurfaceKind::Annotations,
    SurfaceKind::CommandPalette,
    SurfaceKind::EditorHandoff,
    SurfaceKind::TranscriptOverlay,
    SurfaceKind::StatusLineSetup,
    SurfaceKind::ThemeEditor,
    SurfaceKind::WorkspaceProfile,
    SurfaceKind::SessionCheckpoint,
    SurfaceKind::TerminalProfile,
    SurfaceKind::GestureSettings,
    SurfaceKind::GlyphMode,
    SurfaceKind::SmartSplit,
    SurfaceKind::ConfigScreen,
    SurfaceKind::PastePreview,
    SurfaceKind::PasteTransform,
    SurfaceKind::SlashOverlay,
    SurfaceKind::RenameEdit,
    SurfaceKind::PendingApproval,
    SurfaceKind::PendingMcpElicitation,
    SurfaceKind::PendingUserInput,
    SurfaceKind::PendingPlanChoice,
    SurfaceKind::PendingFeedback,
];

pub(crate) fn active_surface_descriptors(
    app: &crate::TuiApp,
) -> impl Iterator<Item = SurfaceDescriptor> + '_ {
    RENDER_ORDER
        .iter()
        .copied()
        .filter(move |kind| kind.is_active(app))
        .map(SurfaceKind::descriptor)
}

pub(crate) fn any_active_surface_matching(
    app: &crate::TuiApp,
    predicate: impl Fn(SurfaceDescriptor) -> bool,
) -> bool {
    active_surface_descriptors(app).any(predicate)
}

pub(crate) fn active_inline_decision_surface(app: &crate::TuiApp) -> Option<SurfaceKind> {
    RENDER_ORDER.iter().copied().find(|kind| {
        matches!(
            kind,
            SurfaceKind::PastePreview
                | SurfaceKind::PasteTransform
                | SurfaceKind::PendingApproval
                | SurfaceKind::PendingMcpElicitation
                | SurfaceKind::PendingUserInput
                | SurfaceKind::PendingPlanChoice
                | SurfaceKind::PendingFeedback
        ) && kind.is_active(app)
    })
}

impl SurfaceDescriptor {
    pub(crate) fn blocks_keymap_actions(self) -> bool {
        matches!(
            self.kind,
            SurfaceKind::PendingApproval
                | SurfaceKind::PendingMcpElicitation
                | SurfaceKind::PendingUserInput
                | SurfaceKind::PendingPlanChoice
                | SurfaceKind::PendingFeedback
        )
    }

    pub(crate) fn blocks_composer_paste(self) -> bool {
        matches!(
            self.kind,
            SurfaceKind::KeybindingEditor
                | SurfaceKind::ThemeEditor
                | SurfaceKind::WorkspaceProfile
                | SurfaceKind::SessionCheckpoint
                | SurfaceKind::TerminalProfile
                | SurfaceKind::GestureSettings
                | SurfaceKind::GlyphMode
                | SurfaceKind::SmartSplit
                | SurfaceKind::PendingApproval
                | SurfaceKind::PendingMcpElicitation
                | SurfaceKind::PendingUserInput
                | SurfaceKind::PendingPlanChoice
                | SurfaceKind::PendingFeedback
                | SurfaceKind::PastePreview
                | SurfaceKind::PasteTransform
                | SurfaceKind::EditorHandoff
        )
    }

    pub(crate) fn blocks_prompt_queue_drain(self) -> bool {
        matches!(
            self.kind,
            SurfaceKind::ConfigScreen
                | SurfaceKind::TranscriptOverlay
                | SurfaceKind::SlashOverlay
                | SurfaceKind::PendingApproval
                | SurfaceKind::PendingMcpElicitation
                | SurfaceKind::PendingUserInput
                | SurfaceKind::PendingPlanChoice
                | SurfaceKind::PendingFeedback
                | SurfaceKind::PastePreview
                | SurfaceKind::PasteTransform
        )
    }
}

impl SurfaceKind {
    fn is_active(self, app: &crate::TuiApp) -> bool {
        match self {
            SurfaceKind::ClipboardHistory => app.clipboard_history_open,
            SurfaceKind::KeybindingEditor => app.keybinding_editor.is_some(),
            SurfaceKind::Snippets => app.snippets_open,
            SurfaceKind::Templates => app.templates_open,
            SurfaceKind::TranscriptIndex => app.transcript_index_open,
            SurfaceKind::RelatedLinks => app.related_links_open,
            SurfaceKind::DuplicateFolds => app.duplicate_folds_open,
            SurfaceKind::ErrorLens => app.error_lens_open,
            SurfaceKind::HealthMarkers => app.health_markers_open,
            SurfaceKind::TurnOutline => app.turn_outline_open,
            SurfaceKind::LaneFold => app.lane_fold_open,
            SurfaceKind::Bookmarks => app.bookmarks_open,
            SurfaceKind::SessionTimeline => app.session_timeline_open,
            SurfaceKind::SubagentTimeline => app.subagent_timeline_open,
            SurfaceKind::SubagentCompare => app.subagent_compare.is_some(),
            SurfaceKind::ReviewBoard => app.review_board_open,
            SurfaceKind::ChangesSince => app.changes_since_open,
            SurfaceKind::ActionPalette => app.action_palette.is_some(),
            SurfaceKind::ToolActions => app.tool_actions.is_some(),
            SurfaceKind::Scratchpad => app.scratchpad_open,
            SurfaceKind::Annotations => app.annotations_open,
            SurfaceKind::CommandPalette => app.command_palette.is_some(),
            SurfaceKind::EditorHandoff => app.editor_handoff.is_some(),
            SurfaceKind::TranscriptOverlay => app.transcript_overlay.is_some(),
            SurfaceKind::StatusLineSetup => app.status_line_setup.is_some(),
            SurfaceKind::ThemeEditor => app.theme_editor.is_some(),
            SurfaceKind::WorkspaceProfile => app.workspace_profile.is_some(),
            SurfaceKind::SessionCheckpoint => app.session_checkpoint.overlay.is_some(),
            SurfaceKind::TerminalProfile => app.terminal_profile_editor.is_some(),
            SurfaceKind::GestureSettings => app.gesture_settings_editor.is_some(),
            SurfaceKind::GlyphMode => app.glyph_mode_editor.is_some(),
            SurfaceKind::SmartSplit => app.smart_split.is_some(),
            SurfaceKind::ConfigScreen => app.config_screen.is_some(),
            SurfaceKind::PastePreview => app.paste_preview.is_some(),
            SurfaceKind::PasteTransform => app.paste_transform.is_some(),
            SurfaceKind::SlashOverlay => app.overlay.is_some(),
            SurfaceKind::RenameEdit => app.rename_edit.is_some(),
            SurfaceKind::PendingApproval => app.pending_approval.is_some(),
            SurfaceKind::PendingMcpElicitation => app.pending_mcp_elicitation.is_some(),
            SurfaceKind::PendingUserInput => app.pending_request_user_input.is_some(),
            SurfaceKind::PendingPlanChoice => app.plan.pending_choice.is_some(),
            SurfaceKind::PendingFeedback => app.pending_feedback.is_some(),
        }
    }

    pub(crate) fn descriptor(self) -> SurfaceDescriptor {
        use SurfaceKind::*;
        match self {
            ClipboardHistory => descriptor(
                self,
                "clipboard history",
                "handle_clipboard_history_key",
                "render_clipboard_history_surface",
                true,
                true,
                true,
            ),
            KeybindingEditor => descriptor(
                self,
                "keybinding editor",
                "handle_keybinding_editor_key",
                "render_keybinding_editor_surface",
                true,
                true,
                true,
            ),
            Snippets => descriptor(
                self,
                "prompt snippets",
                "handle_snippets_key",
                "render_snippets_surface",
                true,
                true,
                true,
            ),
            Templates => descriptor(
                self,
                "prompt templates",
                "handle_templates_key",
                "render_templates_surface",
                true,
                true,
                true,
            ),
            TranscriptIndex => descriptor(
                self,
                "transcript index",
                "handle_transcript_index_key",
                "render_transcript_index_surface",
                true,
                true,
                true,
            ),
            RelatedLinks => descriptor(
                self,
                "related links",
                "handle_related_links_key",
                "render_related_links_surface",
                true,
                true,
                true,
            ),
            DuplicateFolds => descriptor(
                self,
                "duplicate folds",
                "handle_duplicate_folds_key",
                "render_duplicate_folds_surface",
                true,
                true,
                true,
            ),
            ErrorLens => descriptor(
                self,
                "error lens",
                "handle_error_lens_key",
                "render_error_lens_surface",
                true,
                true,
                true,
            ),
            HealthMarkers => descriptor(
                self,
                "health markers",
                "handle_health_markers_key",
                "render_health_markers_surface",
                true,
                true,
                true,
            ),
            TurnOutline => descriptor(
                self,
                "turn outline",
                "handle_turn_outline_key",
                "render_turn_outline_surface",
                true,
                true,
                true,
            ),
            LaneFold => descriptor(
                self,
                "lane fold",
                "handle_lane_fold_key",
                "render_lane_fold_surface",
                true,
                true,
                true,
            ),
            Bookmarks => descriptor(
                self,
                "bookmarks",
                "handle_bookmarks_key",
                "render_bookmarks_surface",
                true,
                true,
                true,
            ),
            SessionTimeline => descriptor(
                self,
                "session timeline",
                "handle_session_timeline_key",
                "render_session_timeline_surface",
                true,
                true,
                true,
            ),
            SubagentTimeline => descriptor(
                self,
                "subagent timeline",
                "handle_subagent_timeline_key",
                "render_subagent_timeline_surface",
                true,
                true,
                true,
            ),
            SubagentCompare => descriptor(
                self,
                "subagent compare",
                "handle_subagent_compare_key",
                "render_subagent_compare_surface",
                false,
                true,
                false,
            ),
            ReviewBoard => descriptor(
                self,
                "review board",
                "handle_review_board_key",
                "render_review_board_surface",
                true,
                true,
                true,
            ),
            ChangesSince => descriptor(
                self,
                "changes since",
                "handle_changes_since_key",
                "render_changes_since_surface",
                true,
                true,
                true,
            ),
            ActionPalette => descriptor(
                self,
                "action palette",
                "handle_action_palette_key",
                "render_action_palette_surface",
                true,
                true,
                true,
            ),
            ToolActions => descriptor(
                self,
                "tool actions",
                "handle_tool_actions_key",
                "render_tool_actions_surface",
                true,
                true,
                true,
            ),
            Scratchpad => descriptor(
                self,
                "scratchpad",
                "handle_scratchpad_key",
                "render_scratchpad_surface",
                false,
                true,
                true,
            ),
            Annotations => descriptor(
                self,
                "annotations",
                "handle_annotations_key",
                "render_annotations_surface",
                true,
                true,
                true,
            ),
            CommandPalette => descriptor(
                self,
                "command palette",
                "handle_command_palette_key",
                "render_command_palette_surface",
                true,
                true,
                true,
            ),
            EditorHandoff => descriptor(
                self,
                "editor handoff",
                "handle_editor_handoff_key",
                "render_editor_handoff_surface",
                true,
                false,
                true,
            ),
            TranscriptOverlay => descriptor(
                self,
                "transcript overlay",
                "handle_transcript_overlay_key",
                "render_transcript_overlay_surface",
                false,
                false,
                true,
            ),
            StatusLineSetup => descriptor(
                self,
                "status line setup",
                "status_line_setup::handle_key",
                "status_line_setup::render",
                false,
                true,
                true,
            ),
            ThemeEditor => descriptor(
                self,
                "theme editor",
                "handle_theme_editor_key",
                "render_theme_editor_surface",
                true,
                true,
                true,
            ),
            WorkspaceProfile => descriptor(
                self,
                "workspace profile",
                "handle_workspace_profile_key",
                "render_workspace_profile_surface",
                true,
                true,
                true,
            ),
            SessionCheckpoint => descriptor(
                self,
                "session checkpoint",
                "handle_session_checkpoint_key",
                "session_checkpoint::render_surface",
                true,
                true,
                true,
            ),
            TerminalProfile => descriptor(
                self,
                "terminal profile",
                "handle_terminal_profile_key",
                "render_terminal_profile_surface",
                true,
                true,
                true,
            ),
            GestureSettings => descriptor(
                self,
                "gesture settings",
                "handle_gesture_settings_key",
                "render_gesture_settings_surface",
                true,
                true,
                true,
            ),
            GlyphMode => descriptor(
                self,
                "glyph mode",
                "handle_glyph_mode_key",
                "render_glyph_mode_surface",
                true,
                true,
                true,
            ),
            SmartSplit => descriptor(
                self,
                "smart split",
                "handle_smart_split_key",
                "render_smart_split_surface",
                false,
                true,
                true,
            ),
            ConfigScreen => descriptor(
                self,
                "config screen",
                "handle_config_key",
                "config_screen::render",
                false,
                true,
                true,
            ),
            PastePreview => descriptor(
                self,
                "paste preview",
                "handle_paste_preview_key",
                "render_paste_preview",
                false,
                true,
                true,
            ),
            PasteTransform => descriptor(
                self,
                "paste transform",
                "handle_paste_transform_key",
                "render_paste_transform",
                false,
                true,
                true,
            ),
            SlashOverlay => descriptor(
                self,
                "slash overlay",
                "handle_overlay_key",
                "overlay::render",
                false,
                false,
                true,
            ),
            RenameEdit => descriptor(
                self,
                "rename label",
                "handle_rename_key",
                "render_rename_editor",
                false,
                true,
                false,
            ),
            PendingApproval => descriptor(
                self,
                "approval",
                "handle_approval_key",
                "render_approval",
                false,
                true,
                true,
            ),
            PendingMcpElicitation => descriptor(
                self,
                "mcp elicitation",
                "handle_mcp_elicitation_key",
                "render_approval",
                false,
                true,
                true,
            ),
            PendingUserInput => descriptor(
                self,
                "user input request",
                "handle_request_user_input_key",
                "render_approval",
                false,
                true,
                true,
            ),
            PendingPlanChoice => descriptor(
                self,
                "plan choice",
                "handle_pending_plan_choice_key",
                "render_approval",
                false,
                true,
                true,
            ),
            PendingFeedback => descriptor(
                self,
                "feedback",
                "handle_pending_feedback_key",
                "render_approval",
                false,
                true,
                true,
            ),
        }
    }
}

const fn descriptor(
    kind: SurfaceKind,
    label: &'static str,
    key_owner: &'static str,
    render_owner: &'static str,
    allows_screen_selection: bool,
    consumes_macro_replay: bool,
    blocks_session_quick_switch: bool,
) -> SurfaceDescriptor {
    SurfaceDescriptor {
        kind,
        label,
        key_owner,
        render_owner,
        allows_screen_selection,
        consumes_macro_replay,
        blocks_session_quick_switch,
    }
}

/// Center a `max_width` x `max_height` area inside `full`, shrinking to fit
/// when the terminal is smaller than the requested caps. Centering is byte
/// identical to the previous per-picker `centered_area` helpers; callers
/// pass their own caps (resume keeps 160x32, startup keeps 98x20).
pub(crate) fn centered(full: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = full.width.min(max_width);
    let height = full.height.min(max_height);
    let x = full.x + full.width.saturating_sub(width) / 2;
    let y = full.y + full.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Render the shared modal surface: clear `full`, draw a centered accent block
/// sized by `max_width` x `max_height` with the supplied `title` (left-aligned),
/// and return the block's inner rect so the caller can lay its own content into
/// it. The border follows `glyph_mode` (§12.7.6): rounded box-drawing on
/// Unicode/Compact, ASCII `+-|` when the user opted into Minimal Glyph Mode so a
/// limited terminal never paints box-drawing tofu.
pub(crate) fn surface(
    frame: &mut Frame<'_>,
    full: Rect,
    max_width: u16,
    max_height: u16,
    title: Line<'_>,
    glyph_mode: GlyphMode,
) -> Rect {
    frame.render_widget(Clear, full);

    let area = centered(full, max_width, max_height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(glyph_mode.border_set())
        .border_style(Style::default().fg(theme::accent()))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Clear the whole terminal exactly once after a picker closes so the modal
/// block leaves no ghost rows behind. Generic over the `CrosstermBackend`
/// inner writer `W`, so it binds to the same guard terminal both pickers hold
/// in production (a `CrosstermBackend<TerminalWriter>`); the tests drive it
/// through a `CrosstermBackend<TerminalWriter::capture>` sink. A
/// `Terminal<TestBackend>` cannot satisfy this signature, so the picker
/// row/layout tests render directly instead of calling this.
pub(crate) fn clear_after_close<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
) -> io::Result<()> {
    // `Terminal::draw` already flushes both the buffer diff and the backend
    // writer, so no separate `terminal.flush()` is needed here.
    terminal.draw(|frame| {
        let full = frame.area();
        frame.render_widget(Clear, full);
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "modal_tests.rs"]
mod tests;
