use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use squeezy_agent::Agent;
use squeezy_core::{
    config_schema::{CONFIG_SECTIONS, FieldKind, FieldValue, SectionId},
    is_builtin_tui_theme_name, is_tui_theme_color_token, normalize_tui_theme_name,
};

use super::{
    ConfigFeedback, ConfigScope, ConfigScreenState, EditorOutcome, FieldEditor, KeyOutcome,
    ModelPickerState, PromptEditorState, SearchOverlayState, SecretEntryState,
    Severity as NotifySeverity, ThemeEditor, ThemeRow, clear_scope_override,
    clear_scope_override_silent, compute_search_matches, cycle_to_next_registry_model,
    discard_all_session_writes, handle_editor_key, model_field_meta, open_editor_for,
    perform_reset, picker_matches, provider_api_key_env, provider_inline_api_key,
    provider_section_name, save_field, save_field_silent, save_inline_provider_api_key,
    save_theme_color, save_theme_delete, save_theme_rename, save_theme_selection,
    save_theme_snapshot, undo_last_write, unset_theme_color,
};

pub(crate) fn handle_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    // Sub-modes take precedence over the regular browse keymap.
    if state.reset_confirm.is_some() {
        return handle_reset_confirm_key(state, agent, notifications, key);
    }
    if state.discard_confirm {
        return handle_discard_confirm_key(state, agent, notifications, key);
    }
    if state.secret_entry.is_some() {
        return handle_secret_entry_key(state, agent, notifications, key);
    }
    if state.theme_editor.is_some() {
        return handle_theme_editor_key(state, agent, notifications, key);
    }
    if state.search.is_some() {
        return handle_search_key(state, key);
    }
    if state.picker.is_some() {
        return handle_picker_key(state, agent, notifications, key);
    }
    if state.prompt_editor.is_some() {
        return handle_prompt_editor_key(state, agent, notifications, key);
    }
    if state.mcp_pending_delete.is_some() {
        return handle_mcp_delete_confirm_key(state, notifications, key);
    }
    if state.mcp_add.is_some() {
        return handle_mcp_add_form_key(state, notifications, key);
    }
    // On the McpServers section, our page-specific bindings (e/r/a/d
    // + S+letter session-only variants, Enter to toggle) precede the
    // regular browse keys. Navigation (arrows, Tab) still falls
    // through so the user can move between sections.
    if state.current_section().id == SectionId::McpServers
        && let Some(outcome) = handle_mcp_browse_key(state, notifications, key)
    {
        return outcome;
    }
    if let Some(editor) = &mut state.editor {
        let commit = handle_editor_key(editor, key);
        match commit {
            EditorOutcome::KeepEditing => return KeyOutcome::KeepOpen,
            EditorOutcome::Cancel => {
                state.editor = None;
                return KeyOutcome::KeepOpen;
            }
            EditorOutcome::Commit(value) => {
                state.editor = None;
                let field = state.current_field();
                let previous = (field.get)(&state.effective);
                if let Err(msg) = (field.set)(&mut state.effective, value.clone()) {
                    notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
                } else {
                    state.dirty = true;
                    // Save immediately; the apply pipeline below routes the
                    // change to the right tier and queues notifications.
                    save_field(state, agent, notifications, field, previous, value);
                }
                return KeyOutcome::KeepOpen;
            }
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if state.dirty {
                notifications.push(
                    "Closed with unsaved edits already applied. Re-open to view current state.",
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::Close
        }
        (KeyCode::Tab, _) => {
            state.scope = state.scope.next();
            KeyOutcome::KeepOpen
        }
        (KeyCode::BackTab, _) => {
            state.scope = state.scope.prev();
            KeyOutcome::KeepOpen
        }
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            if state.section_index == 0 {
                state.section_index = CONFIG_SECTIONS.len() - 1;
            } else {
                state.section_index -= 1;
            }
            state.field_index = 0;
            KeyOutcome::KeepOpen
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
            state.section_index = (state.section_index + 1) % CONFIG_SECTIONS.len();
            state.field_index = 0;
            KeyOutcome::KeepOpen
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            let n = state.row_count();
            if state.field_index == 0 {
                state.field_index = n.saturating_sub(1);
            } else {
                state.field_index -= 1;
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
            let n = state.row_count();
            state.field_index = (state.field_index + 1) % n.max(1);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char(' '), _) => {
            if state.current_section().id == SectionId::Themes {
                handle_theme_row_action(state, agent, notifications);
                return KeyOutcome::KeepOpen;
            }
            if state.on_synthetic_api_key_row() {
                open_api_key_entry_for_current_provider(state, notifications);
                return KeyOutcome::KeepOpen;
            }
            if let Some(action) = state.reset_action_at_row(state.field_index) {
                state.reset_confirm = Some(action.scope);
                return KeyOutcome::KeepOpen;
            }
            let field = state.current_field();
            // Env-shadowed fields are inert; show the same hint we use for
            // Enter so Space doesn't silently no-op.
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to edit in the screen.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            // Space cycles inline through the field's value space. On the
            // Repo and Local tabs the cycle includes a virtual "inherit"
            // position so the user can return to the parent tier's value
            // without leaving the row.
            let current_value = (field.get)(&state.effective);
            let active_owns_field = state.scope_owns_field(field).unwrap_or(false);
            // On non-User tabs that aren't currently owning the field, a
            // single Space starts owning it with the FIRST option. The
            // next Spaces walk through the options; when we reach the end
            // we clear the override (return to inheriting from the parent
            // tier) and the next Space starts the cycle over.
            if matches!(state.scope, ConfigScope::Repo | ConfigScope::Local) {
                let inherit_action = !active_owns_field;
                let mut should_clear = false;
                let next_value: Option<FieldValue> = match (field.kind, &current_value) {
                    _ if inherit_action => {
                        // Currently inherited — first Space starts owning at
                        // the first cyclable value (or toggles for Bool).
                        match (field.kind, &current_value) {
                            (FieldKind::Bool, FieldValue::Bool(b)) => Some(FieldValue::Bool(!b)),
                            (FieldKind::Enum { options }, _) => Some(FieldValue::Enum(options[0])),
                            (FieldKind::OptionalEnum { options }, _) => {
                                Some(FieldValue::OptionalEnum(Some(options[0])))
                            }
                            _ if field.toml_path == ["model", "model"] => {
                                cycle_to_next_registry_model(&state.effective, &current_value)
                            }
                            _ => None,
                        }
                    }
                    (FieldKind::Bool, FieldValue::Bool(true)) => {
                        // Last position of Bool — wrap to "inherit" by
                        // clearing the override.
                        should_clear = true;
                        None
                    }
                    (FieldKind::Bool, FieldValue::Bool(false)) => Some(FieldValue::Bool(true)),
                    (FieldKind::Enum { options }, FieldValue::Enum(current)) => {
                        let idx = options.iter().position(|o| *o == *current).unwrap_or(0);
                        if idx + 1 >= options.len() {
                            should_clear = true;
                            None
                        } else {
                            Some(FieldValue::Enum(options[idx + 1]))
                        }
                    }
                    (FieldKind::OptionalEnum { options }, FieldValue::OptionalEnum(current)) => {
                        let next_idx =
                            match current.and_then(|c| options.iter().position(|o| *o == c)) {
                                None => Some(0),
                                Some(i) if i + 1 < options.len() => Some(i + 1),
                                Some(_) => None,
                            };
                        match next_idx {
                            Some(i) => Some(FieldValue::OptionalEnum(Some(options[i]))),
                            None => {
                                should_clear = true;
                                None
                            }
                        }
                    }
                    _ if field.toml_path == ["model", "model"] => {
                        cycle_to_next_registry_model(&state.effective, &current_value)
                    }
                    _ => None,
                };
                if should_clear {
                    clear_scope_override_silent(state, agent, notifications);
                    return KeyOutcome::KeepOpen;
                }
                if let Some(next) = next_value {
                    if (field.set)(&mut state.effective, next.clone()).is_ok() {
                        state.dirty = true;
                        save_field_silent(state, agent, notifications, field, current_value, next);
                    }
                    return KeyOutcome::KeepOpen;
                }
                notifications.push(
                    format!("Space doesn't cycle {} — press Enter to edit.", field.label),
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }

            // User tab: cycle through values without an inherit position.
            let next: Option<FieldValue> = match (field.kind, &current_value) {
                (FieldKind::Bool, FieldValue::Bool(b)) => Some(FieldValue::Bool(!b)),
                (FieldKind::Enum { options }, FieldValue::Enum(current)) => {
                    let idx = options
                        .iter()
                        .position(|o| *o == *current)
                        .map(|i| (i + 1) % options.len())
                        .unwrap_or(0);
                    Some(FieldValue::Enum(options[idx]))
                }
                (FieldKind::OptionalEnum { options }, FieldValue::OptionalEnum(current)) => {
                    // None → options[0] → options[1] → ... → None
                    let next_idx = match current.and_then(|c| options.iter().position(|o| *o == c))
                    {
                        None => Some(0),
                        Some(i) if i + 1 < options.len() => Some(i + 1),
                        Some(_) => None,
                    };
                    Some(FieldValue::OptionalEnum(next_idx.map(|i| options[i])))
                }
                _ if field.toml_path == ["model", "model"] => {
                    cycle_to_next_registry_model(&state.effective, &current_value)
                }
                _ => None,
            };
            if let Some(next) = next {
                if (field.set)(&mut state.effective, next.clone()).is_ok() {
                    state.dirty = true;
                    save_field_silent(state, agent, notifications, field, current_value, next);
                }
            } else {
                notifications.push(
                    format!("Space doesn't cycle {} — press Enter to edit.", field.label),
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Enter, _) => {
            if state.current_section().id == SectionId::Themes {
                handle_theme_row_action(state, agent, notifications);
                return KeyOutcome::KeepOpen;
            }
            if state.on_synthetic_api_key_row() {
                open_api_key_entry_for_current_provider(state, notifications);
                return KeyOutcome::KeepOpen;
            }
            if let Some(action) = state.reset_action_at_row(state.field_index) {
                state.reset_confirm = Some(action.scope);
                return KeyOutcome::KeepOpen;
            }
            let field = state.current_field();
            // Refuse to edit env-shadowed fields — the value at runtime is
            // the env var's, not the TOML's, so a TOML write is silently
            // inert.
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to edit in the screen.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            // Read-only info rows (e.g. the Routing provider banner) have
            // nothing to edit.
            if matches!(field.kind, FieldKind::Info) {
                return KeyOutcome::KeepOpen;
            }
            // The model field opens a registry-driven picker; the per-provider
            // routing model fields stay free-text (a short id/alias like
            // "haiku" — and the picker's cross-provider switch would be wrong
            // for a provider-scoped routing model). Everything else uses the
            // regular per-kind editor.
            if field.toml_path == ["model", "model"] {
                // Look up by toml_path instead of the hard-coded
                // `CONFIG_SECTIONS[0].fields[0]` so reordering Models'
                // field list never silently retargets the provider value.
                let provider_field = CONFIG_SECTIONS
                    .iter()
                    .find(|s| s.id == squeezy_core::config_schema::SectionId::Models)
                    .and_then(|s| {
                        s.fields
                            .iter()
                            .find(|f| f.toml_path == ["model", "provider"])
                    })
                    .expect("Models section always exposes [model].provider");
                let current_provider = match (provider_field.get)(&state.effective) {
                    FieldValue::Enum(s) => s,
                    _ => "openai",
                };
                state.picker = Some(ModelPickerState {
                    filter: String::new(),
                    cursor: 0,
                    all_providers: false,
                    current_provider,
                });
            } else if matches!(field.kind, FieldKind::Secret { .. }) {
                notifications.push(
                    "Use `squeezy auth set <provider>` to write the secret.",
                    NotifySeverity::Info,
                );
            } else if matches!(
                field.kind,
                FieldKind::TableArray { .. } | FieldKind::ProviderSubTabs
            ) {
                notifications.push(
                    format!(
                        "{} is not yet editable in the screen — edit the TOML directly for now.",
                        field.label
                    ),
                    NotifySeverity::Info,
                );
            } else if matches!(field.kind, FieldKind::String { multiline: true }) {
                // Long paragraph fields (the judge prompt) open a full-screen
                // multi-line editor — the inline single-line caret overflows
                // the row and can't show the text being edited.
                let initial = match (field.get)(&state.effective) {
                    FieldValue::String(s) => s,
                    _ => String::new(),
                };
                state.prompt_editor = Some(PromptEditorState::new(initial));
            } else {
                state.editor = Some(open_editor_for(field, (field.get)(&state.effective)));
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('/'), m) if m.is_empty() => {
            state.search = Some(SearchOverlayState {
                query: String::new(),
                cursor: 0,
                matches: compute_search_matches(""),
            });
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('n'), m)
            if m.is_empty() && state.current_section().id == SectionId::Themes =>
        {
            open_theme_name_editor(state);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('r'), m)
            if m.is_empty() && state.current_section().id == SectionId::Themes =>
        {
            handle_theme_rename(state, notifications);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('d'), m)
            if m.is_empty() && state.current_section().id == SectionId::Themes =>
        {
            handle_theme_delete(state, agent, notifications);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
            if state.current_section().id == SectionId::Themes {
                handle_theme_clear(state, agent, notifications);
                return KeyOutcome::KeepOpen;
            }
            if state.on_synthetic_api_key_row() {
                notifications.push(
                    "API key has no default — use Enter / Space here to set, or clear it from the OS keychain manually.",
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }
            if state.on_reset_action_row() {
                notifications.push(
                    "Use Enter on the Reset row (with y/n confirm) to delete a tier file.",
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }
            let field = state.current_field();
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to reset.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            let default_val = (field.default)();
            let previous = (field.get)(&state.effective);
            if let Err(msg) = (field.set)(&mut state.effective, default_val.clone()) {
                notifications.push(format!("reset failed: {msg}"), NotifySeverity::Error);
            } else {
                state.dirty = true;
                save_field(state, agent, notifications, field, previous, default_val);
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            // /config saves on every commit (Enter / Space), so Ctrl+S
            // is a no-op affordance for muscle memory. Surface the same
            // message regardless of where the cursor sits so the user
            // doesn't think the screen swallowed the chord; mention
            // Ctrl+Z so they know an undo path exists.
            notifications.push(
                "Saves are automatic on Enter / Space. Ctrl+Z to revert the last write.",
                NotifySeverity::Info,
            );
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if state.current_section().id == SectionId::Themes {
                handle_theme_clear(state, agent, notifications);
                return KeyOutcome::KeepOpen;
            }
            if state.on_synthetic_api_key_row() || state.on_reset_action_row() {
                return KeyOutcome::KeepOpen;
            }
            match state.scope {
                ConfigScope::User => {
                    notifications.push(
                        "Ctrl+D clears Repo/Local overrides — switch to Repo or Local (Tab) first.",
                        NotifySeverity::Info,
                    );
                }
                ConfigScope::Repo | ConfigScope::Local => {
                    clear_scope_override(state, agent, notifications);
                }
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
            undo_last_write(state, agent, notifications);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('X'), m) if m == KeyModifiers::SHIFT || m.is_empty() => {
            // Wiping every save made this session is destructive — a
            // single stray Shift+X used to skip straight past the undo
            // stack. Arm a y/n confirmation overlay instead so the user
            // sees what they're about to lose.
            if state.undo_stack.is_empty() {
                notifications.push(
                    "Nothing to discard — no writes this session.",
                    NotifySeverity::Info,
                );
            } else {
                state.discard_confirm = true;
            }
            KeyOutcome::KeepOpen
        }
        _ => KeyOutcome::KeepOpen,
    }
}

fn handle_reset_confirm_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    let scope = state.reset_confirm.expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
            state.reset_confirm = None;
            perform_reset(state, agent, notifications, scope);
        }
        (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
            state.reset_confirm = None;
            notifications.push("Reset cancelled.", NotifySeverity::Info);
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn handle_discard_confirm_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
            state.discard_confirm = false;
            discard_all_session_writes(state, agent, notifications);
        }
        (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
            state.discard_confirm = false;
            notifications.push("Discard cancelled.", NotifySeverity::Info);
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

/// Full-screen multi-line editor for long String fields (the judge prompt).
/// Enter inserts a newline; Ctrl+S saves and closes; Esc cancels (discarding
/// edits, matching the inline editor). Clearing the buffer and saving reverts
/// the field to its built-in default (the per-provider setter stores `None`).
fn handle_prompt_editor_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            state.prompt_editor = None;
        }
        // Ctrl+S commits. (Ctrl+Enter is unreliable across terminals.)
        KeyCode::Char('s') if ctrl => {
            let value = state
                .prompt_editor
                .take()
                .map(|e| e.draft)
                .unwrap_or_default();
            let field = state.current_field();
            let previous = (field.get)(&state.effective);
            if let Err(msg) = (field.set)(&mut state.effective, FieldValue::String(value.clone())) {
                notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
            } else {
                state.dirty = true;
                save_field(
                    state,
                    agent,
                    notifications,
                    field,
                    previous,
                    FieldValue::String(value),
                );
            }
        }
        KeyCode::Enter => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.insert_char('\n');
            }
        }
        KeyCode::Char(c) if !ctrl => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.insert_char(c);
            }
        }
        KeyCode::Backspace => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.backspace();
            }
        }
        KeyCode::Delete => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.delete();
            }
        }
        KeyCode::Left => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.left();
            }
        }
        KeyCode::Right => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.right();
            }
        }
        KeyCode::Up => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.up();
            }
        }
        KeyCode::Down => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.down();
            }
        }
        KeyCode::Home => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.home();
            }
        }
        KeyCode::End => {
            if let Some(ed) = state.prompt_editor.as_mut() {
                ed.end();
            }
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn handle_picker_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    let picker = state.picker.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.picker = None;
        }
        (KeyCode::Tab, _) => {
            picker.all_providers = !picker.all_providers;
            picker.cursor = 0;
        }
        (KeyCode::Up, _) => {
            let n = picker_matches(picker).len();
            if n > 0 && picker.cursor > 0 {
                picker.cursor -= 1;
            }
        }
        (KeyCode::Down, _) => {
            let n = picker_matches(picker).len();
            if n > 0 {
                picker.cursor = (picker.cursor + 1).min(n - 1);
            }
        }
        (KeyCode::Backspace, _) => {
            picker.filter.pop();
            picker.cursor = 0;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            picker.filter.push(c);
            picker.cursor = 0;
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::CONTROL) => {
            // Ctrl+Enter — power-user escape hatch that commits the raw filter
            // even when it matches a known model. Most terminals strip the
            // Control modifier from Enter (only kitty-keyboard-protocol hosts
            // like Kitty/WezTerm/Ghostty deliver it), so the plain-Enter
            // branch below is the load-bearing path for custom ids.
            let custom = picker.filter.trim().to_string();
            if custom.is_empty() {
                notifications.push(
                    "Type a model id first, then press Enter to commit.",
                    NotifySeverity::Info,
                );
            } else {
                commit_model_picker(state, agent, notifications, custom);
            }
        }
        (KeyCode::Enter, _) => {
            let matches = picker_matches(picker);
            if matches.is_empty() {
                let custom = picker.filter.trim().to_string();
                if custom.is_empty() {
                    notifications.push(
                        "Type a model id first, then press Enter to commit.",
                        NotifySeverity::Info,
                    );
                } else {
                    commit_model_picker(state, agent, notifications, custom);
                }
            } else {
                let id = matches[picker.cursor.min(matches.len() - 1)].id.to_string();
                commit_model_picker(state, agent, notifications, id);
            }
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn commit_model_picker(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    model_id: String,
) {
    state.picker = None;
    // Look the provider field up by SectionId/toml_path instead of the
    // brittle `CONFIG_SECTIONS[0].fields[0]` index — if Models ever
    // grew a field before `provider`, the old code would silently
    // overwrite something else.
    let provider_field = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == squeezy_core::config_schema::SectionId::Models)
        .and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.toml_path == ["model", "provider"])
        })
        .expect("Models section always exposes [model].provider");
    let current_provider = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        _ => "openai",
    };
    let previous_provider_value = (provider_field.get)(&state.effective);
    let previous_model_value = (model_field_meta().get)(&state.effective);
    let picked_provider = squeezy_llm::MODEL_REGISTRY
        .iter()
        .find(|m| m.id == model_id)
        .map(|m| m.provider);
    let model_field = model_field_meta();
    let model_value = FieldValue::String(model_id.clone());
    if let Some(new_provider) = picked_provider
        && new_provider != current_provider
    {
        // Apply the provider swap in memory first — `set_provider` will
        // reset `cfg.model` to that provider's default, then we overwrite
        // with the picked id.
        if let Err(msg) = (provider_field.set)(&mut state.effective, FieldValue::Enum(new_provider))
        {
            notifications.push(format!("invalid provider: {msg}"), NotifySeverity::Error);
            return;
        }
        if let Err(msg) = (model_field.set)(&mut state.effective, model_value.clone()) {
            notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
            return;
        }
        state.dirty = true;
        // `save_field_inner` for `[model].provider` already chains a
        // second SettingsEdit that persists the current `[model].model`
        // value in the same write. Issuing a separate `save_field` for
        // the model field used to add a second tier-file write, a
        // second undo-stack entry, and a second "saved …" notification
        // for one user pick. Trust the chained edit.
        save_field(
            state,
            agent,
            notifications,
            provider_field,
            previous_provider_value,
            FieldValue::Enum(new_provider),
        );
    } else {
        if let Err(msg) = (model_field.set)(&mut state.effective, model_value.clone()) {
            notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
            return;
        }
        state.dirty = true;
        save_field(
            state,
            agent,
            notifications,
            model_field,
            previous_model_value,
            model_value,
        );
    }
}

