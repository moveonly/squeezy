use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// A trivial provider so we can build an Agent for testing.
struct NoOpProvider;
impl squeezy_llm::LlmProvider for NoOpProvider {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn stream_response(
        &self,
        _request: squeezy_llm::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> squeezy_llm::LlmStream {
        use futures_util::stream;
        Box::pin(stream::iter(Vec::new()))
    }
}

fn make_agent() -> Agent {
    let provider: Arc<dyn squeezy_llm::LlmProvider> = Arc::new(NoOpProvider);
    Agent::new(AppConfig::default(), provider)
}

static TEMP_CONFIG_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_config_state(focus: Option<SectionId>) -> ConfigScreenState {
    let nonce = TEMP_CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("squeezy_config_screen_{nonce}"));
    std::fs::create_dir_all(&root).expect("create temp config dir");
    let mut state = ConfigScreenState::new(AppConfig::default(), focus);
    let user = root.join("user-settings.toml");
    let project = root.join("repo-settings.toml");
    let repo = root.join("local-settings.toml");
    state.sources.user_path_default = user.clone();
    state.sources.project_path_default = project.clone();
    state.sources.repo_path_default = repo.clone();
    state.sources.user = None;
    state.sources.project = None;
    state.sources.repo = None;
    state.baseline = vec![(user, None), (project, None), (repo, None)];
    state
}

fn field_index(section_id: SectionId, toml_path: &[&str]) -> usize {
    CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == section_id)
        .and_then(|section| {
            section
                .fields
                .iter()
                .position(|field| field.toml_path == toml_path)
        })
        .expect("field exists")
}

#[test]
fn opens_at_requested_section() {
    let state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Permissions));
    assert_eq!(state.current_section().id, SectionId::Permissions);
}

#[test]
fn opens_at_models_when_no_focus() {
    let state = ConfigScreenState::new(AppConfig::default(), None);
    assert_eq!(state.current_section().id, SectionId::Models);
}

#[test]
fn tab_cycles_through_three_scopes() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    // Default scope is User (leftmost tab) — Tab walks
    // User → Repo → Local → User and BackTab reverses.
    assert_eq!(state.scope, ConfigScope::User);
    for expected in [ConfigScope::Repo, ConfigScope::Local, ConfigScope::User] {
        handle_key(
            &mut state,
            &mut agent,
            &mut q,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        assert_eq!(state.scope, expected);
    }
    // BackTab reverses: User → Local.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty()),
    );
    assert_eq!(state.scope, ConfigScope::Local);
}

/// Regression: pressing keys that aren't explicit `/mcp` bindings on
/// the McpServers section used to fall through to the global browse
/// keymap, which called `state.current_field()` for arms like Space
/// and Ctrl+R. McpServers has no `FieldMeta` rows so `current_field`
/// panicked — crashing the whole TUI when a user pressed Space on a
/// server row. The page-specific handler now absorbs every
/// non-navigation key and aliases Space to the toggle action.
#[test]
fn mcp_section_absorbs_keys_that_would_otherwise_panic_current_field() {
    use crate::config_screen::McpAction;
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::McpServers));
    // Seed a single server so `field_index = 0` lands on a real row
    // rather than the trailing `(add)` row.
    state.mcp_servers.insert(
        "bench".to_string(),
        squeezy_core::McpServerConfig {
            enabled: false,
            transport: squeezy_core::McpTransport::Stdio,
            command: Some("squeezy-fake-mcp".to_string()),
            args: Vec::new(),
            url: None,
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
        },
    );
    state.field_index = 0;
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();

    // Space used to panic via the global cycle-bool handler reaching
    // for `state.current_field()`. It must now alias to the toggle
    // action, flipping the optimistic cached state and queuing a
    // `McpAction::Toggle { persist: true }` for the host loop.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert!(
        state.mcp_servers.get("bench").is_some_and(|s| s.enabled),
        "Space must flip the cached enabled flag for the server at focus"
    );
    assert!(
        matches!(
            state.mcp_pending_actions.last(),
            Some(McpAction::Toggle {
                server,
                enabled: true,
                persist: true
            }) if server == "bench"
        ),
        "Space must stage a persist toggle (defaults match lowercase `e`)"
    );

    // Any printable character not bound to an `/mcp` action used to
    // fall through to the global Space handler (which panicked via
    // `current_field`). The page handler should absorb it without
    // staging an action.
    let pending_before = state.mcp_pending_actions.len();
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
    );
    assert_eq!(
        state.mcp_pending_actions.len(),
        pending_before,
        "Unbound `/mcp` keys must be absorbed without staging an action or panicking"
    );
}

#[test]
fn themes_section_exposes_builtins_new_row_and_color_tokens() {
    let state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Themes));

    assert_eq!(state.current_section().id, SectionId::Themes);
    assert_eq!(
        state.row_count(),
        crate::render::theme::available_theme_names(&state.effective).len()
            + 1
            + squeezy_core::TUI_THEME_COLOR_TOKENS.len()
    );
    assert_eq!(
        state.theme_row_at(0),
        Some(ThemeRow::Theme("default".to_string()))
    );
}

#[tokio::test]
async fn themes_section_selects_builtin_theme_immediately() {
    let mut state = temp_config_state(Some(SectionId::Themes));
    let settings_path = state.sources.user_path_default.clone();
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = crate::render::theme::available_theme_names(&state.effective)
        .iter()
        .position(|name| name == "bright")
        .expect("bright builtin exists");

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );

    assert_eq!(state.effective.tui.theme, "bright");
    assert_eq!(agent.config_snapshot().tui.theme, "bright");
    let saved = std::fs::read_to_string(settings_path).expect("theme selection saved");
    assert!(saved.contains("theme = \"bright\""), "{saved}");
}

#[tokio::test]
async fn themes_section_edits_active_theme_rgb_token() {
    let mut state = temp_config_state(Some(SectionId::Themes));
    let settings_path = state.sources.user_path_default.clone();
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    let token = crate::render::theme::token::PALETTE_ACCENT;
    let color_row = crate::render::theme::available_theme_names(&state.effective).len()
        + 1
        + crate::render::theme::token_rows()
            .iter()
            .position(|candidate| *candidate == token)
            .expect("token exists");
    state.field_index = color_row;

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    match state.theme_editor.as_mut().expect("rgb editor opens") {
        ThemeEditor::Rgb { draft, cursor, .. } => {
            *draft = "1,2,3".to_string();
            *cursor = draft.chars().count();
        }
        other => panic!("expected RGB editor, got {other:?}"),
    }
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );

    assert_eq!(
        state
            .effective
            .tui
            .themes
            .get("default")
            .and_then(|theme| theme.colors.get(token)),
        Some(&[1, 2, 3])
    );
    assert_eq!(
        agent
            .config_snapshot()
            .tui
            .themes
            .get("default")
            .and_then(|theme| theme.colors.get(token)),
        Some(&[1, 2, 3])
    );
    let saved = std::fs::read_to_string(settings_path).expect("theme color saved");
    assert!(saved.contains("\"palette.accent\" = [1, 2, 3]"), "{saved}");
}

