use std::collections::BTreeMap;

use std::sync::Arc;

use squeezy_agent::{Agent, PendingConfigSwap};
use squeezy_core::{
    AppConfig, TuiThemeSettings,
    config_schema::{ApplyTier, FieldMeta, FieldValue, SectionId},
    load_separated_settings_sources,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, WriteOutcome, apply_edits},
};

use super::{
    ConfigFeedback, ConfigScope, ConfigScreenState, ConfigTelemetryChange,
    ConfigTelemetryChangeKind, Severity as NotifySeverity, model_field_meta,
    provider_variant_label, tier_path,
};

/// Persist an inline `[providers.<section>] api_key = "..."` into the active
/// scope's TOML and rebuild the provider client so the next turn picks it
/// up. The dynamic section name (e.g. `portkey`, `openai`) precludes the
/// regular `FieldMeta::toml_path` (`&'static [&'static str]`) flow, so we
/// drive `apply_edits` directly via `SetTableEntry` which already accepts
/// an owned `key: String`. Refuses Repo scope to keep secrets out of the
/// committed `squeezy.toml`.
pub(crate) fn save_inline_provider_api_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    section: &str,
    env_var: &str,
    value: &str,
) {
    let (target_path, scope_target) = match state.scope {
        ConfigScope::User => {
            let p = state.sources.user_path_default.clone();
            (p.clone(), SettingsScope::user(&p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(&p))
        }
        ConfigScope::Repo => {
            notifications.push(
                format!(
                    "Refused: API keys never go into the committed squeezy.toml. \
                     Switch the scope tab to Local or User and try again \
                     (env var: {env_var})."
                ),
                NotifySeverity::Warn,
            );
            return;
        }
    };

    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: section.to_string(),
            fields: vec![("api_key", EditOp::SetString(value.to_string()))],
        },
    };

    let outcome = match apply_edits(&scope_target, &[edit]) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };
    state.note_session_write(&outcome.path);

    // Update the in-memory effective config so the pre-fill on a reopen of
    // the secret entry sees what we just wrote, and so the provider
    // rebuild below picks up the new value without re-reading the file.
    set_effective_provider_api_key(&mut state.effective, value);

    if let Ok(reloaded) = squeezy_core::load_separated_settings_sources() {
        state.sources = reloaded;
    }

    // Build the new provider eagerly off the tokio runtime — mirrors the
    // pattern in `save_field_inner` since `provider_from_config` may do
    // blocking secret-store I/O depending on the platform.
    let provider_cfg = state.effective.provider.clone();
    let new_provider = std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg))
        .join()
        .ok()
        .and_then(|r| r.ok());
    if new_provider.is_none() {
        notifications.push(
            format!(
                "Saved {} to {}, but the provider failed to build with the new key.",
                env_var,
                outcome.path.display()
            ),
            NotifySeverity::Error,
        );
        return;
    }

    state.effective.refresh_config_warnings();
    agent.arm_config_swap(PendingConfigSwap {
        config: state.effective.clone(),
        provider: new_provider,
        display_note: Some(format!(
            "{} key updated",
            provider_variant_label(&state.effective.provider)
        )),
    });

    notifications.push(
        format!("✓ saved {} key to {}", section, outcome.path.display()),
        NotifySeverity::Success,
    );
}

fn set_effective_provider_api_key(cfg: &mut AppConfig, value: &str) {
    use squeezy_core::ProviderConfig as P;
    let v = Some(value.to_string());
    match &mut cfg.provider {
        P::OpenAi(c) => c.api_key = v,
        P::Anthropic(c) => c.api_key = v,
        P::Google(c) => c.api_key = v,
        P::AzureOpenAi(c) => c.api_key = v,
        P::OpenAiCompatible(c) => c.api_key = v,
        // OAuth providers hold credentials in auth files, not TOML
        // `api_key` fields. Ignoring the inline write here keeps the
        // config screen surface a no-op for these variants.
        P::Bedrock(_) | P::Ollama(_) | P::OpenAiCodex(_) | P::GitHubCopilot(_) => {}
    }
}

pub(crate) fn save_theme_selection(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    theme: String,
) {
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let edit = SettingsEdit {
        path: &["tui", "theme"],
        op: EditOp::SetString(theme.clone()),
    };
    let outcome = match apply_edits(&scope_target, &[edit]) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    state.note_session_write(&outcome.path);
    state.effective.tui.theme = theme.clone();
    finish_theme_save(state, agent, notifications);
    notifications.push(
        format!("✓ theme {theme} saved to {}", outcome.path.display()),
        NotifySeverity::Success,
    );
}

