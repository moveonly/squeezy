use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use squeezy_agent::{Agent, PendingConfigSwap, RequestUserInputResponse};
use squeezy_core::PermissionCapability;
use tokio::sync::oneshot;

use crate::{PendingRequestUserInput, TranscriptItem, TuiApp, mention, overlay};

pub(crate) const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) available_during_task: bool,
    pub(crate) parameter_hint: Option<&'static str>,
    /// Capabilities this command exercises on the user's behalf. Surfaced in
    /// the slash menu so a user can see at a glance whether typing the
    /// command will read the filesystem, hit the network, modify settings, or
    /// perform a destructive operation. Empty for purely informational or
    /// in-memory commands (e.g. `/cost`, `/tasks`).
    pub(crate) capabilities: &'static [PermissionCapability],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashCommandOccurrence {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) command: SlashCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlashCompletionContext {
    start: usize,
    end: usize,
    at_prompt_start: bool,
}

const fn slash(name: &'static str, description: &'static str) -> SlashCommand {
    SlashCommand {
        name,
        description,
        available_during_task: true,
        parameter_hint: None,
        capabilities: &[],
    }
}

const fn slash_args(
    name: &'static str,
    description: &'static str,
    available_during_task: bool,
    parameter_hint: &'static str,
) -> SlashCommand {
    SlashCommand {
        name,
        description,
        available_during_task,
        parameter_hint: Some(parameter_hint),
        capabilities: &[],
    }
}

const fn slash_caps(
    name: &'static str,
    description: &'static str,
    available_during_task: bool,
    capabilities: &'static [PermissionCapability],
) -> SlashCommand {
    SlashCommand {
        name,
        description,
        available_during_task,
        parameter_hint: None,
        capabilities,
    }
}

const fn slash_args_caps(
    name: &'static str,
    description: &'static str,
    available_during_task: bool,
    parameter_hint: &'static str,
    capabilities: &'static [PermissionCapability],
) -> SlashCommand {
    SlashCommand {
        name,
        description,
        available_during_task,
        parameter_hint: Some(parameter_hint),
        capabilities,
    }
}