#[tokio::test]
async fn themes_section_creates_custom_theme_snapshot() {
    let mut state = temp_config_state(Some(SectionId::Themes));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = crate::render::theme::available_theme_names(&state.effective).len();

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    assert!(matches!(
        &state.theme_editor,
        Some(ThemeEditor::Name { .. })
    ));
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );

    let custom = state
        .effective
        .tui
        .themes
        .get("custom-theme")
        .expect("custom theme saved");
    assert_eq!(state.effective.tui.theme, "custom-theme");
    assert_eq!(agent.config_snapshot().tui.theme, "custom-theme");
    assert_eq!(
        custom.colors.len(),
        squeezy_core::TUI_THEME_COLOR_TOKENS.len(),
        "custom themes start as a full editable snapshot"
    );
}

#[tokio::test]
async fn themes_section_renames_custom_theme() {
    let mut state = temp_config_state(Some(SectionId::Themes));
    let settings_path = state.sources.user_path_default.clone();
    std::fs::write(
        &settings_path,
        "[tui]\ntheme = \"ocean\"\n\n[tui.themes.ocean.colors]\n\"palette.accent\" = [1, 2, 3]\n",
    )
    .expect("seed settings");
    state.effective.tui.theme = "ocean".to_string();
    state.effective.tui.themes.insert(
        "ocean".to_string(),
        squeezy_core::TuiThemeSettings {
            colors: [(
                crate::render::theme::token::PALETTE_ACCENT.to_string(),
                [1, 2, 3],
            )]
            .into_iter()
            .collect(),
        },
    );
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = crate::render::theme::available_theme_names(&state.effective)
        .iter()
        .position(|name| name == "ocean")
        .expect("custom theme row exists");

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
    );
    match state.theme_editor.as_mut().expect("rename editor opens") {
        ThemeEditor::Rename { draft, cursor, .. } => {
            *draft = "ocean-renamed".to_string();
            *cursor = draft.chars().count();
        }
        other => panic!("expected rename editor, got {other:?}"),
    }
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );

    assert!(!state.effective.tui.themes.contains_key("ocean"));
    assert!(state.effective.tui.themes.contains_key("ocean-renamed"));
    assert_eq!(state.effective.tui.theme, "ocean-renamed");
    assert_eq!(agent.config_snapshot().tui.theme, "ocean-renamed");
    let saved = std::fs::read_to_string(settings_path).expect("renamed theme saved");
    assert!(saved.contains("theme = \"ocean-renamed\""), "{saved}");
    assert!(
        saved.contains("[tui.themes.ocean-renamed.colors]"),
        "{saved}"
    );
    assert!(!saved.contains("[tui.themes.ocean.colors]"), "{saved}");
}

#[tokio::test]
async fn themes_section_deletes_custom_theme_and_falls_back_when_active() {
    let mut state = temp_config_state(Some(SectionId::Themes));
    let settings_path = state.sources.user_path_default.clone();
    std::fs::write(
        &settings_path,
        "[tui]\ntheme = \"ocean\"\n\n[tui.themes.ocean.colors]\n\"palette.accent\" = [1, 2, 3]\n",
    )
    .expect("seed settings");
    state.effective.tui.theme = "ocean".to_string();
    state.effective.tui.themes.insert(
        "ocean".to_string(),
        squeezy_core::TuiThemeSettings {
            colors: [(
                crate::render::theme::token::PALETTE_ACCENT.to_string(),
                [1, 2, 3],
            )]
            .into_iter()
            .collect(),
        },
    );
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = crate::render::theme::available_theme_names(&state.effective)
        .iter()
        .position(|name| name == "ocean")
        .expect("custom theme row exists");

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
    );

    assert!(!state.effective.tui.themes.contains_key("ocean"));
    assert_eq!(state.effective.tui.theme, "default");
    assert_eq!(agent.config_snapshot().tui.theme, "default");
    let saved = std::fs::read_to_string(settings_path).expect("theme deletion saved");
    assert!(saved.contains("theme = \"default\""), "{saved}");
    assert!(!saved.contains("[tui.themes.ocean.colors]"), "{saved}");
}

#[test]
fn arrow_keys_navigate_sections_and_fields() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    assert_eq!(state.field_index, 0);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
    );
    assert_eq!(state.field_index, 1);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
    );
    assert_eq!(state.section_index, 1);
    assert_eq!(state.field_index, 0); // reset on section change
}

#[test]
fn space_toggles_bool_field() {
    // Use a field whose schema declares no env_override so the assertion
    // can't race with `enter_on_env_shadowed_field_emits_warning_*` setting
    // SQUEEZY_TELEMETRY in parallel.
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "show_reasoning_usage"]);
    let before = state.effective.tui.show_reasoning_usage;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert_ne!(state.effective.tui.show_reasoning_usage, before);
}

#[test]
fn string_list_editor_round_trips_via_commit() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldKind, FieldValue, SectionId as SId};
    // Find the Graph section and its languages field.
    let graph = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SId::Graph)
        .expect("Graph section registered");
    let lang_field = graph
        .fields
        .iter()
        .find(|f| f.label == "languages")
        .expect("languages field");
    assert!(matches!(lang_field.kind, FieldKind::StringList { .. }));

    // Open the editor with a baseline value, then simulate Enter on the
    // comma-separated text. Asserts the commit path produces StringList.
    let initial = (lang_field.get)(&AppConfig::default());
    assert!(matches!(initial, FieldValue::StringList(_)));

    let mut editor = open_editor_for(lang_field, FieldValue::StringList(vec!["rust".into()]));
    // Append " , python" then commit.
    let extra = [',', ' ', 'p', 'y', 't', 'h', 'o', 'n'];
    for ch in extra {
        let outcome = handle_editor_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
        assert!(matches!(outcome, EditorOutcome::KeepEditing));
    }
    let commit = handle_editor_key(
        &mut editor,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let items = match commit {
        EditorOutcome::Commit(FieldValue::StringList(items)) => items,
        other => panic!("expected StringList commit, got {:?}", other),
    };
    assert_eq!(items, vec!["rust".to_string(), "python".to_string()]);
}