pub(crate) fn save_theme_snapshot(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    theme: String,
) {
    let snapshot =
        crate::render::theme::resolve_theme(&state.effective, &state.effective.tui.theme);
    let colors: Vec<(String, [u8; 3])> = snapshot
        .colors()
        .iter()
        .map(|(token, rgb)| (token.clone(), *rgb))
        .collect();
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let edits = [
        SettingsEdit {
            path: &[],
            op: EditOp::SetThemeColors {
                theme: theme.clone(),
                colors: colors.clone(),
            },
        },
        SettingsEdit {
            path: &["tui", "theme"],
            op: EditOp::SetString(theme.clone()),
        },
    ];
    let outcome = match apply_edits(&scope_target, &edits) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    state.note_session_write(&outcome.path);
    state.effective.tui.themes.insert(
        theme.clone(),
        TuiThemeSettings {
            colors: colors.into_iter().collect::<BTreeMap<_, _>>(),
        },
    );
    state.effective.tui.theme = theme.clone();
    finish_theme_save(state, agent, notifications);
    notifications.push(
        format!("✓ created theme {theme} in {}", outcome.path.display()),
        NotifySeverity::Success,
    );
}

pub(crate) fn save_theme_color(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    theme: String,
    token: String,
    rgb: [u8; 3],
) {
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetThemeColor {
            theme: theme.clone(),
            token: token.clone(),
            rgb,
        },
    };
    let outcome = match apply_edits(&scope_target, &[edit]) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    state.note_session_write(&outcome.path);
    state
        .effective
        .tui
        .themes
        .entry(theme.clone())
        .or_default()
        .colors
        .insert(token.clone(), rgb);
    finish_theme_save(state, agent, notifications);
    notifications.push(
        format!("✓ saved {token} for {theme} to {}", outcome.path.display()),
        NotifySeverity::Success,
    );
}

pub(crate) fn save_theme_rename(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    old_theme: String,
    new_theme: String,
) {
    let Some(settings) = state.effective.tui.themes.get(&old_theme).cloned() else {
        notifications.push(
            format!("Theme {old_theme} is not a custom theme."),
            NotifySeverity::Warn,
        );
        return;
    };

    let colors: Vec<(String, [u8; 3])> = settings
        .colors
        .iter()
        .map(|(token, rgb)| (token.clone(), *rgb))
        .collect();
    let active = state.effective.tui.theme == old_theme;
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let mut edits = vec![
        SettingsEdit {
            path: &[],
            op: EditOp::SetThemeColors {
                theme: new_theme.clone(),
                colors,
            },
        },
        SettingsEdit {
            path: &[],
            op: EditOp::RemoveTableEntry {
                table_path: &["tui", "themes"],
                key: old_theme.clone(),
            },
        },
    ];
    if active {
        edits.push(SettingsEdit {
            path: &["tui", "theme"],
            op: EditOp::SetString(new_theme.clone()),
        });
    }

    let outcome = match apply_edits(&scope_target, &edits) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    state.note_session_write(&outcome.path);
    state.effective.tui.themes.remove(&old_theme);
    state
        .effective
        .tui
        .themes
        .insert(new_theme.clone(), settings);
    if active {
        state.effective.tui.theme = new_theme.clone();
    }
    finish_theme_save(state, agent, notifications);
    notifications.push(
        format!(
            "✓ renamed theme {old_theme} to {new_theme} in {}",
            outcome.path.display()
        ),
        NotifySeverity::Success,
    );
}

pub(crate) fn save_theme_delete(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    theme: String,
) {
    if !state.effective.tui.themes.contains_key(&theme) {
        notifications.push(
            format!("Theme {theme} is not a custom theme."),
            NotifySeverity::Warn,
        );
        return;
    }

    let active = state.effective.tui.theme == theme;
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let mut edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::RemoveTableEntry {
            table_path: &["tui", "themes"],
            key: theme.clone(),
        },
    }];
    if active {
        edits.push(SettingsEdit {
            path: &["tui", "theme"],
            op: EditOp::SetString(squeezy_core::DEFAULT_TUI_THEME_NAME.to_string()),
        });
    }

    let outcome = match apply_edits(&scope_target, &edits) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    state.note_session_write(&outcome.path);
    state.effective.tui.themes.remove(&theme);
    if active {
        state.effective.tui.theme = squeezy_core::DEFAULT_TUI_THEME_NAME.to_string();
    }
    finish_theme_save(state, agent, notifications);
    state.field_index = state.field_index.min(state.row_count().saturating_sub(1));
    notifications.push(
        format!("✓ deleted theme {theme} from {}", outcome.path.display()),
        NotifySeverity::Success,
    );
}

pub(crate) fn unset_theme_color(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    theme: String,
    token: String,
) {
    let (target_path, scope_target) = scope_write_target(state);
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let edit = SettingsEdit {
        path: &[],
        op: EditOp::RemoveThemeColor {
            theme: theme.clone(),
            token: token.clone(),
        },
    };
    let outcome = match apply_edits(&scope_target, &[edit]) {
        Ok(o) => o,
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };
    if outcome.edits_applied == 0 {
        state.pop_undo_snapshot();
        notifications.push(
            format!("{token} has no {} override to clear", state.scope.label()),
            NotifySeverity::Info,
        );
        return;
    }
    state.note_session_write(&outcome.path);

    if let Some(settings) = state.effective.tui.themes.get_mut(&theme) {
        settings.colors.remove(&token);
    }
    finish_theme_save(state, agent, notifications);
    notifications.push(
        format!(
            "✓ cleared {token} for {theme} in {}",
            outcome.path.display()
        ),
        NotifySeverity::Success,
    );
}

