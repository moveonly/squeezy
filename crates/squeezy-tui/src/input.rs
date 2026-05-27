use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use squeezy_agent::RequestUserInputResponse;
use squeezy_core::PermissionCapability;

use crate::{TranscriptItem, TuiApp, mention, overlay};

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
    /// in-memory commands (e.g. `/cost`, `/expand`).
    pub(crate) capabilities: &'static [PermissionCapability],
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

const fn slash_locked(name: &'static str, description: &'static str) -> SlashCommand {
    SlashCommand {
        name,
        description,
        available_during_task: false,
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
    // `/help` is forwarded to the model as a normal user turn, so it counts
    // as a network-exercising command.
    slash_caps(
        "/help",
        "show local Squeezy help topics",
        true,
        &[PermissionCapability::Network],
    ),
    slash_args_caps(
        "/config",
        "open the config screen (or pass a section name)",
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
        "attach a file as prompt context",
        false,
        "<path>",
        &[PermissionCapability::Read],
    ),
    slash("/attachments", "list attached context"),
    slash("/copy", "copy last answer or transcript"),
    // `/compact` triggers a summarisation turn against the model.
    SlashCommand {
        name: "/compact",
        description: "compact conversation context now (use '/compact undo' to restore)",
        available_during_task: false,
        parameter_hint: None,
        capabilities: &[PermissionCapability::Network],
    },
    slash("/collapse", "collapse transcript entries"),
    slash("/expand", "expand transcript entries"),
    slash_caps(
        "/diff",
        "show uncommitted changes (tracked + untracked)",
        true,
        &[PermissionCapability::Git, PermissionCapability::Read],
    ),
    slash("/tasks", "list background tasks (jobs + AI reviewer)"),
    slash_args("/task", "show a background task", true, "<id>"),
    slash_args("/task-cancel", "cancel a background task", true, "<id>"),
    // `/jobs`, `/job`, `/job-cancel` are retained as aliases for one release
    // window so muscle memory keeps working; the documented surface is now
    // `/tasks` (see F07-cc-tasks-and-background-jobs).
    slash("/jobs", "alias for /tasks"),
    slash_args("/job", "alias for /task", true, "<id>"),
    slash_args("/job-cancel", "alias for /task-cancel", true, "<id>"),
    slash_args("/pin", "pin transcript context", false, "<id>"),
    slash("/pins", "list pinned context"),
    slash_args("/unpin", "remove pinned context", false, "<id>"),
    slash_caps(
        "/feedback",
        "preview or send product feedback",
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
        "show a saved session",
        true,
        "<id>",
        &[PermissionCapability::Read],
    ),
    slash_args_caps(
        "/resume",
        "resume a saved session",
        false,
        "<id>",
        &[PermissionCapability::Read],
    ),
    slash_locked(
        "/fork",
        "branch the current session into a sibling with the same transcript",
    ),
    slash_args_caps(
        "/session-export",
        "export a saved session",
        false,
        "<id>",
        &[PermissionCapability::Read, PermissionCapability::Edit],
    ),
    // Locked + destructive: archives or purges sessions on disk.
    SlashCommand {
        name: "/session-cleanup",
        description: "soft-archive (default) or --purge old sessions",
        available_during_task: false,
        parameter_hint: Some("[--archive|--purge] [<id>...]"),
        capabilities: &[PermissionCapability::Destructive],
    },
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
    slash_args_caps(
        "/verbosity",
        "open config focused on response verbosity (or set inline)",
        false,
        "[concise|normal|verbose]",
        &[PermissionCapability::Edit],
    ),
    slash_args_caps(
        "/tool-verbosity",
        "open config focused on tool output verbosity (or set inline)",
        false,
        "[compact|normal|verbose]",
        &[PermissionCapability::Edit],
    ),
    slash_args("/detach", "remove attached context", false, "<id>"),
    slash_caps(
        "/statusline",
        "configure which items appear in the status bar",
        true,
        &[PermissionCapability::Edit],
    ),
    slash_args_caps(
        "/theme",
        "switch theme (persists to settings)",
        true,
        "[system|dark|light|catppuccin|high-contrast]",
        &[PermissionCapability::Edit],
    ),
    slash("/keymap", "list current key bindings"),
];

impl SlashCommand {
    pub(crate) fn is_dimmed(&self, task_in_progress: bool) -> bool {
        task_in_progress && !self.available_during_task
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
    clamp_slash_menu_index(app);
    refresh_mention_popup(app);
}

pub(crate) fn refresh_mention_popup(app: &mut TuiApp) {
    let Some(query) = mention::detect_mention(&app.input, app.input_cursor) else {
        app.mention_popup = None;
        return;
    };
    let root = std::path::Path::new(&app.directory);
    let needs_build = app
        .workspace_file_cache
        .as_ref()
        .is_none_or(|cache| cache.should_rebuild(root));
    if needs_build {
        app.workspace_file_cache = Some(mention::WorkspaceFileCache::build(root));
    }
    let matches = app
        .workspace_file_cache
        .as_ref()
        .map(|cache| mention::rank_files(&query.query, cache.files()))
        .unwrap_or_default();
    if matches.is_empty() {
        app.mention_popup = None;
        return;
    }
    app.mention_popup = Some(mention::MentionPopup::from_query(query, matches));
}

pub(crate) fn handle_overlay_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(overlay) = app.overlay.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
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
            apply_overlay_selection(app);
            true
        }
        _ => false,
    }
}