/// If `text` starts with a registered slash command followed by end-of-line or
/// whitespace, return the byte length of that command. Used by the renderer
/// to highlight recognised commands in amber both in the live input widget
/// and in the transcript view of a sent prompt. Returns the longest match so
/// `/job-cancel foo` resolves to `/job-cancel`, not `/job`.
#[cfg(test)]
pub(crate) fn match_slash_command_prefix(text: &str) -> Option<usize> {
    if !text.starts_with('/') {
        return None;
    }
    SLASH_COMMANDS
        .iter()
        .filter_map(|cmd| {
            if !text.starts_with(cmd.name) {
                return None;
            }
            let len = cmd.name.len();
            // Must be followed by end-of-string or whitespace — `/helpme` is
            // not `/help`.
            let next = text.as_bytes().get(len).copied();
            if next.is_none() || next.is_some_and(|b| b.is_ascii_whitespace()) {
                Some(len)
            } else {
                None
            }
        })
        .max()
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    // `/help` is answered locally for curated topics (zero provider cost).
    // Unknown topics can escalate to a DocHelp subagent, but that path is
    // uncommon; the command is not labelled as network-capable so it doesn't
    // look riskier or costlier than it is in practice.
    slash_args("/help", "show local Squeezy help topics", true, "[topic]"),
    slash_args_caps(
        "/config",
        "open config (or pass a section name)",
        true,
        "[section]",
        &[PermissionCapability::Edit],
    ),
    slash_caps(
        "/model",
        "open config focused on provider and model",
        true,
        &[PermissionCapability::Edit],
    ),
    slash_caps(
        "/permissions",
        "open config focused on permissions",
        true,
        &[PermissionCapability::Edit],
    ),
    slash_caps(
        "/mcp",
        "open config focused on MCP servers (status, enable, restart)",
        true,
        &[PermissionCapability::Edit],
    ),
    slash_args(
        "/plan",
        "switch to Plan mode (optionally with a prompt to run)",
        false,
        "[prompt]",
    ),
    slash_args(
        "/build",
        "switch to Build mode (optionally with a prompt to run)",
        false,
        "[prompt]",
    ),
    slash_args_caps(
        "/plans",
        "manage persisted plan-mode artifacts (list/show/delete/set-active/open)",
        true,
        "[list|show|delete|set-active|open] [<id>]",
        &[PermissionCapability::Read],
    ),
    slash("/cost", "show token and cost accounting"),
    slash("/context", "show context budget and compaction state"),
    slash("/reviewer", "show recent AI reviewer auto-decisions"),
    slash_args_caps(
        "/attach",
        "insert a file token in the prompt",
        false,
        "<path>",
        &[PermissionCapability::Read],
    ),
    // `/compact` triggers a summarisation turn against the model.
    SlashCommand {
        name: "/compact",
        description: "compact context now (undo to restore; history to view timeline)",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[PermissionCapability::Network],
    },
    // `/clear` rotates to a fresh session; the prior one stays resumable.
    // Purely local (in-memory wipe + on-disk session rotation), so no
    // capability badges, but it must not fire mid-turn.
    SlashCommand {
        name: "/clear",
        description: "clear the conversation and start fresh (prior session stays resumable)",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[],
    },
    slash_caps(
        "/diff",
        "show uncommitted changes (tracked + untracked)",
        true,
        &[PermissionCapability::Git, PermissionCapability::Read],
    ),
    slash("/tasks", "list background tasks"),
    slash_args("/task", "show a background task", true, "<id>"),
    slash_args("/task-cancel", "cancel a background task", true, "<id>"),
    SlashCommand {
        name: "/pin",
        description: "pin a transcript item (opens a picker)",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[],
    },
    slash("/pins", "list pinned context"),
    slash_args("/unpin", "remove pinned context", false, "<id>"),
    slash_caps(
        "/feedback",
        "preview feedback and ask to send",
        true,
        &[PermissionCapability::Network],
    ),
    slash_caps(
        "/report",
        "preview or send a bug report",
        true,
        &[PermissionCapability::Network],
    ),
    slash_caps(
        "/sessions",
        "list recent sessions",
        true,
        &[PermissionCapability::Read],
    ),
    slash_args_caps(
        "/session",
        "show a saved session, or rename/label the active one",
        true,
        "<id> | rename <name> | label <name>",
        &[PermissionCapability::Read],
    ),
    slash_args_caps(
        "/resume",
        "resume a saved session",
        false,
        "<id>",
        &[PermissionCapability::Read],
    ),
    slash_args(
        "/fork",
        "branch the current session into a sibling (optionally under another workspace)",
        false,
        "[<workspace_path>]",
    ),
    slash_args_caps(
        "/session-export",
        "export a saved session",
        false,
        "<id>",
        &[PermissionCapability::Read, PermissionCapability::Edit],
    ),
    slash_args_caps(
        "/session-export-html",
        "export a saved session as self-contained HTML",
        false,
        "<id> [path]",
        &[PermissionCapability::Read, PermissionCapability::Edit],
    ),
    slash_caps(
        "/checkpoints",
        "list local checkpoints",
        true,
        &[PermissionCapability::Read],
    ),
    slash_args_caps(
        "/checkpoint",
        "show a local checkpoint",
        true,
        "<id>",
        &[PermissionCapability::Read],
    ),
    SlashCommand {
        name: "/undo",
        description: "undo the latest checkpoint",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[
            PermissionCapability::Edit,
            PermissionCapability::Destructive,
        ],
    },
    SlashCommand {
        name: "/revert-turn",
        description: "revert a turn checkpoint",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[
            PermissionCapability::Edit,
            PermissionCapability::Destructive,
        ],
    },
    slash_args_caps(
        "/effort",
        "set reasoning effort for this session (or `auto` to clear)",
        false,
        "[low|medium|high|xhigh|auto]",
        &[PermissionCapability::Edit],
    ),
    slash("/cheap", "force next turn onto the provider's cheap model"),
    slash("/parent", "force next turn onto the parent model"),
    slash_args(
        "/router",
        "open routing config (or on|off to toggle session-wide auto-routing)",
        true,
        "[on|off]",
    ),
    slash_args_caps(
        "/tool-verbosity",
        "open config focused on tool output verbosity (or set inline)",
        false,
        "[compact|normal|verbose]",
        &[PermissionCapability::Edit],
    ),
    slash_caps(
        "/statusline",
        "configure which items appear in the status bar",
        true,
        &[PermissionCapability::Edit],
    ),
    slash_args_caps(
        "/theme",
        "open theme config or switch theme",
        true,
        "[default|bright|fun|catppuccin|high-contrast|<custom>]",
        &[PermissionCapability::Edit],
    ),
    slash("/keymap", "list current key bindings"),
];

impl SlashCommand {
    pub(crate) fn is_dimmed(&self, task_in_progress: bool) -> bool {
        task_in_progress && !self.available_during_task
    }