fn scope_write_target(state: &ConfigScreenState) -> (std::path::PathBuf, SettingsScope) {
    match state.scope {
        ConfigScope::User => {
            let p = state.sources.user_path_default.clone();
            (p.clone(), SettingsScope::user(p))
        }
        ConfigScope::Repo => {
            let p = state.sources.project_path_default.clone();
            (p.clone(), SettingsScope::project(p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(p))
        }
    }
}

fn finish_theme_save(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    state.dirty = true;
    match load_separated_settings_sources() {
        Ok(reloaded) => state.sources = reloaded,
        Err(err) => {
            notifications.push(
                format!("inheritance map stale: {err}"),
                NotifySeverity::Warn,
            );
        }
    }
    crate::apply_theme_overrides(&state.effective);
    agent.replace_config(state.effective.clone());
}

/// The per-provider routing fields use a `["providers", "*", "<key>"]`
/// placeholder `toml_path`; the `"*"` is substituted with the active provider
/// at write time. Returns the `<key>` (`reroute_model`, `judge_model`, …).
fn provider_routing_field_key(field: &FieldMeta) -> Option<&'static str> {
    match field.toml_path {
        ["providers", "*", key] => Some(key),
        _ => None,
    }
}

/// The per-model limit fields use a `["model_limits", "*", "<key>"]`
/// placeholder `toml_path`; the `"*"` is substituted with the active
/// `provider:model` at write time. Returns the `<key>` (`context_window`).
fn model_limit_field_key(field: &FieldMeta) -> Option<&'static str> {
    match field.toml_path {
        ["model_limits", "*", key] => Some(key),
        _ => None,
    }
}

/// Build a `[model_limits."<provider>:<model>"].<key>` write. "auto"/cleared
/// removes the whole entry (its only field is the window), so the resolver
/// falls back to the auto-resolved layers.
fn model_limit_edit(model_key: &str, child_key: &'static str, value: &FieldValue) -> SettingsEdit {
    match value {
        FieldValue::Integer(v) | FieldValue::OptionalInteger(Some(v)) => SettingsEdit {
            path: &[],
            op: EditOp::SetTableEntry {
                table_path: &["model_limits"],
                key: model_key.to_string(),
                fields: vec![(child_key, EditOp::SetInteger(*v))],
            },
        },
        _ => SettingsEdit {
            path: &[],
            op: EditOp::RemoveTableEntry {
                table_path: &["model_limits"],
                key: model_key.to_string(),
            },
        },
    }
}

/// Canonical slug of the active provider (the `[providers.<name>]` key).
fn active_provider_section(state: &ConfigScreenState) -> String {
    super::active_provider_slug(&state.effective)
}

/// Build a `[providers.<active>].<key>` write. Empty values clear the override
/// (so the field falls back to the global / built-in default).
fn provider_routing_edit(provider: &str, key: &'static str, value: &FieldValue) -> SettingsEdit {
    let child_op = match value {
        FieldValue::String(s) if !s.trim().is_empty() => EditOp::SetString(s.clone()),
        FieldValue::StringList(items) if !items.is_empty() => {
            EditOp::SetArrayOfStrings(items.clone())
        }
        _ => EditOp::Unset,
    };
    SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: provider.to_string(),
            fields: vec![(key, child_op)],
        },
    }
}

pub(crate) fn save_field(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    field: &'static FieldMeta,
    previous: FieldValue,
    value: FieldValue,
) {
    save_field_inner(state, agent, notifications, field, previous, value, false);
}

/// `silent=true` skips the per-save notification — used by Space-cycling
/// so the user doesn't see "saved …" pile up while flipping through
/// values. The actual file write and apply-tier dispatch still happen.
pub(crate) fn save_field_silent(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    field: &'static FieldMeta,
    previous: FieldValue,
    value: FieldValue,
) {
    save_field_inner(state, agent, notifications, field, previous, value, true);
}

