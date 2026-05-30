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
fn tab_cycles_through_three_scopes() {
    let mut state = ConfigScreenState::new(AppConfig::default(), None);
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    state.scope = ConfigScope::User;
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Models));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Models));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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

#[tokio::test]
async fn space_cycling_provider_resets_model_in_memory() {
    use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldValue, SectionId as SId};
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_PROVIDER");
        std::env::remove_var("SQUEEZY_MODEL");
    }
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SId::Models));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.field_index = 4; // show_reasoning_usage (Bool, no env_override)
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.field_index = 4;
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
    use crate::render::palette::ERROR_RED;
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
        Some(ERROR_RED),
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

#[tokio::test]
async fn model_picker_provider_swap_writes_once_not_twice() {
    // SAFETY: tests in this module run single-threaded.
    unsafe {
        std::env::remove_var("SQUEEZY_MODEL");
        std::env::remove_var("SQUEEZY_PROVIDER");
    }
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Models));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let mut q = NotificationQueue::new();
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.field_index = 4;
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
    let mut q = NotificationQueue::new();
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.field_index = 4;
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Verbosity));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.field_index = 0;
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Permissions));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
    state.scope = ConfigScope::User;
    state.effective.permissions.mode = PermissionPolicyMode::Custom;
    state.field_index = 1;
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

#[tokio::test]
async fn next_prompt_tier_save_arms_pending_swap() {
    use squeezy_core::config_schema::ApplyTier;
    // SAFETY: tests in this module run single-threaded.
    unsafe { std::env::remove_var("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL") };
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Subagents));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let new_value = FieldValue::Integer((before as i64) + 1);
    (max_calls_field.set)(&mut state.effective, new_value.clone()).expect("set ok");
    save_field(&mut state, &mut agent, &mut q, max_calls_field, new_value);
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
    let mut state = ConfigScreenState::new(AppConfig::default(), Some(SectionId::Subagents));
    let mut agent = make_agent();
    let mut q = NotificationQueue::new();
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
    let new_value = FieldValue::Integer((before as i64) + 5);
    (field.set)(&mut state.effective, new_value.clone()).expect("set ok");
    save_field(&mut state, &mut agent, &mut q, field, new_value);
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
