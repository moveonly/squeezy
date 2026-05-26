use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use squeezy_agent::Agent;
use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldKind, FieldValue};

use super::{
    ConfigScope, ConfigScreenState, EditorOutcome, FieldEditor, KeyOutcome, ModelPickerState,
    SearchOverlayState, SecretEntryState, clear_scope_override, clear_scope_override_silent,
    compute_search_matches, cycle_to_next_registry_model, discard_all_session_writes,
    handle_editor_key, model_field_meta, open_editor_for, perform_reset, picker_matches,
    provider_api_key_env, provider_inline_api_key, provider_section_name, save_field,
    save_field_silent, save_inline_provider_api_key, undo_last_write,
};
use crate::notification::{NotificationQueue, Severity as NotifySeverity};

pub(crate) fn handle_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    // Sub-modes take precedence over the regular browse keymap.
    if state.reset_confirm.is_some() {
        return handle_reset_confirm_key(state, agent, notifications, key);
    }
    if state.secret_entry.is_some() {
        return handle_secret_entry_key(state, agent, notifications, key);
    }
    if state.search.is_some() {
        return handle_search_key(state, key);
    }
    if state.picker.is_some() {
        return handle_picker_key(state, agent, notifications, key);
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
                if let Err(msg) = (field.set)(&mut state.effective, value.clone()) {
                    notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
                } else {
                    state.dirty = true;
                    // Save immediately; the apply pipeline below routes the
                    // change to the right tier and queues notifications.
                    save_field(state, agent, notifications, field, value);
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
                    clear_scope_override_silent(state, notifications);
                    return KeyOutcome::KeepOpen;
                }
                if let Some(next) = next_value {
                    if (field.set)(&mut state.effective, next.clone()).is_ok() {
                        state.dirty = true;
                        save_field_silent(state, agent, notifications, field, next);
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
                    save_field_silent(state, agent, notifications, field, next);
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
            // The model field opens a registry-driven picker; every other
            // field goes through the regular per-kind editor.
            if field.toml_path == ["model", "model"] {
                let current_provider = match (CONFIG_SECTIONS[0].fields[0].get)(&state.effective) {
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
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
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
            if let Err(msg) = (field.set)(&mut state.effective, default_val.clone()) {
                notifications.push(format!("reset failed: {msg}"), NotifySeverity::Error);
            } else {
                state.dirty = true;
                save_field(state, agent, notifications, field, default_val);
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            // The current design saves on every commit (Enter / Space), so
            // Ctrl+S is a no-op affordance for muscle memory.
            notifications.push(
                "Changes auto-save on commit (Enter / Space).",
                NotifySeverity::Info,
            );
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
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
                    clear_scope_override(state, notifications);
                }
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
            undo_last_write(state, agent, notifications);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('X'), m) if m == KeyModifiers::SHIFT || m.is_empty() => {
            discard_all_session_writes(state, agent, notifications);
            KeyOutcome::KeepOpen
        }
        _ => KeyOutcome::KeepOpen,
    }
}

fn handle_reset_confirm_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
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

fn handle_picker_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
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
            // Ctrl+Enter — commit the raw filter buffer as a custom string.
            let custom = picker.filter.trim().to_string();
            if custom.is_empty() {
                notifications.push(
                    "Type a model id first, then Ctrl+Enter to commit.",
                    NotifySeverity::Info,
                );
            } else {
                commit_model_picker(state, agent, notifications, custom);
            }
        }
        (KeyCode::Enter, _) => {
            let matches = picker_matches(picker);
            if matches.is_empty() {
                notifications.push(
                    "No model matches the filter. Ctrl+Enter to commit the filter as a custom id.",
                    NotifySeverity::Info,
                );
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
    notifications: &mut NotificationQueue,
    model_id: String,
) {
    state.picker = None;
    // If the chosen id belongs to a different provider in the registry
    // (only possible when the picker was in `all_providers` mode), swap
    // the provider field first. This keeps the on-disk pair consistent
    // and avoids the symmetric bug we just guarded for the Space-cycle
    // path — a registry lookup that fails (custom Ctrl+Enter id) simply
    // skips the provider swap, since we can't infer the provider.
    let provider_field = &CONFIG_SECTIONS[0].fields[0];
    let current_provider = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        _ => "openai",
    };
    let picked_provider = squeezy_llm::MODEL_REGISTRY
        .iter()
        .find(|m| m.id == model_id)
        .map(|m| m.provider);
    if let Some(new_provider) = picked_provider
        && new_provider != current_provider
        && let Err(msg) = (provider_field.set)(&mut state.effective, FieldValue::Enum(new_provider))
    {
        notifications.push(format!("invalid provider: {msg}"), NotifySeverity::Error);
        return;
    }
    // After a provider swap `set_provider` rewrote `cfg.model` to that
    // provider's default; overwrite it again with the user's chosen id
    // and save through the regular pipeline, which now persists
    // (provider, model) together.
    let model_field = model_field_meta();
    let model_value = FieldValue::String(model_id);
    if let Err(msg) = (model_field.set)(&mut state.effective, model_value.clone()) {
        notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
        return;
    }
    state.dirty = true;
    if let Some(new_provider) = picked_provider
        && new_provider != current_provider
    {
        // Persist the provider swap first (which chains the model write
        // via the provider→model pairing in `save_field_inner`), then
        // persist the explicit model id the user picked.
        save_field(
            state,
            agent,
            notifications,
            provider_field,
            FieldValue::Enum(new_provider),
        );
    }
    save_field(state, agent, notifications, model_field, model_value);
}

fn open_api_key_entry_for_current_provider(
    state: &mut ConfigScreenState,
    notifications: &mut NotificationQueue,
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
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    let entry = state.secret_entry.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            entry.wipe();
            state.secret_entry = None;
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
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
                state.field_index = fidx;
            }
            state.search = None;
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}