fn save_field_inner(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    field: &'static FieldMeta,
    previous: FieldValue,
    value: FieldValue,
    silent: bool,
) {
    let scope = state.scope;
    let target_path = match scope {
        ConfigScope::User => state.sources.user_path_default.clone(),
        ConfigScope::Repo => state.sources.project_path_default.clone(),
        ConfigScope::Local => state.sources.repo_path_default.clone(),
    };
    let scope_target = match scope {
        ConfigScope::User => SettingsScope::user(&target_path),
        ConfigScope::Repo => SettingsScope::project(&target_path),
        ConfigScope::Local => SettingsScope::repo(&target_path),
    };

    // Snapshot file bytes before the write so Ctrl+Z can revert this
    // single save. `None` means the file didn't exist before.
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state.push_undo_snapshot(target_path.clone(), pre_write_bytes);

    let mut edits: Vec<SettingsEdit> = vec![if let Some(key) = provider_routing_field_key(field) {
        // Per-provider routing fields (`[providers.*].<key>`) write to the
        // ACTIVE provider's table via a runtime-keyed table entry — the
        // static `toml_path` can't carry the dynamic provider name.
        provider_routing_edit(&active_provider_section(state), key, &value)
    } else if let Some(child) = model_limit_field_key(field) {
        // Per-model limit fields write to `[model_limits."provider:model"]`
        // for the ACTIVE model — same runtime-keyed-table reason.
        model_limit_edit(&state.effective.model_limit_key(), child, &value)
    } else {
        field_edit(field, &value)
    }];
    if is_permission_detail_field(field) {
        edits.push(SettingsEdit {
            path: &["permissions", "mode"],
            op: EditOp::SetString("custom".to_string()),
        });
    }
    // When the user flips `[model].provider`, the in-memory setter has
    // already replaced `cfg.model` with that provider's default — persist
    // the same swap to the tier file so we never leave a half-written
    // pair like (provider=anthropic, model=gpt-5-codex) on disk. Without
    // this, the next process start would surface an inconsistent state.
    if field.toml_path == ["model", "provider"]
        && let FieldValue::String(model_id) = (model_field_meta().get)(&state.effective)
    {
        edits.push(SettingsEdit {
            path: model_field_meta().toml_path,
            op: EditOp::SetString(model_id),
        });
    }
    let outcome = match apply_edits(&scope_target, &edits) {
        Ok(o) => o,
        Err(err) => {
            // Roll back the bookkeeping for the failed write so Ctrl+Z
            // doesn't try to revert a write that never happened.
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };
    state.note_session_write(&outcome.path);

    // Refresh the inheritance map by reloading separated sources. If this
    // fails (e.g. some other tier's file became unreadable), we surface the
    // error via notification but keep the in-memory source stale — the next
    // open of the screen will re-read.
    match load_separated_settings_sources() {
        Ok(reloaded) => state.sources = reloaded,
        Err(err) => {
            notifications.push(
                format!("inheritance map stale: {err}"),
                NotifySeverity::Warn,
            );
        }
    }

    if outcome.edits_applied > 0 {
        record_field_telemetry_change(
            state,
            field,
            &previous,
            &value,
            ConfigTelemetryChangeKind::Set,
        );
    }
    apply_by_tier(state, agent, notifications, field, &outcome, silent);
}

fn is_permission_detail_field(field: &'static FieldMeta) -> bool {
    permission_detail_write_path(field).is_some()
}

fn record_field_telemetry_change(
    state: &mut ConfigScreenState,
    field: &'static FieldMeta,
    previous: &FieldValue,
    next: &FieldValue,
    change_kind: ConfigTelemetryChangeKind,
) {
    state.telemetry_changes.push(ConfigTelemetryChange {
        scope: state.scope,
        section: state.current_section().id.slug(),
        field: telemetry_field_id(field),
        apply_tier: field.tier,
        change_kind,
        prev_value: telemetry_value_bucket(field, previous),
        new_value: telemetry_value_bucket(field, next),
    });
}

fn telemetry_field_id(field: &'static FieldMeta) -> String {
    field
        .toml_path
        .iter()
        .map(|part| if *part == "*" { "star" } else { *part })
        .collect::<Vec<_>>()
        .join(".")
}

fn telemetry_value_bucket(field: &'static FieldMeta, value: &FieldValue) -> String {
    match value {
        FieldValue::Unset => "unset".to_string(),
        FieldValue::Bool(true) => "bool_true".to_string(),
        FieldValue::Bool(false) => "bool_false".to_string(),
        FieldValue::Integer(value) => integer_bucket(*value),
        FieldValue::OptionalInteger(Some(value)) => integer_bucket(*value),
        FieldValue::OptionalInteger(None) => "integer_none".to_string(),
        FieldValue::OptionalFloat(Some(_)) => "float_set".to_string(),
        FieldValue::OptionalFloat(None) => "float_none".to_string(),
        FieldValue::Enum(value) | FieldValue::OptionalEnum(Some(value)) => {
            if field.toml_path == ["model", "model"] {
                "model_custom".to_string()
            } else {
                format!("enum_{}", sanitize_token(value))
            }
        }
        FieldValue::OptionalEnum(None) => "enum_none".to_string(),
        FieldValue::String(value) => string_bucket(field, value),
        FieldValue::Duration(value) => duration_bucket(value.as_millis() as u64),
        FieldValue::StringList(items) => count_bucket("list", items.len() as u64),
        FieldValue::Path(path) => {
            if path.as_os_str().is_empty() {
                "path_empty".to_string()
            } else if path.is_absolute() {
                "path_absolute".to_string()
            } else {
                "path_relative".to_string()
            }
        }
        FieldValue::Secret => "secret".to_string(),
        FieldValue::SubTabs(index) => count_bucket("subtabs", *index as u64),
        FieldValue::TableArrayKeyed(items) => count_bucket("table", items.len() as u64),
        FieldValue::TableArrayOrdered(items) => count_bucket("table", items.len() as u64),
    }
}

fn string_bucket(field: &'static FieldMeta, value: &str) -> String {
    if value.trim().is_empty() {
        return "string_empty".to_string();
    }
    if field.toml_path == ["model", "model"] {
        return "model_custom".to_string();
    }
    if value == field.default_display {
        return "string_default".to_string();
    }
    "string_custom".to_string()
}

fn integer_bucket(value: i64) -> String {
    match value {
        i64::MIN..=-1 => "integer_negative".to_string(),
        0 => "integer_zero".to_string(),
        1..=10 => "integer_1_10".to_string(),
        11..=100 => "integer_11_100".to_string(),
        101..=1000 => "integer_101_1000".to_string(),
        _ => "integer_gt_1000".to_string(),
    }
}

fn duration_bucket(ms: u64) -> String {
    match ms {
        0 => "duration_zero".to_string(),
        1..=999 => "duration_lt_1s".to_string(),
        1000..=9999 => "duration_1s_10s".to_string(),
        10_000..=59_999 => "duration_10s_60s".to_string(),
        _ => "duration_ge_60s".to_string(),
    }
}

fn count_bucket(prefix: &str, count: u64) -> String {
    match count {
        0 => format!("{prefix}_0"),
        1 => format!("{prefix}_1"),
        2..=5 => format!("{prefix}_2_5"),
        6..=20 => format!("{prefix}_6_20"),
        _ => format!("{prefix}_gt_20"),
    }
}

fn sanitize_token(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(64));
    for ch in value.chars().take(64) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | '.') {
            out.push('_');
        }
    }
    if out.is_empty() {
        "custom".to_string()
    } else {
        out
    }
}