#[tokio::test]
async fn enter_on_model_field_opens_picker_and_filter_narrows_matches() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    // Layout: provider at row 0, model at row 1, synthetic API-key at row 2.
    state.field_index = 1;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let picker = state.picker.as_ref().expect("picker open");
    assert!(!picker.all_providers);
    let initial_matches = picker_matches(picker).len();
    assert!(
        initial_matches > 0,
        "registry should have at least one openai model"
    );

    // Type "claude" — with provider=openai and all_providers=false, this
    // filter should produce zero matches (claude models live under anthropic).
    for ch in ['c', 'l', 'a', 'u', 'd', 'e'] {
        handle_key(
            &mut state,
            &mut agent,
            &mut q,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
    }
    let after_filter = picker_matches(state.picker.as_ref().unwrap()).len();
    assert_eq!(after_filter, 0, "openai filter should not match claude*");

    // Tab toggles all_providers, which should expose claude entries.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
    );
    let after_tab = picker_matches(state.picker.as_ref().unwrap()).len();
    assert!(
        after_tab > 0,
        "all-providers + claude filter should match anthropic models"
    );
}

#[tokio::test]
async fn esc_on_model_picker_closes_picker_only() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = 1; // model row (synthetic API-key now lives at row 2)
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    assert!(state.picker.is_some(), "picker should be open");
    let outcome = handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
    );
    assert!(state.picker.is_none(), "Esc should close the picker");
    assert!(
        matches!(outcome, KeyOutcome::KeepOpen),
        "Esc on picker should NOT close the whole screen",
    );
}

#[tokio::test]
async fn space_cycles_model_field_to_next_registry_entry() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId};
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_MODEL");
        std::env::remove_var("SQUEEZY_PROVIDER");
    }
    let mut state = temp_config_state(Some(SId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = 1; // model row (synthetic API-key now lives at row 2)
    let before = match (CONFIG_SECTIONS[0].fields[1].get)(&state.effective) {
        FieldValue::String(s) => s,
        other => panic!("expected String, got {other:?}"),
    };
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let after = match (CONFIG_SECTIONS[0].fields[1].get)(&state.effective) {
        FieldValue::String(s) => s,
        other => panic!("expected String, got {other:?}"),
    };
    assert_ne!(
        before, after,
        "Space on model should advance to a different registry entry"
    );
}

#[tokio::test]
async fn space_on_non_cyclable_field_emits_hint() {
    use squeezy_core::config_schema::SectionId as SId;
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Limits));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    // max_parallel_tools is Integer — Space should surface a hint, not silently no-op.
    state.field_index = 0;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let current = q.current().expect("hint notification queued");
    assert!(
        current.message.contains("Space doesn't cycle"),
        "expected cycling hint, got: {}",
        current.message
    );
}

#[tokio::test]
async fn space_cycles_enum_field_to_next_option() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId};
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_PROVIDER");
    }
    let mut state = temp_config_state(Some(SId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = 0; // provider (Enum)
    let before = match (CONFIG_SECTIONS[0].fields[0].get)(&state.effective) {
        FieldValue::Enum(v) => v,
        other => panic!("expected enum, got {other:?}"),
    };
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let after = match (CONFIG_SECTIONS[0].fields[0].get)(&state.effective) {
        FieldValue::Enum(v) => v,
        other => panic!("expected enum, got {other:?}"),
    };
    assert_ne!(
        before, after,
        "Space should advance enum to a different value"
    );
}

#[tokio::test]
async fn slash_opens_search_and_enter_jumps_to_field() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    // Open search with `/`.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()),
    );
    assert!(state.search.is_some(), "search overlay should be open");

    // Type "tele" — should match Telemetry section fields.
    for ch in ['t', 'e', 'l', 'e'] {
        handle_key(
            &mut state,
            &mut agent,
            &mut q,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
    }
    let matches = state.search.as_ref().unwrap().matches.len();
    assert!(matches > 0, "fuzzy 'tele' should match Telemetry fields");

    // Enter jumps to the top match.
    let (target_sidx, target_fidx, _) = state.search.as_ref().unwrap().matches[0];
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    assert!(state.search.is_none(), "Enter closes search");
    assert_eq!(state.section_index, target_sidx);
    assert_eq!(state.field_index, target_fidx);
}

#[tokio::test]
async fn search_enter_lands_on_models_field_past_synthetic_key_row() {
    // Searching to a Models field at or after the synthetic API-key row must
    // resolve back to the intended field, not one display row too high.
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();

    let models_sidx = CONFIG_SECTIONS
        .iter()
        .position(|s| s.id == SectionId::Models)
        .expect("Models section exists");
    let target_fidx = field_index(SectionId::Models, &["model", "reasoning_effort"]);
    assert!(
        target_fidx >= 2,
        "reasoning_effort should sit at/after the synthetic key row for this test to be meaningful"
    );

    // Open search and type "reason" so Models → reasoning_effort is matched.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()),
    );
    for ch in ['r', 'e', 'a', 's', 'o', 'n'] {
        handle_key(
            &mut state,
            &mut agent,
            &mut q,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
    }

    // Point the cursor at the Models → reasoning_effort match regardless of
    // its rank among other "reason"-ish matches.
    let search = state.search.as_mut().expect("search overlay open");
    search.cursor = search
        .matches
        .iter()
        .position(|&(sidx, fidx, _)| sidx == models_sidx && fidx == target_fidx)
        .expect("reasoning_effort is among the matches for 'reason'");

    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );

    assert_eq!(state.section_index, models_sidx);
    assert!(
        !state.on_synthetic_api_key_row(),
        "Enter must not land on the synthetic API-key row"
    );
    let resolved = state.field_at_row(state.field_index).expect("real field");
    assert_eq!(
        resolved.toml_path, CONFIG_SECTIONS[models_sidx].fields[target_fidx].toml_path,
        "focused field should be reasoning_effort, not the row above it"
    );
}

#[tokio::test]
async fn ctrl_r_resets_field_to_default() {
    // Use a field whose schema declares no env_override, so the test stays
    // robust against other tests setting SQUEEZY_* env vars in parallel.
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = field_index(SectionId::Verbosity, &["tui", "response_verbosity"]);
    state.effective.tui.response_verbosity = squeezy_core::ResponseVerbosity::Verbose;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        state.effective.tui.response_verbosity,
        squeezy_core::ResponseVerbosity::Normal,
        "Ctrl+R should restore default Normal"
    );
}

#[tokio::test]
async fn enter_on_env_shadowed_field_emits_warning_instead_of_opening_editor() {
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::set_var("SQUEEZY_TELEMETRY", "off") };
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Telemetry));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    assert!(
        state.editor.is_none(),
        "env-shadowed field should not open editor"
    );
    let current = q.current().expect("warning notification queued");
    assert!(
        current.message.contains("SQUEEZY_TELEMETRY"),
        "warning should name the env var, got: {}",
        current.message
    );
    unsafe { std::env::remove_var("SQUEEZY_TELEMETRY") };
}