fn handle_theme_row_action(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    match state.theme_row_at(state.field_index) {
        Some(ThemeRow::Theme(name)) => {
            save_theme_selection(state, agent, notifications, name);
        }
        Some(ThemeRow::New) => open_theme_name_editor(state),
        Some(ThemeRow::Color(token)) => {
            let theme = state.effective.tui.theme.clone();
            let [r, g, b] = crate::render::theme::resolve_theme(&state.effective, &theme)
                .resolve(token)
                .unwrap_or_else(|| crate::render::theme::rgb(token));
            let draft = format!("{r},{g},{b}");
            state.theme_editor = Some(ThemeEditor::Rgb {
                theme,
                token,
                cursor: draft.chars().count(),
                draft,
            });
        }
        None => {}
    }
}

fn handle_theme_clear(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    match state.theme_row_at(state.field_index) {
        Some(ThemeRow::Color(token)) => {
            let theme = state.effective.tui.theme.clone();
            unset_theme_color(state, agent, notifications, theme, token.to_string());
        }
        Some(ThemeRow::Theme(_)) | Some(ThemeRow::New) | None => {
            notifications.push(
                "Move to a color row to clear that RGB override.",
                NotifySeverity::Info,
            );
        }
    }
}

fn handle_theme_rename(state: &mut ConfigScreenState, notifications: &mut ConfigFeedback) {
    match state.theme_row_at(state.field_index) {
        Some(ThemeRow::Theme(name)) => open_theme_rename_editor(state, notifications, name),
        Some(ThemeRow::Color(_)) => {
            notifications.push(
                "Move to a custom theme row to rename it.",
                NotifySeverity::Info,
            );
        }
        Some(ThemeRow::New) | None => {
            notifications.push(
                "Press n or Enter here to create a theme.",
                NotifySeverity::Info,
            );
        }
    }
}