fn apply_by_tier(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    field: &'static FieldMeta,
    outcome: &WriteOutcome,
    silent: bool,
) {
    state.effective.refresh_config_warnings();
    let path_str = outcome.path.display().to_string();
    match field.tier {
        ApplyTier::Immediate => {
            agent.replace_config(state.effective.clone());
            if !silent {
                notifications.push(
                    format!("✓ saved {} to {}", field.label, path_str),
                    NotifySeverity::Success,
                );
            }
        }
        ApplyTier::NextPrompt => {
            // Edits to any `[model].*` field (and per-provider `[providers.*]`
            // fields, once we surface them) might require rebuilding the LLM
            // client — different api_key_env, base_url, or whole provider
            // variant. Build the new provider eagerly so the next turn
            // doesn't blow up with "missing OPENAI_API_KEY" on a fresh
            // anthropic swap.
            let touches_provider = field
                .toml_path
                .first()
                .copied()
                .map(|head| head == "model" || head == "providers")
                .unwrap_or(false);
            let new_provider = if touches_provider {
                // `provider_from_config` resolves the API key, which on Linux
                // talks to D-Bus via the `keyring` crate. zbus refuses to do
                // blocking I/O inside a tokio runtime and panics. We run the
                // build on a plain OS thread so the keychain call is not
                // observed by tokio.
                let provider_cfg = state.effective.provider.clone();
                let handle =
                    std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg));
                match handle.join() {
                    Ok(Ok(p)) => Some(p),
                    Ok(Err(err)) => {
                        let label = provider_variant_label(&state.effective.provider);
                        // The save itself succeeded; the provider just isn't
                        // usable yet (usually a missing API key). That's
                        // informational, not an error — keep it neutral chrome.
                        notifications.push(
                            format!("saved to {path_str} — {label} isn't active yet: {err}"),
                            NotifySeverity::Info,
                        );
                        Some(Arc::new(squeezy_llm::UnavailableProvider::new(
                            label,
                            err.to_string(),
                        ))
                            as Arc<dyn squeezy_llm::LlmProvider>)
                    }
                    Err(_) => {
                        let label = provider_variant_label(&state.effective.provider);
                        notifications.push(
                            format!("saved to {path_str} — couldn't activate {label}"),
                            NotifySeverity::Info,
                        );
                        Some(Arc::new(squeezy_llm::UnavailableProvider::new(
                            label,
                            "provider builder thread panicked",
                        ))
                            as Arc<dyn squeezy_llm::LlmProvider>)
                    }
                }
            } else {
                None
            };
            // When the change actually rebuilt the provider, name the
            // new provider so the next-prompt apply notification reads
            // "✓ applied: provider → anthropic" instead of the opaque
            // "provider changed". For non-provider NextPrompt fields
            // (model id, reasoning_effort, …) we still use the field
            // label.
            let display_note = if touches_provider {
                format!(
                    "provider → {}",
                    provider_variant_label(&state.effective.provider)
                )
            } else {
                format!("{} changed", field.label)
            };
            agent.arm_config_swap(PendingConfigSwap {
                config: state.effective.clone(),
                provider: new_provider,
                display_note: Some(display_note),
            });
            if !silent {
                notifications.push(
                    format!(
                        "{} changed — applies on next prompt. Saved to {}",
                        field.label, path_str
                    ),
                    NotifySeverity::Info,
                );
            }
        }
        ApplyTier::Restart => {
            if !silent {
                notifications.push(
                    format!(
                        "{} saved to {}. Restart required for the change to take effect.",
                        field.label, path_str
                    ),
                    NotifySeverity::Warn,
                );
            }
        }
    }
}