pub(crate) fn apply_overlay_selection(app: &mut TuiApp) {
    let Some(overlay) = app.overlay.take() else {
        return;
    };
    match overlay {
        overlay::Overlay::Model(picker) => {
            if let Some(entry) = picker.selected() {
                let provider = entry.provider;
                let id = entry.id;
                app.provider_name = provider;
                app.model = id.to_string();
                app.status = format!("selected model {provider}:{id}");
                app.push_transcript_item(TranscriptItem::system(format!(
                    "Model set to {provider}:{id} (restart the session to apply)"
                )));
            }
        }
        overlay::Overlay::Verbosity(picker) => {
            if let Some(entry) = picker.selected() {
                app.response_verbosity = entry.0;
                app.status = format!("response verbosity {}", entry.0.as_str());
            }
        }
        overlay::Overlay::ToolVerbosity(picker) => {
            if let Some(entry) = picker.selected() {
                app.tool_output_verbosity = entry.0;
                app.status = format!("tool output verbosity {}", entry.0.as_str());
            }
        }
        overlay::Overlay::Permissions(_) => {
            app.status = "permission overlay closed".to_string();
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
    clamp_slash_menu_index(app);
}

pub(crate) fn set_input(app: &mut TuiApp, input: String) {
    app.input = input;
    app.input_cursor = app.input.len();
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
    if input.trim().is_empty() || input.starts_with('/') {
        return;
    }
    if app.input_history.last().is_some_and(|last| last == &input) {
        return;
    }
    app.input_history.push(input);
    if app.input_history.len() > 100 {
        app.input_history.remove(0);
    }
}

pub(crate) fn reject_unknown_slash_command(app: &mut TuiApp, input: &str) -> bool {
    if !input.starts_with('/') {
        return false;
    }
    app.status = "unknown command; use Up/Down to choose a / command".to_string();
    true
}

pub(crate) fn recall_prompt_history(app: &mut TuiApp, direction: HistoryDirection) {
    if app.input_history.is_empty() {
        app.status = "no prompt history".to_string();
        return;
    }
    if app.input_history_index.is_none() && !app.input.trim().is_empty() {
        return;
    }
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
        (None, HistoryDirection::Next) => return,
        (Some(0), HistoryDirection::Previous) => Some(0),
        (Some(index), HistoryDirection::Previous) => Some(index - 1),
        (Some(index), HistoryDirection::Next) if index >= last => {
            set_input(app, app.input_history_draft.clone());
            app.input_history_draft.clear();
            app.input_history_index = None;
            app.slash_menu_index = 0;
            return;
        }
        (Some(index), HistoryDirection::Next) => Some(index + 1),
    };
    if let Some(index) = next {
        set_input(app, app.input_history[index].clone());
        app.input_history_index = Some(index);
        app.selected_entry = None;
        app.slash_menu_index = 0;
    }
}

pub(crate) fn slash_suggestions(input: &str) -> Vec<SlashCommand> {
    if !is_slash_completion_input(input) {
        return Vec::new();
    }
    let needle = input.trim();
    // Bare `/` lists every command, ordered alphabetically by name.
    if needle == "/" {
        let mut suggestions = SLASH_COMMANDS.to_vec();
        suggestions.sort_by(|left, right| left.name.cmp(right.name));
        return suggestions;
    }
    let mut scored: Vec<(SlashCommand, i32)> = SLASH_COMMANDS
        .iter()
        .copied()
        .filter_map(|command| {
            squeezy_rank::fuzzy_score(command.name, needle).map(|score| (command, score))
        })
        .collect();
    // Prefix/contiguous hits sort first via the negative bonuses in
    // `fuzzy_score`; ties broken alphabetically for stable rendering.
    scored.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.name.cmp(right.0.name)));
    scored.into_iter().map(|(cmd, _)| cmd).collect()
}