fn handle_theme_delete(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    match state.theme_row_at(state.field_index) {
        Some(ThemeRow::Theme(name)) => {
            if is_builtin_tui_theme_name(&name) {
                notifications.push(
                    "Built-in themes cannot be deleted. Press n to create an editable copy.",
                    NotifySeverity::Info,
                );
                return;
            }
            save_theme_delete(state, agent, notifications, name);
        }
        Some(ThemeRow::Color(_)) => {
            notifications.push(
                "Move to a custom theme row to delete it. Ctrl+R clears the selected color override.",
                NotifySeverity::Info,
            );
        }
        Some(ThemeRow::New) | None => {
            notifications.push(
                "Move to a custom theme row to delete it.",
                NotifySeverity::Info,
            );
        }
    }
}

fn open_theme_name_editor(state: &mut ConfigScreenState) {
    let draft = next_theme_name(state);
    state.theme_editor = Some(ThemeEditor::Name {
        cursor: draft.chars().count(),
        draft,
    });
}

fn open_theme_rename_editor(
    state: &mut ConfigScreenState,
    notifications: &mut ConfigFeedback,
    name: String,
) {
    if is_builtin_tui_theme_name(&name) {
        notifications.push(
            "Built-in themes cannot be renamed. Press n to create an editable copy.",
            NotifySeverity::Info,
        );
        return;
    }
    let cursor = name.chars().count();
    state.theme_editor = Some(ThemeEditor::Rename {
        original: name.clone(),
        draft: name,
        cursor,
    });
}