fn field_edit(field: &'static FieldMeta, value: &FieldValue) -> SettingsEdit {
    let op = match (field.kind, value) {
        (_, FieldValue::Unset) => EditOp::Unset,
        (_, FieldValue::String(s)) => EditOp::SetString(s.clone()),
        (_, FieldValue::Bool(b)) => EditOp::SetBool(*b),
        (_, FieldValue::Integer(v)) => EditOp::SetInteger(*v),
        (_, FieldValue::OptionalInteger(Some(v))) => EditOp::SetInteger(*v),
        (_, FieldValue::OptionalInteger(None)) => EditOp::Unset,
        (_, FieldValue::OptionalFloat(Some(v))) => EditOp::SetFloat(*v),
        (_, FieldValue::OptionalFloat(None)) => EditOp::Unset,
        (_, FieldValue::Enum(s)) => EditOp::SetString((*s).to_string()),
        (_, FieldValue::OptionalEnum(Some(s))) => EditOp::SetString((*s).to_string()),
        (_, FieldValue::OptionalEnum(None)) => EditOp::Unset,
        (_, FieldValue::Duration(d)) => EditOp::SetInteger(d.as_millis() as i64),
        (_, FieldValue::StringList(items)) => EditOp::SetArrayOfStrings(items.clone()),
        (_, FieldValue::Path(p)) => EditOp::SetString(p.display().to_string()),
        // Secret / SubTabs / TableArray* never go through field_edit; their
        // commits are routed to dedicated handlers in commit 5. If we ever
        // reach this arm it's a programmer bug.
        (_, FieldValue::Secret)
        | (_, FieldValue::SubTabs(_))
        | (_, FieldValue::TableArrayKeyed(_))
        | (_, FieldValue::TableArrayOrdered(_)) => EditOp::Unset,
    };
    SettingsEdit {
        path: field_write_path(field),
        op,
    }
}

fn field_write_path(field: &'static FieldMeta) -> &'static [&'static str] {
    permission_detail_write_path(field).unwrap_or(field.toml_path)
}

/// The `[permissions.custom].<field>` location the screen both writes to
/// and reads back from for the ten granular permission fields. `None` for
/// every other field. The read paths (`tier_value_at_path`,
/// `scope_owns_field`) must probe this before the legacy top-level
/// `field.toml_path`, or saved values never re-display.
///
/// Keyed off `field.toml_path` (itself `'static`), so it works for a
/// borrowed `&FieldMeta` and not just `&'static FieldMeta`.
pub(crate) fn permission_detail_read_path(field: &FieldMeta) -> Option<&'static [&'static str]> {
    permission_detail_path(field.toml_path)
}

fn permission_detail_write_path(field: &'static FieldMeta) -> Option<&'static [&'static str]> {
    permission_detail_path(field.toml_path)
}

fn permission_detail_path(toml_path: &[&str]) -> Option<&'static [&'static str]> {
    match toml_path {
        ["permissions", "read"] => Some(&["permissions", "custom", "read"]),
        ["permissions", "search"] => Some(&["permissions", "custom", "search"]),
        ["permissions", "edit"] => Some(&["permissions", "custom", "edit"]),
        ["permissions", "shell"] => Some(&["permissions", "custom", "shell"]),
        ["permissions", "ignored_search"] => Some(&["permissions", "custom", "ignored_search"]),
        ["permissions", "web"] => Some(&["permissions", "custom", "network"]),
        ["permissions", "mcp"] => Some(&["permissions", "custom", "mcp"]),
        ["permissions", "git"] => Some(&["permissions", "custom", "git"]),
        ["permissions", "compiler"] => Some(&["permissions", "custom", "compiler"]),
        ["permissions", "destructive"] => Some(&["permissions", "custom", "destructive"]),
        _ => None,
    }
}

fn clear_field_edits(field: &'static FieldMeta) -> Vec<SettingsEdit> {
    let mut edits = vec![SettingsEdit {
        path: field_write_path(field),
        op: EditOp::Unset,
    }];
    if permission_detail_write_path(field).is_some() {
        edits.push(SettingsEdit {
            path: field.toml_path,
            op: EditOp::Unset,
        });
    }
    edits
}