    pub(crate) fn supports_inline_use(&self) -> bool {
        matches!(self.name, "/attach" | "/help" | "/plan" | "/build")
    }

    pub(crate) fn visible_with_checkpoints(&self, _checkpoints_enabled: bool) -> bool {
        true
    }

    /// Short label used in the slash menu badge, e.g. `net`, `read`, `edit`.
    /// Matches `PermissionCapability::as_str()` so users can correlate the
    /// hint with the `permissions.toml` capability they recognise.
    pub(crate) fn capability_badges(&self) -> Vec<&'static str> {
        self.capabilities
            .iter()
            .map(|cap| capability_badge_label(*cap))
            .collect()
    }
}

pub(crate) const fn capability_badge_label(capability: PermissionCapability) -> &'static str {
    match capability {
        PermissionCapability::Read => "read",
        PermissionCapability::Search => "search",
        PermissionCapability::Edit => "edit",
        PermissionCapability::Shell => "shell",
        PermissionCapability::Network => "net",
        PermissionCapability::Mcp => "mcp",
        PermissionCapability::Git => "git",
        PermissionCapability::Compiler => "compiler",
        PermissionCapability::Destructive => "destructive",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionDirection {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryDirection {
    Previous,
    Next,
}

pub(crate) fn note_input_edited(app: &mut TuiApp) {
    app.input_history_index = None;
    app.input_history_draft.clear();
    app.selected_entry = None;
    app.prune_prompt_attachments();
    clamp_slash_menu_index(app);
    refresh_mention_popup(app);
}

pub(crate) fn refresh_mention_popup(app: &mut TuiApp) {
    let Some(query) = mention::detect_mention(&app.input, app.input_cursor) else {
        app.mention_popup = None;
        return;
    };
    // Kick off a workspace walk off the UI thread when the cache is
    // missing or stale, but only when one isn't already in flight. The
    // popup keeps ranking against the last-known cache (if any) until the
    // fresh list lands via `drain_pending_mention_walk`, so the composer
    // never blocks on `readdir`/`stat`.
    let root = std::path::Path::new(&app.directory);
    let needs_build = app
        .workspace_file_cache
        .as_ref()
        .is_none_or(|cache| cache.should_rebuild(root));
    if needs_build && app.pending_mention_walk.is_none() {
        let root = app.directory.clone();
        let (tx, rx) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(mention::WorkspaceFileCache::build(std::path::Path::new(
                &root,
            )));
        });
        app.pending_mention_walk = Some(rx);
    }
    let truncated = app
        .workspace_file_cache
        .as_ref()
        .is_some_and(|cache| cache.is_truncated());
    let (matches, total) = app
        .workspace_file_cache
        .as_ref()
        .map(|cache| mention::rank_files(&query.query, cache.files()))
        .unwrap_or_default();
    if matches.is_empty() {
        app.mention_popup = None;
        return;
    }
    app.mention_popup = Some(mention::MentionPopup::from_query(
        query, matches, total, truncated,
    ));
}

pub(crate) fn handle_overlay_key(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> bool {
    let Some(overlay) = app.overlay.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => {
            close_overlay(app);
            app.status = "overlay cancelled".to_string();
            true
        }
        KeyCode::Up => {
            overlay.move_up();
            true
        }
        KeyCode::Down => {
            overlay.move_down();
            true
        }
        KeyCode::Enter => {
            apply_overlay_selection(app, agent);
            true
        }
        _ => false,
    }
}

/// Close the slash-command overlay and clear the matching dialog-handle
/// bookkeeping. Centralised so the `overlay` / `overlay_active_id` pair
/// stays invariant: `active_id` is `Some` iff `overlay` is `Some`.
pub(crate) fn close_overlay(app: &mut TuiApp) {
    app.overlay = None;
    app.overlay_active_id = None;
}

