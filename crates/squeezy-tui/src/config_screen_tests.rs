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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Telemetry));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    let before = state.effective.telemetry.enabled;
    // first field in Telemetry section is `enabled` (Bool).
    handle_key(
        &mut state,
        &mut agent,
        &mut q,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
    );
    // Toggling will try to save (which writes to disk) — to keep the test
    // hermetic we only assert the in-memory effective config flipped.
    assert_ne!(state.effective.telemetry.enabled, before);
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
