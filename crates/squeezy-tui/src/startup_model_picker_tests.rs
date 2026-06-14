use super::*;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

fn choices(reasoning_effort: bool) -> Vec<StartupModelPickerProvider> {
    vec![StartupModelPickerProvider {
        label: "Anthropic via ANTHROPIC_API_KEY".to_string(),
        credential: StartupProviderCredential::Configured,
        models: vec![
            StartupModelPickerModel {
                label: "claude-haiku".to_string(),
                reasoning_effort: false,
            },
            StartupModelPickerModel {
                label: "claude-sonnet".to_string(),
                reasoning_effort,
            },
        ],
    }]
}

fn themes() -> Vec<StartupThemeChoice> {
    vec![
        StartupThemeChoice {
            name: "default".to_string(),
            label: "default".to_string(),
            action: StartupThemeAction::Select,
        },
        StartupThemeChoice {
            name: "bright".to_string(),
            label: "bright".to_string(),
            action: StartupThemeAction::Select,
        },
    ]
}

fn rendered_line_text(line: Line<'_>) -> String {
    line.spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

#[test]
fn theme_is_first_and_enter_applies_before_provider() {
    let mut state = StartupModelPickerState::new(themes(), choices(false), "default", 0);

    assert_eq!(state.step, PickerStep::Theme);
    state.dispatch(press(KeyCode::Down));
    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::ApplyTheme("bright".to_string()))
    );
    assert_eq!(state.step, PickerStep::Provider);
}

#[test]
fn moving_theme_cursor_previews_without_advancing() {
    let mut state = StartupModelPickerState::new(themes(), choices(false), "default", 0);

    assert_eq!(
        state.dispatch(press(KeyCode::Down)),
        Some(PickerOutcome::PreviewTheme("bright".to_string()))
    );
    assert_eq!(state.step, PickerStep::Theme);
}

#[test]
fn enter_advances_provider_then_finishes_model_without_reasoning() {
    let mut state = StartupModelPickerState::new(themes(), choices(false), "default", 0);

    state.dispatch(press(KeyCode::Enter)); // theme
    assert_eq!(state.step, PickerStep::Provider);
    assert!(state.dispatch(press(KeyCode::Enter)).is_none());
    assert_eq!(state.step, PickerStep::Model);
    state.dispatch(press(KeyCode::Down));

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            theme: "default".to_string(),
            provider_index: 0,
            model_index: 1,
            reasoning_effort: None,
            open_theme_config: false,
            open_model_config: false,
        }))
    );
}

#[test]
fn reasoning_capable_model_prompts_for_effort_before_finish() {
    let mut state = StartupModelPickerState::new(themes(), choices(true), "default", 0);
    state.dispatch(press(KeyCode::Enter)); // theme
    state.dispatch(press(KeyCode::Enter)); // provider
    state.dispatch(press(KeyCode::Down));

    assert!(state.dispatch(press(KeyCode::Enter)).is_none());
    assert_eq!(state.step, PickerStep::Reasoning);
    state.dispatch(press(KeyCode::Down));

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            theme: "default".to_string(),
            provider_index: 0,
            model_index: 1,
            reasoning_effort: Some(ReasoningEffort::High),
            open_theme_config: false,
            open_model_config: false,
        }))
    );
}

#[test]
fn left_returns_to_previous_question() {
    let mut state = StartupModelPickerState::new(themes(), choices(true), "default", 0);
    state.dispatch(press(KeyCode::Enter)); // theme
    state.dispatch(press(KeyCode::Enter)); // provider
    assert_eq!(state.step, PickerStep::Model);

    state.dispatch(press(KeyCode::Left));

    assert_eq!(state.step, PickerStep::Provider);
}

#[test]
fn theme_question_does_not_preview_future_provider_or_model_fields() {
    let state = StartupModelPickerState::new(themes(), choices(false), "default", 0);

    let question = rendered_line_text(render_question_line(&state));
    let summary = rendered_line_text(render_selection_summary(&state));

    assert_eq!(question, "Step 1 of 3 Choose a theme");
    assert!(!question.contains("Provider"));
    assert!(!question.contains("Model"));
    assert_eq!(summary, "theme default");
}