pub(crate) fn is_slash_completion_input(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.starts_with('/')
        && !trimmed[1..].contains(char::is_whitespace)
        && !trimmed.contains('\n')
}

pub(crate) fn clamp_slash_menu_index(app: &mut TuiApp) {
    let count = slash_suggestions(&app.input).len();
    if count == 0 {
        app.slash_menu_index = 0;
    } else if app.slash_menu_index >= count {
        app.slash_menu_index = count - 1;
    }
}

pub(crate) fn move_slash_menu_selection(app: &mut TuiApp, direction: SelectionDirection) -> bool {
    let count = slash_suggestions(&app.input).len();
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
    let suggestions = slash_suggestions(&app.input);
    if suggestions.is_empty() {
        return false;
    }
    let selected = suggestions[app.slash_menu_index.min(suggestions.len() - 1)];
    if app.input.trim() == selected.name {
        return false;
    }
    set_input(app, format!("{} ", selected.name));
    app.slash_menu_index = 0;
    app.status = format!("selected {}", selected.name);
    true
}

pub(crate) fn handle_request_user_input_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(mut pending) = app.pending_request_user_input.take() else {
        return false;
    };
    let choice_count = pending.request.choices.len();
    match key.code {
        KeyCode::Up => {
            if choice_count > 0 {
                pending.selection_index = pending.selection_index.saturating_sub(1);
            }
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Down => {
            if choice_count > 0 {
                pending.selection_index =
                    (pending.selection_index + 1).min(choice_count.saturating_sub(1));
            }
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Enter => {
            if choice_count > 0
                && let Some(choice) = pending.request.choices.get(pending.selection_index)
            {
                let response = RequestUserInputResponse::choice(choice.value.clone());
                let _ = pending.response_tx.send(response);
                app.status = format!("answered: {}", choice.label);
                return true;
            }
            if pending.request.allow_freeform && !app.input.trim().is_empty() {
                let text = std::mem::take(&mut app.input);
                app.input_cursor = 0;
                let _ = pending
                    .response_tx
                    .send(RequestUserInputResponse::freeform(text));
                app.status = "answered with free-form text".to_string();
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
        KeyCode::Backspace if pending.request.allow_freeform => {
            delete_before_cursor(app);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Delete if pending.request.allow_freeform => {
            delete_at_cursor(app);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Left if pending.request.allow_freeform => {
            move_input_cursor_left(app);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Right if pending.request.allow_freeform => {
            move_input_cursor_right(app);
            app.pending_request_user_input = Some(pending);
            true
        }
        KeyCode::Char(ch)
            if pending.request.allow_freeform
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            insert_input_char(app, ch);
            app.pending_request_user_input = Some(pending);
            true
        }
        _ => {
            app.pending_request_user_input = Some(pending);
            true
        }
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
