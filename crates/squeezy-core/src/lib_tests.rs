use super::*;

#[test]
fn turn_id_displays_stably() {
    assert_eq!(TurnId::new(42).to_string(), "turn-42");
}

#[test]
fn transcript_constructors_set_roles() {
    assert_eq!(TranscriptItem::user("hello").role, Role::User);
    assert_eq!(TranscriptItem::assistant("hi").role, Role::Assistant);
    assert_eq!(TranscriptItem::system("rules").role, Role::System);
}

#[test]
fn source_span_contains_byte_inclusively() {
    let span = SourceSpan::new(10, 20, SourcePoint::new(1, 0), SourcePoint::new(1, 10));

    assert!(!span.contains_byte(9));
    assert!(span.contains_byte(10));
    assert!(span.contains_byte(20));
    assert!(!span.contains_byte(21));
}

#[test]
fn config_without_env_uses_openai_provider_defaults() {
    let config = AppConfig::from_env_vars(|_| None);
    assert_eq!(config.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(config.max_output_tokens, Some(DEFAULT_MAX_OUTPUT_TOKENS));
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key_env, "OPENAI_API_KEY");
            assert_eq!(openai.base_url, DEFAULT_OPENAI_BASE_URL);
        }
    }
}

#[test]
fn config_reads_supported_env_overrides() {
    let config = AppConfig::from_env_vars(|name| match name {
        "SQUEEZY_MODEL" => Some("custom-model".to_string()),
        "OPENAI_BASE_URL" => Some("https://example.test/v1".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "custom-model");
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.base_url, "https://example.test/v1");
        }
    }
}