fn next_theme_name(state: &ConfigScreenState) -> String {
    for i in 1..1000 {
        let candidate = if i == 1 {
            "custom-theme".to_string()
        } else {
            format!("custom-theme-{i}")
        };
        if !crate::render::theme::theme_exists(&state.effective, &candidate) {
            return candidate;
        }
    }
    "custom-theme".to_string()
}

fn handle_theme_editor_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    if key.code == KeyCode::Esc {
        state.theme_editor = None;
        return KeyOutcome::KeepOpen;
    }

    if key.code == KeyCode::Enter {
        commit_theme_editor(state, agent, notifications);
        return KeyOutcome::KeepOpen;
    }

    if let Some(editor) = state.theme_editor.as_mut() {
        match editor {
            ThemeEditor::Name { draft, cursor } | ThemeEditor::Rename { draft, cursor, .. } => {
                edit_theme_text(draft, cursor, key, |c| {
                    c.is_ascii_alphanumeric() || c == '-' || c == '_'
                });
            }
            ThemeEditor::Rgb { draft, cursor, .. } => {
                edit_theme_text(draft, cursor, key, |c| {
                    c.is_ascii_digit() || c == ',' || c == ' '
                });
            }
        }
    }
    KeyOutcome::KeepOpen
}

fn commit_theme_editor(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    let Some(editor) = state.theme_editor.take() else {
        return;
    };
    match editor {
        ThemeEditor::Name { draft, .. } => {
            let Some(name) = normalize_tui_theme_name(&draft) else {
                notifications.push("Invalid theme name.", NotifySeverity::Error);
                return;
            };
            if crate::render::theme::theme_exists(&state.effective, &name) {
                notifications.push(
                    format!("Theme {name} already exists."),
                    NotifySeverity::Warn,
                );
                return;
            }
            save_theme_snapshot(state, agent, notifications, name);
        }
        ThemeEditor::Rename {
            original, draft, ..
        } => {
            let Some(name) = normalize_tui_theme_name(&draft) else {
                notifications.push("Invalid theme name.", NotifySeverity::Error);
                return;
            };
            if name == original {
                notifications.push("Theme name unchanged.", NotifySeverity::Info);
                return;
            }
            if is_builtin_tui_theme_name(&name)
                || crate::render::theme::theme_exists(&state.effective, &name)
            {
                notifications.push(
                    format!("Theme {name} already exists."),
                    NotifySeverity::Warn,
                );
                return;
            }
            save_theme_rename(state, agent, notifications, original, name);
        }
        ThemeEditor::Rgb {
            theme,
            token,
            draft,
            ..
        } => {
            if !is_tui_theme_color_token(token) {
                notifications.push(
                    format!("Unknown theme token {token}."),
                    NotifySeverity::Error,
                );
                return;
            }
            let Some(rgb) = parse_rgb_draft(&draft) else {
                notifications.push(
                    "RGB must be three values from 0 to 255.",
                    NotifySeverity::Error,
                );
                return;
            };
            save_theme_color(state, agent, notifications, theme, token.to_string(), rgb);
        }
    }
}