#[tokio::test]
async fn space_cycling_provider_resets_model_in_memory() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId};
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_PROVIDER");
        std::env::remove_var("SQUEEZY_MODEL");
    }
    let mut state = temp_config_state(Some(SId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    // provider is row 0
    state.field_index = 0;
    let model_before = match (CONFIG_SECTIONS[0].fields[1].get)(&state.effective) {
        FieldValue::String(s) => s,
        other => panic!("expected String model, got {other:?}"),
    };
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let model_after = match (CONFIG_SECTIONS[0].fields[1].get)(&state.effective) {
        FieldValue::String(s) => s,
        other => panic!("expected String model, got {other:?}"),
    };
    assert_ne!(
        model_before, model_after,
        "switching provider via Space must also reset the model to that provider's default"
    );
}

#[test]
fn secret_entry_editor_inserts_and_backspaces_multibyte() {
    let mut entry = SecretEntryState {
        env_var: "SQUEEZY_OPENAI_KEY".to_string(),
        provider_label: "OpenAI".to_string(),
        draft: String::new(),
        cursor: 0,
        reveal: false,
    };
    for c in ['s', 'k', '-', 'é'] {
        entry.insert_char(c);
    }
    assert_eq!(entry.draft, "sk-é");
    assert_eq!(entry.cursor, 4);
    assert_eq!(entry.char_len(), 4);
    entry.backspace();
    assert_eq!(entry.draft, "sk-");
    assert_eq!(entry.cursor, 3);
    // Cursor mid-string then insert should land between the existing chars.
    entry.cursor = 1;
    entry.insert_char('!');
    assert_eq!(entry.draft, "s!k-");
    assert_eq!(entry.cursor, 2);
    entry.wipe();
    assert!(entry.draft.is_empty());
    assert_eq!(entry.cursor, 0);
}

#[test]
fn reset_preview_reports_changed_fields() {
    use squeezy_core::{
        TierSource,
        config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId},
    };
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Permissions));
    // Synthesize a User tier that flips `permissions.read` from the
    // default `allow` to `deny`. The Reset preview for `User` must
    // include this field (`deny` → `allow`).
    let toml_text = "[permissions]\nread = \"deny\"\n";
    state.sources.user = Some(TierSource {
        path: std::path::PathBuf::from("/virtual/user.toml"),
        doc: toml_text.parse().expect("valid toml"),
    });
    // Bring `state.effective` in line with the tier so before == "deny".
    let perm_read = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SId::Permissions)
        .unwrap()
        .fields
        .iter()
        .find(|f| f.label == "read")
        .unwrap();
    (perm_read.set)(&mut state.effective, FieldValue::Enum("deny")).unwrap();

    let preview = state.reset_preview(ConfigScope::User);
    let entry = preview
        .iter()
        .find(|e| e.field_label == "read")
        .expect("preview should flag permissions.read as changing");
    assert_eq!(entry.before, "deny");
    assert_eq!(entry.after, "allow");
}

#[tokio::test]
async fn reset_section_enter_arms_confirmation_and_n_cancels() {
    use squeezy_core::config_schema::SectionId as SId;
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Reset));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    assert_eq!(state.field_index, 0);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    assert!(
        state.reset_confirm.is_some(),
        "Enter on a Reset row should arm the y/n confirmation"
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
    );
    assert!(
        state.reset_confirm.is_none(),
        "`n` should cancel the confirmation"
    );
}

#[test]
fn path_editor_commits_pathbuf() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldKind, FieldValue, SectionId as SId};
    let cache = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SId::Cache)
        .expect("Cache section registered");
    let root = cache
        .fields
        .iter()
        .find(|f| f.label == "root")
        .expect("root field");
    assert!(matches!(root.kind, FieldKind::Path { .. }));

    let mut editor = open_editor_for(root, FieldValue::Path(std::path::PathBuf::new()));
    for ch in ['/', 't', 'm', 'p', '/', 'c'] {
        let _ = handle_editor_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
    }
    let commit = handle_editor_key(
        &mut editor,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let p = match commit {
        EditorOutcome::Commit(FieldValue::Path(p)) => p,
        other => panic!("expected Path commit, got {:?}", other),
    };
    assert_eq!(p, std::path::PathBuf::from("/tmp/c"));
}

// ─── /config UX/UI eval fixture ───────────────────────────────────────────
//
// Companion to `crates/squeezy-eval/fixtures/scenarios/options-screen-routing.toml`
// (slash-router level) and to the `unknown_config_slug_…` tests in
// `lib_tests.rs` (TuiApp dispatch). The tests in this block drive
// `ConfigScreenState` directly to cover the rendering, key handling,
// and runtime-efficacy invariants identified by the /config UX audit.

fn render_screen_to_text(state: &ConfigScreenState, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            render(frame, area, state);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

#[test]
fn default_scope_is_user() {
    let state = ConfigScreenState::new(AppConfig::default(), None);
    assert_eq!(
        state.scope,
        ConfigScope::User,
        "first-open scope should be User (leftmost tab)"
    );
}

#[tokio::test]
async fn shift_x_arms_discard_confirmation_then_n_cancels() {
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "show_reasoning_usage"]);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert!(
        !state.undo_stack.is_empty(),
        "Space should have queued a write"
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
    );
    assert!(
        state.discard_confirm,
        "Shift+X should arm the discard-confirm overlay"
    );
    assert!(
        !state.undo_stack.is_empty(),
        "discard must NOT fire until y is pressed"
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
    );
    assert!(!state.discard_confirm, "'n' should clear the confirmation");
    assert!(
        !state.undo_stack.is_empty(),
        "cancelled discard must keep the undo stack intact"
    );
}

#[tokio::test]
async fn shift_x_on_empty_undo_stack_short_circuits_without_confirm() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    assert!(state.undo_stack.is_empty());
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
    );
    assert!(
        !state.discard_confirm,
        "no undo entries → no confirmation overlay"
    );
    let note = q.current().expect("info note");
    assert!(
        note.message.contains("Nothing to discard"),
        "empty-stack path should explain why, got: {}",
        note.message
    );
}

#[tokio::test]
async fn discard_confirm_y_wipes_session_writes() {
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "show_reasoning_usage"]);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert!(!state.undo_stack.is_empty());
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()),
    );
    assert!(!state.discard_confirm);
    assert!(
        state.undo_stack.is_empty(),
        "confirmed discard should clear the undo stack"
    );
}

#[test]
fn footer_documents_ctrl_r_ctrl_d_and_shift_x() {
    let state = ConfigScreenState::new(AppConfig::default(), None);
    let rendered = render_screen_to_text(&state, 120, 30);
    assert!(
        rendered.contains("Ctrl+R") && rendered.contains("reset"),
        "footer should advertise Ctrl+R reset-to-default"
    );
    assert!(
        rendered.contains("Ctrl+D") && rendered.contains("clear"),
        "footer should advertise Ctrl+D clear-override"
    );
    assert!(
        rendered.contains("Shift+X"),
        "discard binding should be labelled Shift+X, not bare X"
    );
}

#[test]
fn footer_documents_shift_tab_for_reverse_scope_cycle() {
    let state = ConfigScreenState::new(AppConfig::default(), None);
    let rendered = render_screen_to_text(&state, 120, 30);
    assert!(
        rendered.contains("Shift+Tab"),
        "footer should mention Shift+Tab as the reverse-scope chord"
    );
}