/// Clear the focused field's value in whichever tier the active scope tab
/// points at. The User scope returns early in the caller, so this only runs
/// for Repo (committed `./squeezy.toml`) or Local (per-machine).
/// Ctrl+Z — restore the most recent edited tier file to its
/// pre-write bytes and refresh the agent + sources. No-op when the
/// session hasn't written anything yet.
pub(crate) fn undo_last_write(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    let Some((path, pre_bytes, marker)) = state.pop_undo_snapshot() else {
        notifications.push("Nothing to undo this session.", NotifySeverity::Info);
        return;
    };
    if state.restore_would_clobber_external_edit(&path) {
        notifications.push(
            format!(
                "{} changed on disk since this session wrote it — undo skipped to avoid \
                 clobbering an external edit.",
                path.display()
            ),
            NotifySeverity::Warn,
        );
        state.truncate_telemetry_to(marker);
        return;
    }
    if let Err(err) = restore_path(&path, pre_bytes.as_deref()) {
        notifications.push(
            format!("Undo failed for {}: {err}", path.display()),
            NotifySeverity::Error,
        );
        // Put the snapshot back so a retry is possible.
        state.push_undo_snapshot(path, pre_bytes);
        return;
    }
    state.truncate_telemetry_to(marker);
    reload_sources_and_agent(state, agent, notifications);
    notifications.push(
        format!("✓ undid last write to {}", path.display()),
        NotifySeverity::Success,
    );
}

/// Discard every write made since the screen opened by restoring each
/// tier file to its baseline bytes. Clears the undo stack.
pub(crate) fn discard_all_session_writes(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    if state.undo_stack.is_empty() {
        notifications.push(
            "Nothing to discard — no writes this session.",
            NotifySeverity::Info,
        );
        return;
    }
    let mut restored = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let plan: Vec<(std::path::PathBuf, Option<Vec<u8>>, bool)> = state
        .baseline
        .iter()
        .map(|(path, baseline_bytes)| {
            (
                path.clone(),
                baseline_bytes.clone(),
                state.restore_would_clobber_external_edit(path),
            )
        })
        .collect();
    for (path, baseline_bytes, would_clobber) in &plan {
        if *would_clobber {
            skipped.push(path.display().to_string());
            continue;
        }
        if let Err(err) = restore_path(path, baseline_bytes.as_deref()) {
            failed.push(format!("{}: {err}", path.display()));
        } else {
            restored += 1;
        }
    }
    state.undo_stack.clear();
    state.telemetry_undo_markers.clear();
    state.telemetry_changes.clear();
    reload_sources_and_agent(state, agent, notifications);
    if !skipped.is_empty() {
        notifications.push(
            format!(
                "kept external edits to {} (changed on disk since this session opened)",
                skipped.join(", ")
            ),
            NotifySeverity::Warn,
        );
    }
    if failed.is_empty() {
        notifications.push(
            format!("✓ discarded all session writes ({restored} files restored)"),
            NotifySeverity::Success,
        );
    } else {
        notifications.push(
            format!(
                "partial restore — {} ok, failures: {}",
                restored,
                failed.join("; ")
            ),
            NotifySeverity::Warn,
        );
    }
}

/// Either write `bytes` to `path` (overwriting) or remove `path` when
/// the baseline is `None` (the file didn't exist when the screen opened).
pub(crate) fn restore_path(path: &std::path::Path, bytes: Option<&[u8]>) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match bytes {
        Some(b) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, b)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        },
    }
}

/// Reload separated sources and refresh the agent's effective config so
/// the post-undo/discard/reset state propagates without requiring a
/// restart.
///
/// If the provider *variant* changed (e.g. anthropic → openai because a
/// Local override was reset), the LLM client must be rebuilt — otherwise
/// the next turn runs an Anthropic client against an OpenAI config and
/// fails immediately. The rebuild happens on a plain OS thread (same
/// trick as `apply_by_tier::NextPrompt`) so the Linux keychain doesn't
/// panic from inside a tokio runtime, and is armed as a NextPrompt swap
/// so any in-flight turn keeps its current client.
pub(crate) fn reload_sources_and_agent(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    if let Ok(reloaded) = load_separated_settings_sources() {
        state.sources = reloaded;
    }
    let new_cfg = match AppConfig::from_env_and_settings() {
        Ok(cfg) => cfg,
        Err(err) => {
            notifications.push(
                format!("(note) couldn't rebuild effective config: {err}"),
                NotifySeverity::Warn,
            );
            return;
        }
    };
    let old_provider = provider_variant_label(&state.effective.provider);
    let new_provider = provider_variant_label(&new_cfg.provider);
    let provider_changed = old_provider != new_provider;
    state.effective = new_cfg.clone();
    if !provider_changed {
        agent.replace_config(new_cfg);
        return;
    }
    let provider_cfg = new_cfg.provider.clone();
    let handle = std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg));
    match handle.join() {
        Ok(Ok(provider)) => {
            agent.arm_config_swap(PendingConfigSwap {
                config: new_cfg,
                provider: Some(provider),
                display_note: Some(format!(
                    "provider {old_provider} → {new_provider} (applies on next prompt)"
                )),
            });
        }
        Ok(Err(err)) => {
            agent.replace_config(new_cfg);
            notifications.push(
                format!("provider switched in config but the new client failed to build: {err}"),
                NotifySeverity::Error,
            );
        }
        Err(_) => {
            agent.replace_config(new_cfg);
            notifications.push(
                "provider switched but the builder thread panicked",
                NotifySeverity::Error,
            );
        }
    }
}

