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

#[test]
fn enter_advances_provider_then_finishes_model_without_reasoning() {
    let mut state = StartupModelPickerState::new(choices(false));

    assert_eq!(state.step, PickerStep::Provider);
    assert!(state.dispatch(press(KeyCode::Enter)).is_none());
    assert_eq!(state.step, PickerStep::Model);
    state.dispatch(press(KeyCode::Down));

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            provider_index: 0,
            model_index: 1,
            reasoning_effort: None,
        }))
    );
}

#[test]
fn reasoning_capable_model_prompts_for_effort_before_finish() {
    let mut state = StartupModelPickerState::new(choices(true));
    state.dispatch(press(KeyCode::Enter));
    state.dispatch(press(KeyCode::Down));

    assert!(state.dispatch(press(KeyCode::Enter)).is_none());
    assert_eq!(state.step, PickerStep::Reasoning);
    state.dispatch(press(KeyCode::Down));

    assert_eq!(
        state.dispatch(press(KeyCode::Enter)),
        Some(PickerOutcome::Selected(StartupModelPickerSelection {
            provider_index: 0,
            model_index: 1,
            reasoning_effort: Some(ReasoningEffort::High),
        }))
    );
}

#[test]
fn left_returns_to_previous_question() {
    let mut state = StartupModelPickerState::new(choices(true));
    state.dispatch(press(KeyCode::Enter));
    assert_eq!(state.step, PickerStep::Model);

    state.dispatch(press(KeyCode::Left));

    assert_eq!(state.step, PickerStep::Provider);
}