#[test]
fn env_source_badge_no_longer_uses_error_red() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    // Use SQUEEZY_SESSION_MODE (Modes section) so this test doesn't race
    // with `enter_on_env_shadowed_field_emits_warning_instead_of_opening_editor`,
    // which also flips SQUEEZY_TELEMETRY around the same time when the
    // test runner parallelises.
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::set_var("SQUEEZY_SESSION_MODE", "plan") };
    let state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Modes));
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, 120, 30);
            render(frame, area, &state);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut found_env_badge = false;
    let mut env_badge_colour = None;
    for y in 0..30u16 {
        let mut row = String::new();
        for x in 0..120u16 {
            row.push_str(buffer[(x, y)].symbol());
        }
        if let Some(start) = row.find("[env]") {
            found_env_badge = true;
            env_badge_colour = Some(buffer[(start as u16 + 1, y)].fg);
            break;
        }
    }
    unsafe { std::env::remove_var("SQUEEZY_SESSION_MODE") };
    assert!(
        found_env_badge,
        "env-shadowed field should render an [env] badge"
    );
    assert_ne!(
        env_badge_colour,
        Some(crate::render::theme::red()),
        "[env] badge must not be coloured as an error"
    );
}

#[test]
fn api_key_row_reports_env_var_presence() {
    // Construct an OpenAI provider explicitly so the api_key_env name
    // is deterministic — `AppConfig::default()` reads the user's real
    // `~/.squeezy/settings.toml` overlay and can yield a different
    // provider depending on whose machine runs the test.
    let custom_env = "SQUEEZY_OPTIONS_EVAL_OPENAI_KEY";
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::set_var(custom_env, "sk-test-from-env-XYZ") };
    let cfg = AppConfig {
        provider: squeezy_core::ProviderConfig::OpenAi(squeezy_core::OpenAiConfig {
            api_key_env: custom_env.to_string(),
            api_key: None,
            base_url: squeezy_core::DEFAULT_OPENAI_BASE_URL.to_string(),
            organization: None,
            project: None,
            service_tier: None,
            transport: Default::default(),
        }),
        ..AppConfig::default()
    };
    let mut state = ConfigScreenState::new(cfg, Some(SectionId::Models));
    state.field_index = 2; // synthetic API-key row
    let rendered = render_screen_to_text(&state, 120, 30);
    unsafe { std::env::remove_var(custom_env) };
    assert!(
        rendered.contains("[env · openai]") && rendered.contains("from environment"),
        "API-key row should advertise env-provided credentials, got:\n{rendered}"
    );
}

#[test]
fn api_key_row_reports_fallback_env_var() {
    // Audit 7.1: the row must reflect the *fallback* env var the runtime
    // honors (e.g. ANTHROPIC_API_KEY for the SQUEEZY_ANTHROPIC_KEY pair),
    // not only the canonical name. Previously a working fallback rendered
    // as "unset", so operators could not trust /config. Use a synthetic
    // provider pair so the test never collides with a real key on the
    // developer's machine.
    let canonical = "SQUEEZY_OPTIONS_EVAL_FALLBACK_KEY";
    let fallback = "OPTIONS_EVAL_FALLBACK_API_KEY";
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var(canonical);
        std::env::set_var(fallback, "sk-test-from-fallback-XYZ");
    }
    let cfg = AppConfig {
        provider: squeezy_core::ProviderConfig::OpenAi(squeezy_core::OpenAiConfig {
            api_key_env: canonical.to_string(),
            api_key: None,
            base_url: squeezy_core::DEFAULT_OPENAI_BASE_URL.to_string(),
            organization: None,
            project: None,
            service_tier: None,
            transport: Default::default(),
        }),
        ..AppConfig::default()
    };
    let mut state = ConfigScreenState::new(cfg, Some(SectionId::Models));
    state.field_index = 2; // synthetic API-key row
    let rendered = render_screen_to_text(&state, 120, 30);
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::remove_var(fallback) };
    assert!(
        rendered.contains("[env · openai]")
            && rendered.contains(fallback)
            && rendered.contains("from environment"),
        "API-key row should advertise the working fallback env var, got:\n{rendered}"
    );
}

#[tokio::test]
async fn model_picker_provider_swap_writes_once_not_twice() {
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_MODEL");
        std::env::remove_var("SQUEEZY_PROVIDER");
    }
    let mut state = temp_config_state(Some(SectionId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = 1;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let picker = state.picker.as_mut().expect("picker open");
    picker.all_providers = true;
    for ch in ['c', 'l', 'a', 'u', 'd'] {
        handle_key(
            &mut state,
            &mut agent,
            &mut q,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
        );
    }
    let before_writes = state.undo_stack.len();
    let provider_field = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Models)
        .and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.toml_path == ["model", "provider"])
        })
        .unwrap();
    let provider_before = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        _ => panic!("provider not Enum"),
    };
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let provider_after = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        _ => panic!("provider not Enum"),
    };
    if provider_before != provider_after {
        let writes_added = state.undo_stack.len() - before_writes;
        assert!(
            writes_added <= 1,
            "provider+model swap should issue at most one save_field write, got {writes_added}"
        );
    }
}

#[test]
fn search_overlay_clips_with_scroll_indicators() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut matches = Vec::new();
    for (sidx, section) in CONFIG_SECTIONS.iter().enumerate() {
        for (fidx, _field) in section.fields.iter().enumerate() {
            matches.push((sidx, fidx, 0));
        }
    }
    let total = matches.len();
    assert!(
        total > 20,
        "this fixture needs CONFIG_SECTIONS to have enough fields to overflow"
    );
    state.search = Some(SearchOverlayState {
        query: String::new(),
        cursor: total - 1,
        matches,
    });
    let rendered = render_screen_to_text(&state, 120, 18);
    assert!(
        rendered.contains("▲"),
        "scrolled-down overlay must show ▲ marker for hidden rows above"
    );
}

#[test]
fn model_picker_clips_with_scroll_indicators() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Models));
    state.picker = Some(ModelPickerState {
        filter: String::new(),
        cursor: 0,
        all_providers: true,
        current_provider: "openai",
    });
    let rendered = render_screen_to_text(&state, 120, 14);
    assert!(
        rendered.contains("▼"),
        "tight pane should show ▼ marker for entries below the window"
    );
}

#[tokio::test]
async fn ctrl_s_message_includes_undo_hint() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
    );
    let note = q.current().expect("ctrl+s should push a note");
    assert!(
        note.message.contains("Ctrl+Z"),
        "ctrl+s message should hint at the undo path, got: {}",
        note.message
    );
}