pub(crate) fn apply_overlay_selection(app: &mut TuiApp, agent: &mut Agent) {
    let Some(overlay) = app.overlay.take() else {
        return;
    };
    app.overlay_active_id = None;
    match overlay {
        overlay::Overlay::Model(picker) => {
            if let Some(entry) = picker.selected() {
                let provider = entry.provider;
                let id = entry.id;
                app.provider_name = provider;
                app.model = id.to_string();
                if agent.provider_name() == provider {
                    // Same provider: the client is model-agnostic (each
                    // request's model comes from `config.model`), so swap the
                    // config only. Armed as a NextPrompt swap so an in-flight
                    // turn is undisturbed; drained at the next `start_turn`,
                    // after which pricing and the model both track the new id.
                    let mut new_cfg = agent.config_snapshot();
                    new_cfg.model = id.to_string();
                    agent.arm_config_swap(PendingConfigSwap {
                        config: new_cfg,
                        provider: None,
                        display_note: Some(format!(
                            "model {provider}:{id} (applies on next prompt)"
                        )),
                    });
                    app.status = format!("selected model {provider}:{id}");
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "Model set to {provider}:{id} (applies on your next prompt)"
                    )));
                } else {
                    // Cross-provider switch needs a freshly built provider
                    // client (auth/transport) that the picker can't synthesize
                    // from a provider name alone, so keep the restart path.
                    app.status = format!("selected model {provider}:{id}");
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "Model set to {provider}:{id} (restart the session to apply — \
                         cross-provider switch)"
                    )));
                }
            }
        }
    }
}

pub(crate) fn handle_mention_popup_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    if app.mention_popup.is_none() {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            app.mention_popup = None;
            true
        }
        KeyCode::Up => {
            if let Some(popup) = app.mention_popup.as_mut() {
                popup.move_up();
            }
            true
        }
        KeyCode::Down => {
            if let Some(popup) = app.mention_popup.as_mut() {
                popup.move_down();
            }
            true
        }
        KeyCode::Tab | KeyCode::Enter => apply_mention_popup(app),
        _ => false,
    }
}

pub(crate) fn apply_mention_popup(app: &mut TuiApp) -> bool {
    let popup = match app.mention_popup.as_ref() {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return false,
    };
    if let Some((new_input, new_cursor)) = popup.apply(&app.input) {
        app.input = new_input;
        app.input_cursor = new_cursor;
        app.mention_popup = None;
        clamp_slash_menu_index(app);
        return true;
    }
    false
}

pub(crate) fn clear_input(app: &mut TuiApp) {
    app.input.clear();
    app.input_cursor = 0;
    app.clear_prompt_attachments();
    clamp_slash_menu_index(app);
}

pub(crate) fn set_input(app: &mut TuiApp, input: String) {
    app.input = input;
    app.input_cursor = app.input.len();
    app.clear_prompt_attachments();
    clamp_input_cursor(app);
    clamp_slash_menu_index(app);
}

/// Move the stashed `cancelled_prompt` back into the composer so a turn
/// the user just aborted doesn't vaporize their typed prompt. Skipped
/// when the composer already has text (the user started a new draft mid-
/// cancel) or when nothing was stashed. Returns `true` when the prompt
/// was restored.
pub(crate) fn restore_prompt_after_cancel(app: &mut TuiApp) -> bool {
    if !app.input.is_empty() {
        return false;
    }
    let Some(text) = app.cancelled_prompt.take() else {
        return false;
    };
    app.input = text;
    app.input_cursor = app.input.len();
    app.clear_prompt_attachments();
    true
}

pub(crate) fn input_cursor(app: &TuiApp) -> usize {
    text_cursor(&app.input, app.input_cursor)
}

pub(crate) fn clamp_input_cursor(app: &mut TuiApp) {
    app.input_cursor = text_cursor(&app.input, app.input_cursor);
}

pub(crate) fn text_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

pub(crate) fn insert_input_char(app: &mut TuiApp, ch: char) {
    clamp_input_cursor(app);
    app.input.insert(app.input_cursor, ch);
    app.input_cursor += ch.len_utf8();
    note_input_edited(app);
}

pub(crate) fn insert_input_text(app: &mut TuiApp, text: &str) {
    if text.is_empty() {
        return;
    }
    clamp_input_cursor(app);
    app.input.insert_str(app.input_cursor, text);
    app.input_cursor += text.len();
    note_input_edited(app);
}