/// Silent variant — used by Space-cycle when wrapping past the last
/// option to "inherit". Suppresses the chatty "cleared … (now inherited)"
/// notification that would otherwise pile up.
pub(crate) fn clear_scope_override_silent(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    let field = state.current_field();
    let previous = (field.get)(&state.effective);
    let (path, scope_target) = match state.scope {
        ConfigScope::Repo => {
            let p = state.sources.project_path_default.clone();
            (p.clone(), SettingsScope::project(&p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(&p))
        }
        ConfigScope::User => return,
    };
    let edits = clear_field_edits(field);
    // Snapshot for undo before the write.
    let pre = std::fs::read(&path).ok();
    state.push_undo_snapshot(path.clone(), pre);
    match apply_edits(&scope_target, &edits) {
        Ok(outcome) => {
            if outcome.edits_applied > 0 {
                state.note_session_write(&path);
                record_field_telemetry_change(
                    state,
                    field,
                    &previous,
                    &FieldValue::Unset,
                    ConfigTelemetryChangeKind::Unset,
                );
                reload_sources_and_agent(state, agent, notifications);
            } else {
                state.pop_undo_snapshot();
            }
        }
        Err(_) => {
            // Failed write — drop the unused snapshot so Ctrl+Z stays in sync.
            state.pop_undo_snapshot();
        }
    }
}

pub(crate) fn clear_scope_override(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
) {
    let field = state.current_field();
    let previous = (field.get)(&state.effective);
    let (path, scope_target) = match state.scope {
        ConfigScope::Repo => {
            let p = state.sources.project_path_default.clone();
            (p.clone(), SettingsScope::project(&p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(&p))
        }
        ConfigScope::User => return, // caller filters this out
    };
    let edits = clear_field_edits(field);
    let scope_label = state.scope.label();
    let pre = std::fs::read(&path).ok();
    state.push_undo_snapshot(path.clone(), pre);
    match apply_edits(&scope_target, &edits) {
        Ok(outcome) if outcome.edits_applied > 0 => {
            state.note_session_write(&path);
            record_field_telemetry_change(
                state,
                field,
                &previous,
                &FieldValue::Unset,
                ConfigTelemetryChangeKind::Unset,
            );
            reload_sources_and_agent(state, agent, notifications);
            notifications.push(
                format!(
                    "cleared {} override in {} (now inherited)",
                    field.label,
                    path.display()
                ),
                NotifySeverity::Success,
            );
        }
        Ok(_) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("{} had no {} override to clear", field.label, scope_label),
                NotifySeverity::Info,
            );
        }
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Failed to clear override: {err}"),
                NotifySeverity::Error,
            );
        }
    }
}

/// Delete `scope`'s tier file (no-op when it doesn't exist on disk) and
/// reload the in-memory sources + effective config. Any field whose
/// previous value lived in the deleted file falls back through the
/// remaining tiers — that recompute can also change values shown on
/// other tabs, which is exactly what "reset" promises.
pub(crate) fn perform_reset(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut ConfigFeedback,
    scope: ConfigScope,
) {
    let path = tier_path(state, scope);
    if path.as_os_str().is_empty() {
        notifications.push(
            format!(
                "{} tier has no resolved file path; nothing to reset.",
                scope.label()
            ),
            NotifySeverity::Warn,
        );
        return;
    }
    // Snapshot the file bytes so Ctrl+Z can put them back.
    let pre = std::fs::read(&path).ok();
    state.push_undo_snapshot(path.clone(), pre.clone());
    match restore_path(&path, None) {
        Ok(()) => {
            state.note_session_write(&path);
            reload_sources_and_agent(state, agent, notifications);
            if pre.is_some() {
                state.telemetry_changes.push(ConfigTelemetryChange {
                    scope,
                    section: SectionId::Reset.slug(),
                    field: "tier_file".to_string(),
                    apply_tier: ApplyTier::Restart,
                    change_kind: ConfigTelemetryChangeKind::Reset,
                    prev_value: "file_present".to_string(),
                    new_value: "file_absent".to_string(),
                });
            }
            let msg = if pre.is_some() {
                format!(
                    "✓ reset {} settings — deleted {} (Ctrl+Z to restore)",
                    scope.label(),
                    path.display()
                )
            } else {
                // No file existed; nothing was actually removed, but
                // restating it as "already at defaults" reads cleaner
                // than a silent no-op.
                state.pop_undo_snapshot();
                format!(
                    "{} tier already at inherited / default values.",
                    scope.label()
                )
            };
            notifications.push(msg, NotifySeverity::Success);
        }
        Err(err) => {
            state.pop_undo_snapshot();
            notifications.push(
                format!("Reset of {} failed: {err}", path.display()),
                NotifySeverity::Error,
            );
        }
    }
}

#[cfg(test)]
#[path = "save_tests.rs"]
mod tests;