#[tokio::test]
async fn picker_open_path_locates_provider_field_by_toml_path() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Models));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = 1;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let picker = state.picker.as_ref().expect("picker should be open");
    let provider_field = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Models)
        .and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.toml_path == ["model", "provider"])
        })
        .expect("Models section must expose [model].provider");
    let resolved = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        other => panic!("provider field not Enum: {other:?}"),
    };
    assert_eq!(
        picker.current_provider, resolved,
        "picker should derive current_provider from the toml_path lookup, not hard-coded index"
    );
}

#[tokio::test]
async fn discard_confirm_overlay_renders_with_file_list() {
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "show_reasoning_usage"]);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
    );
    assert!(state.discard_confirm);
    let rendered = render_screen_to_text(&state, 120, 30);
    assert!(
        rendered.contains("Discard all session writes"),
        "overlay should announce itself, got:\n{rendered}"
    );
    assert!(
        rendered.contains("y") && rendered.contains("n"),
        "overlay should list y/n bindings, got:\n{rendered}"
    );
}

#[test]
fn sidebar_shows_more_below_marker_when_clipped() {
    let state = ConfigScreenState::new(AppConfig::default(), None);
    let rendered = render_screen_to_text(&state, 80, 10);
    assert!(
        rendered.contains("▼"),
        "short pane should show ▼ marker on the sidebar to flag clipping, got:\n{rendered}"
    );
}

#[test]
fn sidebar_keeps_late_sections_visible_before_reset() {
    for (section, label) in [
        (SectionId::Feedback, "Feedback"),
        (SectionId::Redaction, "Redaction"),
        (SectionId::Web, "Web"),
    ] {
        let state = ConfigScreenState::new(AppConfig::default(), Some(section));
        let rendered = render_screen_to_text(&state, 80, 14);
        assert!(
            rendered.contains(&format!("› {label}")),
            "sidebar should scroll before Reset so {label} is visibly selected, got:\n{rendered}"
        );
    }
}

#[test]
fn field_pane_keeps_active_row_visible_when_clipped() {
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    state.field_index = state.row_count() - 2;
    let active_label = state.current_field().label;
    let rendered = render_screen_to_text(&state, 120, 14);
    assert!(
        rendered.contains(&format!("› {active_label}")),
        "short field pane should scroll to the active row, got:\n{rendered}"
    );
    assert!(
        rendered.contains("▲"),
        "scrolled field pane should show rows hidden above, got:\n{rendered}"
    );
}

#[tokio::test]
async fn tab_from_default_user_scope_advances_to_repo() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    assert_eq!(state.scope, ConfigScope::User);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
    );
    assert_eq!(
        state.scope,
        ConfigScope::Repo,
        "Tab from the default User scope should advance to Repo"
    );
}

// ─── Runtime efficacy probes ───────────────────────────────────────────────
//
// "Does the option actually take effect after I change it." Each test
// exercises a field, then reads the agent's effective config (Immediate
// tier) or queued swap (NextPrompt) and asserts the new value
// propagated. Restart-tier fields only fire after a fresh process boot
// and are out of scope for unit tests.

#[tokio::test]
async fn immediate_tier_bool_save_propagates_to_agent() {
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "show_reasoning_usage"]);
    let before = agent.config_snapshot().tui.show_reasoning_usage;
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let after = agent.config_snapshot().tui.show_reasoning_usage;
    assert_ne!(
        before, after,
        "Immediate-tier Bool save must hot-swap into the agent's effective config"
    );
}

#[tokio::test]
async fn immediate_tier_enum_save_propagates_to_agent() {
    use squeezy_core::ResponseVerbosity;
    let mut state = temp_config_state(Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.field_index = field_index(SectionId::Verbosity, &["tui", "response_verbosity"]);
    state.effective.tui.response_verbosity = ResponseVerbosity::Normal;
    agent.replace_config(state.effective.clone());
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    let observed = agent.config_snapshot().tui.response_verbosity;
    assert_ne!(
        observed,
        ResponseVerbosity::Normal,
        "Space-cycle on response_verbosity must change the agent's live config"
    );
}

#[tokio::test]
async fn immediate_tier_permission_save_propagates_to_agent() {
    use squeezy_core::{PermissionMode, PermissionPolicyMode};
    let mut state = temp_config_state(Some(SectionId::Permissions));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    state.effective.permissions.mode = PermissionPolicyMode::Custom;
    state.field_index = field_index(SectionId::Permissions, &["permissions", "read"]);
    state.effective.permissions.read = PermissionMode::Allow;
    agent.replace_config(state.effective.clone());
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert_ne!(
        agent.config_snapshot().permissions.read,
        PermissionMode::Allow,
        "permission saves must hot-swap so the next tool call sees the new mode"
    );
}

#[test]
fn granular_permission_read_back_uses_custom_subtable() {
    use squeezy_core::{
        TierSource,
        config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId},
    };
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Permissions));
    let perm_edit = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SId::Permissions)
        .unwrap()
        .fields
        .iter()
        .find(|f| f.label == "edit")
        .unwrap();

    // The screen writes granular permissions to `[permissions.custom]`;
    // synthesize a Local tier in that modern shape with `edit = "deny"`.
    let toml_text = "[permissions]\nmode = \"custom\"\n\n[permissions.custom]\nedit = \"deny\"\n";
    let tier = TierSource {
        path: std::path::PathBuf::from("/virtual/local.toml"),
        doc: toml_text.parse().expect("valid toml"),
    };
    state.sources.repo = Some(tier);

    // Read-back on the Local tab must reflect the saved `deny`, not the
    // stale top-level default `allow`.
    state.scope = ConfigScope::Local;
    let (value, _src) = state.displayed_value_and_source(perm_edit);
    assert_eq!(
        value,
        FieldValue::Enum("deny"),
        "displayed value must read the `[permissions.custom]` write location"
    );

    // The tier owns the field, so Repo/Local Space-cycle can advance past
    // the first option instead of perpetually re-applying `allow`.
    assert_eq!(
        state.scope_owns_field(perm_edit),
        Some(true),
        "scope_owns_field must count the `[permissions.custom]` location"
    );
}

#[tokio::test]
async fn next_prompt_tier_save_arms_pending_swap() {
    use squeezy_core::config_schema::ApplyTier;
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::remove_var("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL") };
    let mut state = temp_config_state(Some(SectionId::Subagents));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    let section = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Subagents)
        .unwrap();
    let (idx, max_calls_field) = section
        .fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.toml_path == ["subagents", "max_tool_calls_per_call"])
        .expect("field exists");
    state.field_index = idx;
    assert!(
        matches!(max_calls_field.tier, ApplyTier::NextPrompt),
        "test fixture relies on this being NextPrompt"
    );
    let before = agent.config_snapshot().subagents.max_tool_calls_per_call;
    let previous = (max_calls_field.get)(&state.effective);
    let new_value = FieldValue::Integer((before as i64) + 1);
    (max_calls_field.set)(&mut state.effective, new_value.clone()).expect("set ok");
    save_field(
        &mut state,
        &mut agent,
        &mut q,
        max_calls_field,
        previous,
        new_value,
    );
    let swap = agent
        .pending_config_swap()
        .expect("NextPrompt save must arm a pending swap");
    assert_eq!(
        swap.config.subagents.max_tool_calls_per_call,
        before + 1,
        "queued swap must carry the new value"
    );
    assert_eq!(
        agent.config_snapshot().subagents.max_tool_calls_per_call,
        before,
        "NextPrompt saves must NOT mutate the live agent.config until drain_pending_swap fires"
    );
}