fn edit_theme_text(
    draft: &mut String,
    cursor: &mut usize,
    key: KeyEvent,
    allow: impl Fn(char) -> bool,
) {
    match key.code {
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && allow(c) =>
        {
            let mut chars: Vec<char> = draft.chars().collect();
            let idx = (*cursor).min(chars.len());
            chars.insert(idx, c);
            *draft = chars.into_iter().collect();
            *cursor = idx + 1;
        }
        KeyCode::Backspace if *cursor > 0 => {
            let mut chars: Vec<char> = draft.chars().collect();
            let idx = (*cursor - 1).min(chars.len().saturating_sub(1));
            chars.remove(idx);
            *draft = chars.into_iter().collect();
            *cursor -= 1;
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(draft.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = draft.chars().count(),
        _ => {}
    }
}

fn parse_rgb_draft(draft: &str) -> Option<[u8; 3]> {
    let mut parts = draft.split(',').map(str::trim);
    let rgb = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    if parts.next().is_some() {
        return None;
    }
    Some(rgb)
}

fn open_api_key_entry_for_current_provider(
    state: &mut ConfigScreenState,
    notifications: &mut ConfigFeedback,
) {
    match provider_api_key_env(&state.effective.provider) {
        Some((label, env_var)) => {
            // Pre-fill from the inline `api_key` already present in the
            // merged TOML (user + repo + local) so reopening the field
            // shows the saved value as •••• and Ctrl+T can reveal it.
            // We deliberately do not consult env vars here: the field is
            // labelled as the TOML-stored secret, and a stale env value
            // would mis-suggest that pressing Enter would overwrite it.
            let draft = provider_inline_api_key(&state.effective.provider).unwrap_or_default();
            let cursor = draft.chars().count();
            state.secret_entry = Some(SecretEntryState {
                env_var,
                provider_label: label.to_string(),
                draft,
                cursor,
                reveal: false,
            });
        }
        None => {
            notifications.push(
                "This provider does not have a simple API-key env var \
                 (Bedrock uses AWS creds, Ollama is local).",
                NotifySeverity::Info,
            );
        }
    }
}

fn handle_secret_entry_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    let entry = state.secret_entry.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            entry.wipe();
            state.secret_entry = None;
        }
        (KeyCode::F(2), _) => {
            entry.reveal = !entry.reveal;
        }
        (KeyCode::Backspace, _) => {
            entry.backspace();
        }
        (KeyCode::Left, _) => {
            entry.cursor = entry.cursor.saturating_sub(1);
        }
        (KeyCode::Right, _) => {
            entry.cursor = (entry.cursor + 1).min(entry.char_len());
        }
        (KeyCode::Home, _) => entry.cursor = 0,
        (KeyCode::End, _) => entry.cursor = entry.char_len(),
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            entry.insert_char(c);
        }
        (KeyCode::Enter, _) => {
            let env_var = entry.env_var.clone();
            let value = std::mem::take(&mut entry.draft);
            entry.cursor = 0;
            state.secret_entry = None;
            if value.trim().is_empty() {
                notifications.push("API key was empty — nothing written.", NotifySeverity::Warn);
                return KeyOutcome::KeepOpen;
            }
            let Some(section) = provider_section_name(&state.effective.provider) else {
                notifications.push(
                    "This provider does not have a single inline API key (Bedrock \
                     uses AWS creds, Ollama is local).",
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            };
            save_inline_provider_api_key(
                state,
                agent,
                notifications,
                section,
                &env_var,
                value.trim(),
            );
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

/// Dispatch a `crossterm::Event::Paste` payload while the config screen is
/// open. Bracketed paste arrives as one `Event::Paste(text)` — not as a
/// stream of `KeyEvent::Char` events — so without this hook the active
/// input field never sees the pasted text. Routes to whichever sub-mode
/// owns the cursor (secret entry, search, picker, or field editor).
pub(crate) fn handle_paste(state: &mut ConfigScreenState, text: &str) {
    // Most config inputs are single-line scalars; take only the first line
    // so a stray trailing newline from the clipboard does not commit early
    // or look garbled in the masked secret entry.
    let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
    let line = match cleaned.lines().next() {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    if let Some(entry) = state.secret_entry.as_mut() {
        for c in line.chars() {
            entry.insert_char(c);
        }
        return;
    }

    if let Some(editor) = state.theme_editor.as_mut() {
        match editor {
            ThemeEditor::Name { draft, cursor } | ThemeEditor::Rename { draft, cursor, .. } => {
                insert_chars_at(
                    draft,
                    cursor,
                    line.chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_'),
                );
            }
            ThemeEditor::Rgb { draft, cursor, .. } => {
                insert_chars_at(
                    draft,
                    cursor,
                    line.chars()
                        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == ' '),
                );
            }
        }
        return;
    }

    if let Some(search) = state.search.as_mut() {
        search.query.push_str(line);
        search.matches = compute_search_matches(&search.query);
        search.cursor = 0;
        return;
    }

    if let Some(picker) = state.picker.as_mut() {
        picker.filter.push_str(line);
        picker.cursor = 0;
        return;
    }

    if let Some(editor) = state.editor.as_mut() {
        insert_into_editor(editor, line);
    }
}

fn insert_into_editor(editor: &mut FieldEditor, text: &str) {
    match editor {
        FieldEditor::Text { draft, cursor }
        | FieldEditor::StringList { draft, cursor }
        | FieldEditor::Path { draft, cursor } => {
            insert_chars_at(draft, cursor, text.chars());
        }
        FieldEditor::Integer { draft, cursor, .. }
        | FieldEditor::OptionalInteger { draft, cursor, .. }
        | FieldEditor::Duration { draft, cursor } => {
            insert_chars_at(
                draft,
                cursor,
                text.chars().filter(|c| c.is_ascii_digit() || *c == '-'),
            );
        }
        FieldEditor::Enum { .. } | FieldEditor::OptionalEnum { .. } | FieldEditor::Bool(_) => {}
    }
}

fn insert_chars_at(draft: &mut String, cursor: &mut usize, chars: impl IntoIterator<Item = char>) {
    let mut existing: Vec<char> = draft.chars().collect();
    for c in chars {
        if *cursor > existing.len() {
            *cursor = existing.len();
        }
        existing.insert(*cursor, c);
        *cursor += 1;
    }
    *draft = existing.into_iter().collect();
}

fn handle_search_key(state: &mut ConfigScreenState, key: KeyEvent) -> KeyOutcome {
    let search = state.search.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.search = None;
        }
        (KeyCode::Up, _) if !search.matches.is_empty() && search.cursor > 0 => {
            search.cursor -= 1;
        }
        (KeyCode::Down, _) => {
            let n = search.matches.len();
            if n > 0 {
                search.cursor = (search.cursor + 1).min(n - 1);
            }
        }
        (KeyCode::Backspace, _) => {
            search.query.pop();
            search.matches = compute_search_matches(&search.query);
            search.cursor = 0;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            search.query.push(c);
            search.matches = compute_search_matches(&search.query);
            search.cursor = 0;
        }
        (KeyCode::Enter, _) => {
            if let Some((sidx, fidx, _)) = search.matches.get(search.cursor).copied() {
                state.section_index = sidx;
                let section = &CONFIG_SECTIONS[sidx];
                state.field_index = if section.id
                    == squeezy_core::config_schema::SectionId::Permissions
                    && state.effective.permissions.mode
                        != squeezy_core::PermissionPolicyMode::Custom
                {
                    0
                } else {
                    // `field_index` is a display-row index; translate the raw
                    // matched field index through the section's synthetic-row
                    // layout so Models fields at/after the API-key row resolve
                    // back to the intended field.
                    ConfigScreenState::display_row_for_field(section, fidx)
                };
            }
            state.search = None;
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

/// Handle keys on the `/mcp` page browse mode. Returns `Some(outcome)`
/// when the key was consumed (e/r/a/d/Enter on a server row, plus
/// session-only variants); `None` lets the caller fall through to the
/// regular browse keymap so arrows/Tab keep working.
fn handle_mcp_browse_key(
    state: &mut ConfigScreenState,
    notifications: &mut super::ConfigFeedback,
    key: KeyEvent,
) -> Option<KeyOutcome> {
    use super::{McpAction, McpAddForm};
    let names = state.mcp_server_names();
    let add_row = names.len();
    let is_add_row = state.field_index == add_row;
    let server_at_focus = state.mcp_server_at_row(state.field_index);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // Treat both `'A'` and `shift+a` as the shifted variant. Some
    // terminals deliver shift+letter as `Char('A')` with `SHIFT`
    // set, others as `Char('A')` with no modifier, and a few as
    // `Char('a')` with `SHIFT`. We accept all three so the
    // documented session-only modifier never silently no-ops.
    let session_only_add = matches!(key.code, KeyCode::Char('A')) || shift;
    match (key.code, key.modifiers) {
        // Open the "add server" overlay. Lower-case `a` persists by
        // default; upper-case `A` (or shift+a) flips to session-only.
        (KeyCode::Char('a'), _) | (KeyCode::Char('A'), _) => {
            state.mcp_add = Some(McpAddForm {
                session_only: session_only_add,
                ..McpAddForm::default()
            });
            Some(KeyOutcome::KeepOpen)
        }
        // Toggle enabled. Enter / Space / e / E: persist; Shift-e
        // (or capital E) flips to session-only. Space is aliased to
        // the toggle action both because it matches the user's
        // intuition (Space cycles bool values throughout `/config`)
        // and because the global Space handler would otherwise call
        // `current_field()`, which panics on the McpServers section
        // — the page renders custom rows with no `FieldMeta`
        // backing.
        (KeyCode::Enter, _)
        | (KeyCode::Char(' '), _)
        | (KeyCode::Char('e'), _)
        | (KeyCode::Char('E'), _) => {
            if is_add_row {
                state.mcp_add = Some(McpAddForm::default());
                return Some(KeyOutcome::KeepOpen);
            }
            let Some((name, server)) = server_at_focus else {
                notifications.push("no MCP server at this row", NotifySeverity::Warn);
                return Some(KeyOutcome::KeepOpen);
            };
            let persist = !shift && !matches!(key.code, KeyCode::Char('E'));
            let next_enabled = !server.enabled;
            let label = if next_enabled { "enable" } else { "disable" };
            // Apply optimistically to the cached snapshot so the row
            // flips immediately; the host's async dispatch will refresh
            // from the agent once the registry confirms.
            if let Some(entry) = state.mcp_servers.get_mut(&name) {
                entry.enabled = next_enabled;
            }
            state.mcp_pending_actions.push(McpAction::Toggle {
                server: name.clone(),
                enabled: next_enabled,
                persist,
            });
            state.mcp_last_status_line = Some(format!(
                "{label} {name}{}",
                if persist { "" } else { " (session-only)" }
            ));
            notifications.push(
                format!(
                    "{} {name}{}",
                    label,
                    if persist { "" } else { " — session-only" }
                ),
                NotifySeverity::Info,
            );
            Some(KeyOutcome::KeepOpen)
        }
        // Restart in place.
        (KeyCode::Char('r'), _) | (KeyCode::Char('R'), _) => {
            if is_add_row {
                return Some(KeyOutcome::KeepOpen);
            }
            let Some((name, _)) = server_at_focus else {
                notifications.push("no MCP server at this row", NotifySeverity::Warn);
                return Some(KeyOutcome::KeepOpen);
            };
            state.mcp_pending_actions.push(McpAction::Restart {
                server: name.clone(),
            });
            state.mcp_last_status_line = Some(format!("restarting {name}"));
            notifications.push(format!("restarting {name}"), NotifySeverity::Info);
            Some(KeyOutcome::KeepOpen)
        }
        // Stage a delete with y/n confirmation.
        (KeyCode::Char('d'), _) | (KeyCode::Char('x'), _) | (KeyCode::Char('D'), _) => {
            if is_add_row {
                return Some(KeyOutcome::KeepOpen);
            }
            let Some((name, _)) = server_at_focus else {
                return Some(KeyOutcome::KeepOpen);
            };
            state.mcp_pending_delete = Some(name);
            Some(KeyOutcome::KeepOpen)
        }
        // Navigation keys (arrows, Tab, BackTab, Esc, Home/End,
        // PageUp/Down, and the Ctrl-hjkl vim variants) fall through
        // to the global keymap so the user can still move between
        // rows, sections, and scope tabs from the `/mcp` page.
        //
        // Everything else is intentionally absorbed: the global
        // browse handler reaches for `state.current_field()` in
        // multiple arms (Space cycling, Enter editing, Ctrl+R
        // reset), and `current_field` panics when `field_at_row`
        // returns `None` — which it does for every row in the
        // McpServers section. Letting an unhandled key fall through
        // would crash the whole TUI.
        _ if is_mcp_navigation_key(&key) => None,
        _ => Some(KeyOutcome::KeepOpen),
    }
}

fn is_mcp_navigation_key(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Esc
    ) || matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('h' | 'j' | 'k' | 'l'), KeyModifiers::CONTROL,),
    )
}