#[test]
fn setup_progress_excludes_deferred_resume_question() {
    let mut state = StartupModelPickerState::new(themes(), choices(true), "default", 1);
    state.dispatch(press(KeyCode::Enter)); // theme
    state.dispatch(press(KeyCode::Enter)); // provider
    state.dispatch(press(KeyCode::Down)); // reasoning-capable model
    state.dispatch(press(KeyCode::Enter));

    assert_eq!(state.step, PickerStep::Reasoning);
    assert_eq!(
        rendered_line_text(render_question_line(&state)),
        "Step 4 of 4 Choose reasoning effort  · then 1 more question"
    );
}

#[test]
fn custom_theme_row_continues_then_opens_config_after_selection() {
    let mut themes = themes();
    themes.push(StartupThemeChoice {
        name: String::new(),
        label: "Pick a theme later in the config screen".to_string(),
        action: StartupThemeAction::ConfigureInConfig,
    });
    let mut state = StartupModelPickerState::new(themes, choices(false), "default", 0);

    state.dispatch(press(KeyCode::Down));
    state.dispatch(press(KeyCode::Down));

    assert!(state.dispatch(press(KeyCode::Enter)).is_none());
    assert_eq!(state.step, PickerStep::Provider);
    state.dispatch(press(KeyCode::Enter));

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            theme: "default".to_string(),
            provider_index: 0,
            model_index: 0,
            reasoning_effort: None,
            open_theme_config: true,
            open_model_config: false,
        }))
    );
}

#[test]
fn provider_without_key_adds_key_question_and_marks_config_followup() {
    let mut choices = choices(false);
    choices[0].credential = StartupProviderCredential::NeedsConfig {
        env_var: "ANTHROPIC_API_KEY".to_string(),
    };
    let mut state = StartupModelPickerState::new(themes(), choices, "default", 0);
    state.dispatch(press(KeyCode::Enter)); // theme

    assert_eq!(
        rendered_line_text(render_question_line(&state)),
        "Step 2 of 4 Choose a provider"
    );
    state.dispatch(press(KeyCode::Enter)); // provider
    assert_eq!(state.step, PickerStep::Key);
    assert_eq!(
        render_choice_rows(&state, 3)
            .into_iter()
            .map(rendered_line_text)
            .collect::<Vec<_>>(),
        vec!["▸ Set ANTHROPIC_API_KEY later -- the config screen opens after setup"]
    );
    state.dispatch(press(KeyCode::Enter)); // key
    assert_eq!(state.step, PickerStep::Model);

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            theme: "default".to_string(),
            provider_index: 0,
            model_index: 0,
            reasoning_effort: None,
            open_theme_config: false,
            open_model_config: true,
        }))
    );
}

#[test]
fn provider_scroll_window_keeps_active_ollama_row_visible() {
    let mut providers = (0..7)
        .map(|index| StartupModelPickerProvider {
            label: format!("Provider {index}"),
            credential: StartupProviderCredential::Configured,
            models: vec![StartupModelPickerModel {
                label: "model".to_string(),
                reasoning_effort: false,
            }],
        })
        .collect::<Vec<_>>();
    providers.push(StartupModelPickerProvider {
        label: "Ollama local (http://127.0.0.1:11434)".to_string(),
        credential: StartupProviderCredential::NotRequired,
        models: vec![StartupModelPickerModel {
            label: "gpt-oss:20b (local default)".to_string(),
            reasoning_effort: false,
        }],
    });
    let mut state = StartupModelPickerState::new(themes(), providers, "default", 0);
    state.step = PickerStep::Provider;
    state.provider_cursor = state.choices.len() - 1;

    let rows = render_choice_rows(&state, 5)
        .into_iter()
        .map(rendered_line_text)
        .collect::<Vec<_>>();

    assert!(rows.len() <= 5);
    assert!(rows.iter().any(|row| row.contains("Ollama local")));
    assert!(rows.last().is_some_and(|row| row.contains("Ollama local")));
}

#[test]
fn height_two_middle_cursor_signals_more_below() {
    let (start, end, show_above, show_below) = visible_window(5, 2, 2);

    assert_eq!((start, end), (2, 3));
    assert!(!show_above);
    assert!(show_below);
}

#[test]
fn height_two_last_cursor_signals_more_above() {
    let (start, end, show_above, show_below) = visible_window(5, 4, 2);

    assert_eq!((start, end), (4, 5));
    assert!(show_above);
    assert!(!show_below);
}