#[tokio::test]
async fn next_prompt_swap_applies_on_drain() {
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::remove_var("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL") };
    let mut state = temp_config_state(Some(SectionId::Subagents));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.scope = ConfigScope::User;
    let section = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Subagents)
        .unwrap();
    let (idx, field) = section
        .fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.toml_path == ["subagents", "max_tool_calls_per_call"])
        .unwrap();
    state.field_index = idx;
    let before = agent.config_snapshot().subagents.max_tool_calls_per_call;
    let previous = (field.get)(&state.effective);
    let new_value = FieldValue::Integer((before as i64) + 5);
    (field.set)(&mut state.effective, new_value.clone()).expect("set ok");
    save_field(&mut state, &mut agent, &mut q, field, previous, new_value);
    let drained = agent.drain_pending_swap();
    assert!(
        drained.is_some(),
        "drain_pending_swap should yield the armed swap"
    );
    assert_eq!(
        agent.config_snapshot().subagents.max_tool_calls_per_call,
        before + 5,
        "drained swap must apply the new value to the live config"
    );
    assert!(
        agent.pending_config_swap().is_none(),
        "drain should consume the queued swap"
    );
}

#[test]
fn local_tab_stays_visible_when_repo_subtitle_is_long() {
    // Regression for squeezy-5wu: a worktree-style repo path plus
    // `(committed)` used to push the Local tab off the right edge at
    // width=140 (the default eval terminal). The tab strip now budgets
    // subtitle width so the rightmost label is always present.
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    state.sources.user_path_default = std::path::PathBuf::from("/home/eval/.squeezy/settings.toml");
    state.sources.project_path_default = std::path::PathBuf::from(
        "/home/eval/esqueezy/squeezy/.claude/worktrees/agent-a8f9491133db8e8fa/squeezy.toml",
    );
    state.sources.repo_path_default = std::path::PathBuf::from(
        "/home/eval/.squeezy/projects/aaaabbbbccccddddeeeeffff00001111/settings.toml",
    );
    let rendered = render_screen_to_text(&state, 140, 30);
    // Header row 0 contains the tab strip; "User", "Repo", and "Local"
    // must all survive truncation. The previous bug rendered only the
    // first two labels at width=140.
    let header = rendered.lines().next().expect("header row");
    assert!(
        header.contains("User") && header.contains("Repo") && header.contains("Local"),
        "all three tab labels should fit on a 140-col row; got:\n{header}"
    );
}

#[test]
fn routing_section_shows_resolved_per_provider_defaults() {
    // Regression: the Routing rows rendered "—" because the render path
    // (`displayed_value_and_source`) fell through to the empty schema default
    // for the provider Info row and the `["providers","*",…]` fields instead of
    // resolving them against the active provider. With AppConfig::default()
    // (openai, nothing customized) the rows must show the built-in defaults.
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldKind, SectionId as SId};
    unsafe {
        std::env::remove_var("SQUEEZY_SMALL_FAST_MODEL");
        std::env::remove_var("SQUEEZY_ROUTING_JUDGE_MODEL");
    }
    let state = temp_config_state(Some(SId::Routing));
    let section = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SId::Routing)
        .expect("routing section exists");
    for field in section.fields {
        let (value, _src) = state.displayed_value_and_source(field);
        let shown = value.as_display();
        match field.toml_path {
            // The provider banner and the cheap/judge model + prompt resolve to
            // a concrete built-in value — never the "—" placeholder.
            ["routing", "_provider_info"]
            | ["providers", "*", "cheap_model"]
            | ["providers", "*", "judge_model"]
            | ["providers", "*", "judge_prompt"] => {
                assert_ne!(
                    shown, "—",
                    "Routing field {:?} must show a resolved default, got '—'",
                    field.toml_path
                );
                assert!(
                    !shown.is_empty(),
                    "Routing field {:?} empty",
                    field.toml_path
                );
            }
            // Resolves to the per-provider default reroute filter (a real regex
            // with a negative lookahead skipping cheap tiers) — never blank, and
            // exactly what the router applies.
            ["providers", "*", "expensive_models"] => {
                assert_ne!(shown, "—", "default reroute filter should not be blank");
                assert!(
                    shown.contains("(?!"),
                    "default reroute filter should be a negative-lookahead regex, got {shown}"
                );
            }
            _ => {}
        }
        // The provider banner is the only Info row; it must be non-editable.
        if field.toml_path == ["routing", "_provider_info"] {
            assert!(matches!(field.kind, FieldKind::Info));
        }
    }
}

#[test]
fn routing_pane_renders_default_filter_and_pinned_banner() {
    // The default reroute filter (a negative-lookahead regex skipping cheap
    // tiers) shows verbatim, the banner carries the pinned-provider note, and an
    // explicit empty filter renders as the friendly "any".
    use squeezy_core::config_schema::SectionId as SId;
    unsafe {
        std::env::remove_var("SQUEEZY_SMALL_FAST_MODEL");
        std::env::remove_var("SQUEEZY_ROUTING_JUDGE_MODEL");
        std::env::remove_var("SQUEEZY_ROUTING_EXPENSIVE_MODELS");
    }
    let mut state = temp_config_state(Some(SId::Routing));
    let rendered = render_screen_to_text(&state, 120, 24);
    assert!(
        rendered.contains("(?!"),
        "default reroute filter should show a negative-lookahead regex, got:\n{rendered}"
    );
    assert!(
        rendered.contains("pinned"),
        "provider banner should carry the pinned note, got:\n{rendered}"
    );

    // An explicit empty filter ("reroute any") renders as "any".
    let slug = active_provider_slug(&state.effective);
    state
        .effective
        .providers
        .entry(slug)
        .or_default()
        .expensive_models = Some(String::new());
    let rendered2 = render_screen_to_text(&state, 120, 24);
    assert!(
        rendered2.contains("any"),
        "explicit empty filter should render as 'any', got:\n{rendered2}"
    );
}

#[test]
fn prompt_editor_editing_and_line_navigation() {
    use super::PromptEditorState;
    let mut ed = PromptEditorState::new(String::new());
    for c in "ab".chars() {
        ed.insert_char(c);
    }
    ed.insert_char('\n');
    for c in "cd".chars() {
        ed.insert_char(c);
    }
    assert_eq!(ed.draft, "ab\ncd");
    // Cursor sits after 'd' (byte 5). Up lands on the first line, same column.
    ed.up();
    assert_eq!(ed.cursor, 2);
    ed.home();
    assert_eq!(ed.cursor, 0);
    ed.end();
    assert_eq!(ed.cursor, 2);
    ed.down();
    assert_eq!(ed.cursor, 5);
    ed.backspace();
    assert_eq!(ed.draft, "ab\nc");
    ed.left();
    ed.delete();
    assert_eq!(ed.draft, "ab\n");
}

