use squeezy_core::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_BEDROCK_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_OPENAI_MODEL,
    resolve_model_alias,
};

#[test]
fn resolve_opus_alias_for_anthropic() {
    assert_eq!(
        resolve_model_alias("anthropic", "opus"),
        Some(DEFAULT_ANTHROPIC_MODEL),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "OPUS"),
        Some("claude-opus-4-7")
    );
    assert_eq!(
        resolve_model_alias("anthropic", " opus "),
        Some("claude-opus-4-7"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "sonnet"),
        Some("claude-sonnet-4-6"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "haiku"),
        Some("claude-haiku-4-5-20251001"),
    );
}

#[test]
fn resolve_opus_alias_for_openai_returns_flagship() {
    assert_eq!(
        resolve_model_alias("openai", "opus"),
        Some(DEFAULT_OPENAI_MODEL)
    );
    assert_eq!(
        resolve_model_alias("openai", "best"),
        Some(DEFAULT_OPENAI_MODEL)
    );
    assert_eq!(
        resolve_model_alias("openai", "sonnet"),
        Some("gpt-5.4-mini")
    );
    assert_eq!(resolve_model_alias("openai", "haiku"), Some("gpt-5.4-nano"));
}

#[test]
fn resolve_alias_passes_through_full_ids_and_unknown_inputs() {
    assert_eq!(resolve_model_alias("anthropic", "claude-opus-4-7"), None);
    assert_eq!(resolve_model_alias("openai", "gpt-5.5"), None);
    assert_eq!(resolve_model_alias("anthropic", "opusplan"), None);
    assert_eq!(resolve_model_alias("ollama", "opus"), None);
    assert_eq!(resolve_model_alias("openrouter", "opus"), None);
}

#[test]
fn resolve_alias_for_bedrock_and_google() {
    assert_eq!(
        resolve_model_alias("bedrock", "opus"),
        Some(DEFAULT_BEDROCK_MODEL)
    );
    assert_eq!(
        resolve_model_alias("bedrock", "haiku"),
        Some(DEFAULT_BEDROCK_MODEL)
    );
    assert_eq!(
        resolve_model_alias("google", "opus"),
        Some(DEFAULT_GOOGLE_MODEL)
    );
    assert_eq!(
        resolve_model_alias("google", "haiku"),
        Some("gemini-2.5-flash-lite")
    );
}