/// Handle keys while the "remove server" confirmation is open.
fn handle_mcp_delete_confirm_key(
    state: &mut ConfigScreenState,
    notifications: &mut super::ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    use super::McpAction;
    let Some(name) = state.mcp_pending_delete.clone() else {
        return KeyOutcome::KeepOpen;
    };
    let confirm_persist = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
    let confirm_session = matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'));
    let cancel = matches!(
        key.code,
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N')
    );
    if confirm_persist || confirm_session {
        state.mcp_servers.remove(&name);
        state.mcp_pending_delete = None;
        let persist = confirm_persist;
        state.mcp_pending_actions.push(McpAction::Remove {
            name: name.clone(),
            persist,
        });
        state.mcp_last_status_line = Some(format!(
            "removed {name}{}",
            if persist { "" } else { " (session-only)" }
        ));
        // Cap the focus so we don't dangle past the trimmed row count.
        let max_row = state.row_count().saturating_sub(1);
        if state.field_index > max_row {
            state.field_index = max_row;
        }
        notifications.push(
            format!(
                "removed {name}{}",
                if persist { "" } else { " — session-only" }
            ),
            NotifySeverity::Info,
        );
        return KeyOutcome::KeepOpen;
    }
    if cancel {
        state.mcp_pending_delete = None;
        return KeyOutcome::KeepOpen;
    }
    KeyOutcome::KeepOpen
}