#[tokio::test]
async fn enter_on_judge_prompt_opens_full_editor_then_ctrl_s_saves() {
    use squeezy_core::config_schema::SectionId as SId;
    let mut state = temp_config_state(Some(SId::Routing));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = field_index(SId::Routing, &["providers", "*", "judge_prompt"]);

    // Enter opens the full-screen editor pre-filled with the resolved built-in
    // prompt — NOT the inline single-line editor.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    let ed = state.prompt_editor.as_ref().expect("full editor open");
    assert!(
        !ed.draft.is_empty(),
        "editor seeds with the built-in judge prompt"
    );
    assert!(
        state.editor.is_none(),
        "multiline fields must not use the inline editor"
    );

    // Append a marker and save with Ctrl+S.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::empty()),
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
    );
    assert!(state.prompt_editor.is_none(), "Ctrl+S closes the editor");
    let provider = active_provider_slug(&state.effective);
    let saved = state
        .effective
        .providers
        .get(&provider)
        .and_then(|p| p.judge_prompt.clone())
        .expect("custom judge prompt stored as a per-provider override");
    assert!(saved.ends_with('!'));
}

#[tokio::test]
async fn esc_on_judge_prompt_editor_discards_edits() {
    use squeezy_core::config_schema::SectionId as SId;
    let mut state = temp_config_state(Some(SId::Routing));
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    state.field_index = field_index(SId::Routing, &["providers", "*", "judge_prompt"]);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::empty()),
    );
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
    );
    assert!(state.prompt_editor.is_none(), "Esc closes the editor");
    let provider = active_provider_slug(&state.effective);
    assert!(
        state
            .effective
            .providers
            .get(&provider)
            .and_then(|p| p.judge_prompt.clone())
            .is_none(),
        "Esc must not persist the edit"
    );
}

#[test]
fn permissions_visible_rows_reveal_reviewer_under_auto_review() {
    // Field count for the Permissions section (mode + 2 reviewer + caps).
    let total = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Permissions)
        .unwrap()
        .fields
        .len();
    // Default / Full Access expose only the mode row.
    assert_eq!(
        permissions_visible_rows(PermissionPolicyMode::Default, total),
        1
    );
    assert_eq!(
        permissions_visible_rows(PermissionPolicyMode::FullAccess, total),
        1
    );
    // Auto-review adds the two reviewer rows.
    assert_eq!(
        permissions_visible_rows(PermissionPolicyMode::AutoReview, total),
        1 + PERMISSION_REVIEWER_ROWS
    );
    // Custom exposes every per-capability row.
    assert_eq!(
        permissions_visible_rows(PermissionPolicyMode::Custom, total),
        total
    );
}

#[test]
fn permission_mode_change_reveals_reviewer_rows_immediately() {
    // State-level: flipping the effective mode changes the visible row count on
    // the very next query — no navigation needed.
    let mut state = temp_config_state(Some(SectionId::Permissions));
    assert_eq!(
        state.effective.permissions.mode,
        PermissionPolicyMode::Default
    );
    assert_eq!(state.row_count(), 1, "default mode shows only the mode row");

    let mode_field = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Permissions)
        .unwrap()
        .fields
        .iter()
        .find(|f| f.label == "mode")
        .unwrap();
    (mode_field.set)(&mut state.effective, FieldValue::Enum("auto_review")).unwrap();
    assert_eq!(
        state.effective.permissions.mode,
        PermissionPolicyMode::AutoReview
    );
    assert_eq!(state.row_count(), 1 + PERMISSION_REVIEWER_ROWS);
}

#[test]
fn cycling_permission_mode_via_key_expands_rows() {
    let mut state = temp_config_state(Some(SectionId::Permissions));
    state.field_index = 0; // the `mode` row
    let mut agent = make_agent();
    let mut q = ConfigFeedback::new();
    assert_eq!(state.row_count(), 1);

    // Space cycles the mode enum: default -> auto_review.
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    assert_eq!(
        state.effective.permissions.mode,
        PermissionPolicyMode::AutoReview,
        "space must cycle the mode to auto_review"
    );
    assert_eq!(
        state.row_count(),
        1 + PERMISSION_REVIEWER_ROWS,
        "cycling mode must reveal the reviewer rows in the same frame"
    );
}

#[test]
fn reviewer_rows_display_resolved_values_not_dashes() {
    // Regression: reviewer rows used to show "—" because the display fell back
    // to the static (empty) default instead of the running effective value.
    let mut state = temp_config_state(Some(SectionId::Permissions));
    state
        .effective
        .permissions
        .apply_mode(PermissionPolicyMode::AutoReview);

    let perms = CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == SectionId::Permissions)
        .unwrap();
    let field = |label: &str| perms.fields.iter().find(|f| f.label == label).unwrap();

    // The capability remit shows the active set, not a bare dash.
    let (caps, _) = state.displayed_value_and_source(field("reviewer_capabilities"));
    let caps_str = caps.as_display();
    assert!(
        caps_str.contains("edit") && caps_str.contains("shell"),
        "reviewer_capabilities should list the active set, got {caps_str:?}"
    );

    // The model shows the resolved model, never empty/dash.
    let (model, _) = state.displayed_value_and_source(field("reviewer_model"));
    let model_str = model.as_display();
    assert!(
        !model_str.is_empty() && model_str != "—",
        "reviewer_model should resolve to a real model, got {model_str:?}"
    );
}

#[test]
fn reviewer_rows_visible_when_saved_mode_diverges_from_snapshot() {
    use squeezy_core::TierSource;
    // The agent snapshot (effective) lags at the shipped `default`, but the
    // saved settings file says auto_review — the divergence that hid the
    // reviewer rows on open. Row visibility tracks the displayed (saved) mode.
    let mut state = temp_config_state(Some(SectionId::Permissions));
    assert_eq!(
        state.effective.permissions.mode,
        PermissionPolicyMode::Default
    );
    state.sources.user = Some(TierSource {
        path: std::path::PathBuf::from("/virtual/user.toml"),
        doc: "[permissions]\nmode = \"auto_review\"\n"
            .parse()
            .expect("valid toml"),
    });
    assert_eq!(
        state.row_count(),
        1 + PERMISSION_REVIEWER_ROWS,
        "reviewer rows must show when the saved mode is auto_review, even if the snapshot lags"
    );
    assert_eq!(
        state.field_at_row(state.row_count() - 1).map(|f| f.label),
        Some("reviewer_capabilities")
    );
}