pub(crate) fn delete_before_cursor(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor == 0 {
        app.input_cursor = 0;
        return;
    }
    let previous = app.input[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    app.input.drain(previous..cursor);
    app.input_cursor = previous;
    note_input_edited(app);
}

pub(crate) fn delete_at_cursor(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor >= app.input.len() {
        app.input_cursor = app.input.len();
        return;
    }
    let next = cursor
        + app.input[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    app.input.drain(cursor..next);
    app.input_cursor = cursor;
    note_input_edited(app);
}

pub(crate) fn delete_to_line_start(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    let start = line_start_before_cursor(&app.input, cursor);
    if start >= cursor {
        if cursor > 0 && app.input[..cursor].ends_with('\n') {
            delete_before_cursor(app);
        } else {
            app.input_cursor = cursor;
        }
        return;
    }
    app.input.drain(start..cursor);
    app.input_cursor = start;
    note_input_edited(app);
}

pub(crate) fn delete_to_line_end(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    let end = line_end_after_cursor(&app.input, cursor);
    if end <= cursor {
        if cursor < app.input.len() {
            delete_at_cursor(app);
        } else {
            app.input_cursor = app.input.len();
        }
        return;
    }
    app.input.drain(cursor..end);
    app.input_cursor = cursor;
    note_input_edited(app);
}

pub(crate) fn delete_previous_word(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    let start = previous_word_start(&app.input, cursor);
    if start >= cursor {
        app.input_cursor = cursor;
        return;
    }
    app.input.drain(start..cursor);
    app.input_cursor = start;
    note_input_edited(app);
}

pub(crate) fn delete_next_word(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    let end = next_word_end(&app.input, cursor);
    if end <= cursor {
        app.input_cursor = cursor;
        return;
    }
    app.input.drain(cursor..end);
    app.input_cursor = cursor;
    note_input_edited(app);
}

pub(crate) fn move_input_cursor_left(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = app.input[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
}

pub(crate) fn move_input_cursor_right(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor >= app.input.len() {
        app.input_cursor = app.input.len();
        return;
    }
    app.input_cursor = cursor
        + app.input[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
}

pub(crate) fn move_input_cursor_line_start(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = line_start_before_cursor(&app.input, cursor);
}

pub(crate) fn move_input_cursor_line_end(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = line_end_after_cursor(&app.input, cursor);
}

pub(crate) fn move_input_cursor_word_left(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = previous_word_start(&app.input, cursor);
}

pub(crate) fn move_input_cursor_word_right(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = next_word_end(&app.input, cursor);
}

/// Move the cursor up one line in a multi-line input. Returns `true` when
/// it moved, `false` when there is no previous line (caller can then fall
/// through to history recall / transcript scroll).
pub(crate) fn move_input_cursor_up(app: &mut TuiApp) -> bool {
    let cursor = input_cursor(app);
    let curr_start = line_start_before_cursor(&app.input, cursor);
    if curr_start == 0 {
        return false;
    }
    let col = cursor - curr_start;
    let prev_end = curr_start - 1;
    let prev_start = app.input[..prev_end]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let prev_len = prev_end - prev_start;
    app.input_cursor = prev_start + col.min(prev_len);
    true
}

/// Move the cursor down one line in a multi-line input. Returns `true`
/// when it moved, `false` when already on the last line.
pub(crate) fn move_input_cursor_down(app: &mut TuiApp) -> bool {
    let cursor = input_cursor(app);
    let curr_start = line_start_before_cursor(&app.input, cursor);
    let col = cursor - curr_start;
    let Some(next_start) = app.input[curr_start..]
        .find('\n')
        .map(|offset| curr_start + offset + 1)
    else {
        return false;
    };
    let next_end = app.input[next_start..]
        .find('\n')
        .map(|offset| next_start + offset)
        .unwrap_or(app.input.len());
    let next_len = next_end - next_start;
    app.input_cursor = next_start + col.min(next_len);
    true
}

fn line_start_before_cursor(text: &str, cursor: usize) -> usize {
    let cursor = text_cursor(text, cursor);
    text[..cursor]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn line_end_after_cursor(text: &str, cursor: usize) -> usize {
    let cursor = text_cursor(text, cursor);
    text[cursor..]
        .find('\n')
        .map(|index| cursor + index)
        .unwrap_or(text.len())
}

fn previous_word_start(text: &str, cursor: usize) -> usize {
    let cursor = text_cursor(text, cursor);
    let prefix = &text[..cursor];
    let Some((mut start, ch)) = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
    else {
        return 0;
    };
    let separator = is_word_separator(ch);
    for (index, ch) in prefix[..start].char_indices().rev() {
        if ch.is_whitespace() || is_word_separator(ch) != separator {
            break;
        }
        start = index;
    }
    start
}

fn next_word_end(text: &str, cursor: usize) -> usize {
    let cursor = text_cursor(text, cursor);
    let suffix = &text[cursor..];
    let Some((first_offset, first)) = suffix.char_indices().find(|(_, ch)| !ch.is_whitespace())
    else {
        return text.len();
    };
    let separator = is_word_separator(first);
    let mut end = cursor + first_offset + first.len_utf8();
    for (offset, ch) in suffix[first_offset + first.len_utf8()..].char_indices() {
        if ch.is_whitespace() || is_word_separator(ch) != separator {
            break;
        }
        end = cursor + first_offset + first.len_utf8() + offset + ch.len_utf8();
    }
    end
}

fn is_word_separator(ch: char) -> bool {
    WORD_SEPARATORS.contains(ch)
}

pub(crate) fn push_input_history(app: &mut TuiApp, input: String) {
    // Slash commands are UI actions, not prompts the user would want to
    // recall via Up/Down — keep them out of the ring at the TUI seam so
    // the storage layer stays agnostic of squeezy's command vocabulary.
    if input.starts_with('/') {
        return;
    }
    app.input_history.push(input);
}

pub(crate) fn reject_unknown_slash_command(app: &mut TuiApp, input: &str) -> bool {
    if !input.starts_with('/') {
        return false;
    }
    app.status = "unknown command; use Up/Down to choose a / command".to_string();
    true
}

/// Returns `true` when the keypress was consumed by history recall — the
/// composer text or history index changed, or the recall deliberately
/// stepped out of history mode back to the draft. Returns `false` when
/// there was nothing to recall in the requested direction, so the caller
/// can treat the arrow as "fall through" (e.g. Down then focuses the
/// subagent pane instead of dead-ending in the composer).
pub(crate) fn recall_prompt_history(app: &mut TuiApp, direction: HistoryDirection) -> bool {
    if app.input_history.is_empty() {
        app.status = "no prompt history".to_string();
        return false;
    }
    // Plain Up/Down iterate history even with a half-typed draft in the
    // composer — the draft is stashed into `input_history_draft` (see the
    // `(None, Previous)` arm) and restored when the user steps back down past
    // the newest entry, matching shell history behaviour.
    let last = app.input_history.len() - 1;
    let next = match (app.input_history_index, direction) {
        (None, HistoryDirection::Previous) => {
            app.input_history_draft = if app.input.trim().is_empty() {
                String::new()
            } else {
                app.input.clone()
            };
            Some(last)
        }
        (None, HistoryDirection::Next) => return false,
        (Some(0), HistoryDirection::Previous) => Some(0),
        (Some(index), HistoryDirection::Previous) => Some(index - 1),
        (Some(index), HistoryDirection::Next) if index >= last => {
            let draft = std::mem::take(&mut app.input_history_draft);
            set_input(app, draft);
            app.input_history_index = None;
            app.slash_menu_index = 0;
            return true;
        }
        (Some(index), HistoryDirection::Next) => Some(index + 1),
    };
    if let Some(index) = next {
        if let Some(entry) = app.input_history.get(index) {
            set_input(app, entry.to_string());
        }
        app.input_history_index = Some(index);
        app.selected_entry = None;
        app.slash_menu_index = 0;
    }
    true
}

#[cfg(test)]
pub(crate) fn slash_suggestions(input: &str) -> Vec<SlashCommand> {
    slash_suggestions_at(input, input.len())
}

pub(crate) fn slash_suggestions_at(input: &str, cursor: usize) -> Vec<SlashCommand> {
    let Some(context) = slash_completion_context(input, cursor) else {
        return Vec::new();
    };
    let needle = &input[context.start..context.end];
    // Bare `/` lists every command, ordered alphabetically by name.
    if needle == "/" {
        let mut suggestions = SLASH_COMMANDS
            .iter()
            .filter(|command| slash_command_matches_context(command, context))
            .copied()
            .collect::<Vec<_>>();
        suggestions.sort_by(|left, right| left.name.cmp(right.name));
        return suggestions;
    }
    let query = crate::fuzzy::PreparedQuery::new(needle);
    let mut scored: Vec<(SlashCommand, i32)> = SLASH_COMMANDS
        .iter()
        .filter(|command| slash_command_matches_context(command, context))
        .copied()
        .filter_map(|command| {
            crate::fuzzy::score_prepared(command.name, &query).map(|score| (command, score))
        })
        .collect();
    // Word-boundary / consecutive bonuses keep prefix hits on top;
    // higher score is better here, ties broken alphabetically.
    scored.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.name.cmp(right.0.name)));
    scored.into_iter().map(|(cmd, _)| cmd).collect()
}

pub(crate) fn slash_suggestions_for_app(app: &TuiApp) -> Vec<SlashCommand> {
    slash_suggestions_at(&app.input, app.input_cursor)
        .into_iter()
        .filter(|command| command.visible_with_checkpoints(app.checkpoints_enabled))
        .collect()
}

fn slash_suggestion_count_for_app(app: &TuiApp) -> usize {
    slash_suggestion_count_at(&app.input, app.input_cursor, app.checkpoints_enabled)
}

fn slash_suggestion_count_at(input: &str, cursor: usize, checkpoints_enabled: bool) -> usize {
    let Some(context) = slash_completion_context(input, cursor) else {
        return 0;
    };
    let needle = &input[context.start..context.end];
    let include = |command: &&SlashCommand| {
        slash_command_matches_context(command, context)
            && command.visible_with_checkpoints(checkpoints_enabled)
    };
    if needle == "/" {
        return SLASH_COMMANDS.iter().filter(include).count();
    }
    let query = crate::fuzzy::PreparedQuery::new(needle);
    SLASH_COMMANDS
        .iter()
        .filter(include)
        .filter(|command| crate::fuzzy::score_prepared(command.name, &query).is_some())
        .count()
}

fn slash_command_matches_context(command: &SlashCommand, context: SlashCompletionContext) -> bool {
    context.at_prompt_start || command.supports_inline_use()
}

fn slash_completion_context(input: &str, cursor: usize) -> Option<SlashCompletionContext> {
    let cursor = text_cursor(input, cursor);
    let (start, end) = token_bounds_at_cursor(input, cursor);
    let token = &input[start..end];
    if !token.starts_with('/') || token[1..].contains('/') {
        return None;
    }
    Some(SlashCompletionContext {
        start,
        end,
        at_prompt_start: input[..start].trim().is_empty(),
    })
}

fn token_bounds_at_cursor(input: &str, cursor: usize) -> (usize, usize) {
    let start = input[..cursor]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    let end = input[cursor..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| cursor + index)
        .unwrap_or(input.len());
    (start, end)
}

pub(crate) fn find_inline_slash_dispatch_command(input: &str) -> Option<SlashCommandOccurrence> {
    let mut cursor = 0;
    while let Some(occurrence) = next_slash_command_occurrence(input, &mut cursor) {
        if !input[..occurrence.start].trim().is_empty() && occurrence.command.supports_inline_use()
        {
            return Some(occurrence);
        }
    }
    None
}

pub(crate) fn slash_command_ranges(input: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(occurrence) = next_slash_command_occurrence(input, &mut cursor) {
        if input[..occurrence.start].trim().is_empty() || occurrence.command.supports_inline_use() {
            ranges.push((occurrence.start, occurrence.end));
        }
    }
    ranges
}

fn next_slash_command_occurrence(
    input: &str,
    cursor: &mut usize,
) -> Option<SlashCommandOccurrence> {
    while *cursor < input.len() {
        let Some((relative_start, _)) = input[*cursor..]
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
        else {
            *cursor = input.len();
            return None;
        };
        let start = *cursor + relative_start;
        let end = input[start..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, _)| start + index)
            .unwrap_or(input.len());
        *cursor = end;
        let token = &input[start..end];
        if let Some(command) = SLASH_COMMANDS
            .iter()
            .copied()
            .find(|command| command.name == token)
        {
            return Some(SlashCommandOccurrence {
                start,
                end,
                command,
            });
        }
    }
    None
}

pub(crate) fn clamp_slash_menu_index(app: &mut TuiApp) {
    let count = slash_suggestion_count_for_app(app);
    if count == 0 {
        app.slash_menu_index = 0;
    } else if app.slash_menu_index >= count {
        app.slash_menu_index = count - 1;
    }
}

pub(crate) fn move_slash_menu_selection(app: &mut TuiApp, direction: SelectionDirection) -> bool {
    let count = slash_suggestion_count_for_app(app);
    if count == 0 {
        return false;
    }
    app.slash_menu_index = match direction {
        SelectionDirection::Previous => {
            if app.slash_menu_index == 0 {
                count - 1
            } else {
                app.slash_menu_index - 1
            }
        }
        SelectionDirection::Next => (app.slash_menu_index + 1) % count,
    };
    true
}

pub(crate) fn complete_selected_slash_command(app: &mut TuiApp) -> bool {
    let Some(context) = slash_completion_context(&app.input, app.input_cursor) else {
        return false;
    };
    let suggestions = slash_suggestions_for_app(app);
    if suggestions.is_empty() {
        return false;
    }
    let selected = suggestions[app.slash_menu_index.min(suggestions.len() - 1)];
    if &app.input[context.start..context.end] == selected.name {
        return false;
    }
    let replacement = format!("{} ", selected.name);
    app.input
        .replace_range(context.start..context.end, &replacement);
    app.input_cursor = context.start + replacement.len();
    note_input_edited(app);
    app.slash_menu_index = 0;
    app.status = format!("selected {}", selected.name);
    true
}

pub(crate) fn handle_request_user_input_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(mut pending) = app.pending_request_user_input.take() else {
        return false;
    };
    let choice_count = pending.request.choices.len();
    let allow_freeform = pending.request.allow_freeform;
    let max_selection = if allow_freeform {
        choice_count
    } else {
        choice_count.saturating_sub(1)
    };
    match key.code {
        KeyCode::Up => {
            if choice_count > 0 || allow_freeform {
                pending.selection_index = pending.selection_index.saturating_sub(1);
            }
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Down => {
            if choice_count > 0 || allow_freeform {
                pending.selection_index = (pending.selection_index + 1).min(max_selection);
            }
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Enter => {
            if allow_freeform && pending.selection_index >= choice_count {
                if pending.answer.trim().is_empty() {
                    app.pending_request_user_input = Some(pending);
                    return true;
                }
                let text = std::mem::take(&mut pending.answer);
                pending.answer_cursor = 0;
                let _ = pending
                    .response_tx
                    .send(RequestUserInputResponse::freeform(text));
                app.status = "answered with free-form text".to_string();
                return true;
            }
            if choice_count > 0
                && let Some(choice) = pending.request.choices.get(pending.selection_index)
            {
                let response = RequestUserInputResponse::choice(choice.value.clone());
                let _ = pending.response_tx.send(response);
                app.status = format!("answered: {}", choice.label);
                return true;
            }
            // Nothing to send yet — keep the modal up.
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Esc => {
            let _ = pending
                .response_tx
                .send(RequestUserInputResponse::cancelled());
            app.status = "plan-mode question cancelled".to_string();
            true
        }
        KeyCode::Backspace if allow_freeform => {
            pending.selection_index = choice_count;
            delete_answer_before_cursor(&mut pending);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Delete if allow_freeform => {
            pending.selection_index = choice_count;
            delete_answer_at_cursor(&mut pending);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Left if allow_freeform => {
            pending.selection_index = choice_count;
            move_answer_cursor_left(&mut pending);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Right if allow_freeform => {
            pending.selection_index = choice_count;
            move_answer_cursor_right(&mut pending);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Char(ch)
            if allow_freeform
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            pending.selection_index = choice_count;
            insert_answer_char(&mut pending, ch);
            app.pending_request_user_input = Some(pending);
            true
        }
        _ => {
            app.pending_request_user_input = Some(pending);
            true
        }
    }
}

fn insert_answer_char(pending: &mut PendingRequestUserInput, ch: char) {
    let cursor = clamp_byte_cursor(&pending.answer, pending.answer_cursor);
    pending.answer.insert(cursor, ch);
    pending.answer_cursor = cursor + ch.len_utf8();
}

fn delete_answer_before_cursor(pending: &mut PendingRequestUserInput) {
    let cursor = clamp_byte_cursor(&pending.answer, pending.answer_cursor);
    if cursor == 0 {
        return;
    }
    let mut prev = cursor - 1;
    while prev > 0 && !pending.answer.is_char_boundary(prev) {
        prev -= 1;
    }
    pending.answer.replace_range(prev..cursor, "");
    pending.answer_cursor = prev;
}

fn delete_answer_at_cursor(pending: &mut PendingRequestUserInput) {
    let cursor = clamp_byte_cursor(&pending.answer, pending.answer_cursor);
    if cursor >= pending.answer.len() {
        return;
    }
    let mut next = cursor + 1;
    while next < pending.answer.len() && !pending.answer.is_char_boundary(next) {
        next += 1;
    }
    pending.answer.replace_range(cursor..next, "");
    pending.answer_cursor = cursor;
}

fn move_answer_cursor_left(pending: &mut PendingRequestUserInput) {
    let cursor = clamp_byte_cursor(&pending.answer, pending.answer_cursor);
    if cursor == 0 {
        return;
    }
    let mut prev = cursor - 1;
    while prev > 0 && !pending.answer.is_char_boundary(prev) {
        prev -= 1;
    }
    pending.answer_cursor = prev;
}

fn move_answer_cursor_right(pending: &mut PendingRequestUserInput) {
    let cursor = clamp_byte_cursor(&pending.answer, pending.answer_cursor);
    if cursor >= pending.answer.len() {
        return;
    }
    let mut next = cursor + 1;
    while next < pending.answer.len() && !pending.answer.is_char_boundary(next) {
        next += 1;
    }
    pending.answer_cursor = next;
}

fn clamp_byte_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
