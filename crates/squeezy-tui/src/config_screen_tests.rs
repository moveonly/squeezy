use super::*;
use std::sync::Arc;

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
fn tab_cycles_scope() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    assert_eq!(state.scope, ConfigScope::User);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
    );
    assert_eq!(state.scope, ConfigScope::Project);
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
    );
    assert_eq!(state.scope, ConfigScope::User);
}

#[test]
fn arrow_keys_navigate_sections_and_fields() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    // show_reasoning_usage is at index 4 in Verbosity: env_override=None, Bool.
    state.field_index = 4;
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
    let mut q = NotificationQueue::new();
    // The `provider` field is index 0; the `model` field is index 1.
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
    let mut q = NotificationQueue::new();
    state.field_index = 1; // model field
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
async fn space_cycles_enum_field_to_next_option() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId};
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_PROVIDER");
    }
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Models));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    assert_ne!(before, after, "Space should advance enum to a different value");
}

#[tokio::test]
async fn slash_opens_search_and_enter_jumps_to_field() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
async fn ctrl_r_resets_field_to_default() {
    // Use a field whose schema declares no env_override, so the test stays
    // robust against other tests setting SQUEEZY_* env vars in parallel.
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    // response_verbosity is index 0: env_override=None, default=Normal.
    state.field_index = 0;
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
    let mut q = NotificationQueue::new();
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

#[test]
fn notification_dismiss_current_and_clear_all() {
    let mut q = NotificationQueue::new();
    q.push("a", crate::notification::Severity::Info);
    q.push("b", crate::notification::Severity::Info);
    q.push("c", crate::notification::Severity::Info);
    assert_eq!(q.len(), 3);
    assert!(q.dismiss_current());
    assert_eq!(q.len(), 2);
    let removed = q.clear_all();
    assert_eq!(removed, 2);
    assert!(q.is_empty());
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