/// Handle keys while the "add server" overlay is open.
fn handle_mcp_add_form_key(
    state: &mut ConfigScreenState,
    notifications: &mut super::ConfigFeedback,
    key: KeyEvent,
) -> KeyOutcome {
    use super::{MCP_ADD_FIELD_COUNT, McpAction, McpAddTransport};
    let Some(form) = state.mcp_add.as_mut() else {
        return KeyOutcome::KeepOpen;
    };
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.mcp_add = None;
        }
        (KeyCode::Tab, _) => {
            form.session_only = !form.session_only;
        }
        (KeyCode::Up, _) => {
            if form.field_index == 0 {
                form.field_index = MCP_ADD_FIELD_COUNT - 1;
            } else {
                form.field_index -= 1;
            }
        }
        (KeyCode::Down, _) => {
            form.field_index = (form.field_index + 1) % MCP_ADD_FIELD_COUNT;
        }
        (KeyCode::Char(' '), KeyModifiers::NONE) if form.field_index == 1 => {
            form.transport = form.transport.next();
        }
        (KeyCode::Backspace, _) => {
            let target = active_add_field(form);
            target.pop();
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT)
            if form.field_index != 1 =>
        {
            let target = active_add_field(form);
            target.push(c);
        }
        (KeyCode::Enter, _) => {
            // Validate + stage. We rebuild a fresh `McpAddForm` if
            // validation fails so the error line shows above the form.
            let trimmed_name = form.name.trim().to_string();
            if trimmed_name.is_empty() {
                form.error = Some("name is required".to_string());
                return KeyOutcome::KeepOpen;
            }
            if state.mcp_servers.contains_key(&trimmed_name) {
                form.error = Some(format!("server {trimmed_name:?} already exists"));
                return KeyOutcome::KeepOpen;
            }
            let transport = form.transport;
            let command = form.command.trim().to_string();
            let url = form.url.trim().to_string();
            match transport {
                McpAddTransport::Stdio if command.is_empty() => {
                    form.error = Some("stdio transport needs a command".to_string());
                    return KeyOutcome::KeepOpen;
                }
                McpAddTransport::Http | McpAddTransport::Sse if url.is_empty() => {
                    form.error = Some(format!("{} transport needs a url", transport.as_str()));
                    return KeyOutcome::KeepOpen;
                }
                _ => {}
            }
            let server = squeezy_core::McpServerConfig {
                enabled: true,
                transport: match transport {
                    McpAddTransport::Stdio => squeezy_core::McpTransport::Stdio,
                    McpAddTransport::Http => squeezy_core::McpTransport::Http,
                    McpAddTransport::Sse => squeezy_core::McpTransport::Sse,
                },
                command: (!command.is_empty()).then_some(command),
                args: Vec::new(),
                url: (!url.is_empty()).then_some(url),
                timeout_ms: None,
                discovery_timeout_ms: None,
                tool_call_timeout_ms: None,
                enabled_tools: None,
                disabled_tools: Vec::new(),
                env: std::collections::BTreeMap::new(),
                permissions: squeezy_core::McpPermissionConfig::default(),
                bearer_token_env_var: None,
                http_headers: std::collections::BTreeMap::new(),
                env_http_headers: std::collections::BTreeMap::new(),
            };
            let persist = !form.session_only;
            state
                .mcp_servers
                .insert(trimmed_name.clone(), server.clone());
            state.mcp_pending_actions.push(McpAction::Add {
                name: trimmed_name.clone(),
                server: Box::new(server),
                persist,
            });
            state.mcp_last_status_line = Some(format!(
                "added {trimmed_name}{}",
                if persist { "" } else { " (session-only)" }
            ));
            // Focus the newly-added row (alphabetical order).
            let names = state.mcp_server_names();
            if let Some(idx) = names.iter().position(|n| n == &trimmed_name) {
                state.field_index = idx;
            }
            state.mcp_add = None;
            notifications.push(
                format!(
                    "added {trimmed_name}{}",
                    if persist { "" } else { " — session-only" }
                ),
                NotifySeverity::Info,
            );
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn active_add_field(form: &mut super::McpAddForm) -> &mut String {
    match form.field_index {
        0 => &mut form.name,
        2 => &mut form.command,
        3 => &mut form.url,
        // The transport row uses Space to cycle (not free-form
        // input); fall back to `name` defensively so the helper
        // never returns a dangling reference.
        _ => &mut form.name,
    }
}
