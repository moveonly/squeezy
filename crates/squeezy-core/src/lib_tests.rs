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
fn config_without_env_uses_openai_provider_defaults() {
    let config = AppConfig::from_env_vars(|_| None);
    assert_eq!(config.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(config.max_output_tokens, Some(DEFAULT_MAX_OUTPUT_TOKENS));
    assert_eq!(config.permissions, PermissionPolicy::default());
    assert!(!config.store_responses);
    assert_eq!(config.max_parallel_tools, 8);
    assert_eq!(config.exa_mcp_url, DEFAULT_EXA_MCP_URL);
    assert_eq!(config.exa_api_key_env, DEFAULT_EXA_API_KEY_ENV);
    assert_eq!(
        config.max_tool_result_bytes_per_round,
        DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND
    );
    assert_eq!(
        config.tool_spill_threshold_bytes,
        DEFAULT_TOOL_SPILL_THRESHOLD_BYTES
    );
    assert_eq!(config.tool_preview_bytes, DEFAULT_TOOL_PREVIEW_BYTES);
    assert_eq!(
        config.tool_output_retention_days,
        DEFAULT_TOOL_OUTPUT_RETENTION_DAYS
    );
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key_env, "OPENAI_API_KEY");
            assert_eq!(openai.base_url, DEFAULT_OPENAI_BASE_URL);
        }
        ProviderConfig::Anthropic(_) => panic!("expected OpenAI provider"),
    }
}

#[test]
fn config_reads_supported_env_overrides() {
    let config = AppConfig::from_env_vars(|name| match name {
        "SQUEEZY_MODEL" => Some("custom-model".to_string()),
        "OPENAI_BASE_URL" => Some("https://example.test/v1".to_string()),
        "SQUEEZY_EDIT_PERMISSION" => Some("allow".to_string()),
        "SQUEEZY_SHELL_PERMISSION" => Some("deny".to_string()),
        "SQUEEZY_STORE_RESPONSES" => Some("true".to_string()),
        "SQUEEZY_MAX_PARALLEL_TOOLS" => Some("3".to_string()),
        "SQUEEZY_WEB_PERMISSION" => Some("allow".to_string()),
        "SQUEEZY_EXA_MCP_URL" => Some("https://search.example/mcp".to_string()),
        "SQUEEZY_EXA_API_KEY_ENV" => Some("CUSTOM_EXA_KEY".to_string()),
        "SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES" => Some("1234".to_string()),
        "SQUEEZY_TOOL_PREVIEW_BYTES" => Some("456".to_string()),
        "SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND" => Some("7890".to_string()),
        "SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS" => Some("2".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "custom-model");
    assert_eq!(config.permissions.edit, PermissionMode::Allow);
    assert_eq!(config.permissions.shell, PermissionMode::Deny);
    assert_eq!(config.permissions.web, PermissionMode::Allow);
    assert!(config.store_responses);
    assert_eq!(config.max_parallel_tools, 3);
    assert_eq!(config.exa_mcp_url, "https://search.example/mcp");
    assert_eq!(config.exa_api_key_env, "CUSTOM_EXA_KEY");
    assert_eq!(config.tool_spill_threshold_bytes, 1234);
    assert_eq!(config.tool_preview_bytes, 456);
    assert_eq!(config.max_tool_result_bytes_per_round, 7890);
    assert_eq!(config.tool_output_retention_days, 2);
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.base_url, "https://example.test/v1");
        }
        ProviderConfig::Anthropic(_) => panic!("expected OpenAI provider"),
    }
}

#[test]
fn config_can_select_anthropic_provider_defaults() {
    let config = AppConfig::from_env_vars(|name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(config.model, DEFAULT_ANTHROPIC_MODEL);
    match config.provider {
        ProviderConfig::Anthropic(anthropic) => {
            assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
            assert_eq!(anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        }
        ProviderConfig::OpenAi(_) => panic!("expected Anthropic provider"),
    }
}

#[test]
fn config_reads_anthropic_env_overrides() {
    let config = AppConfig::from_env_vars(|name| match name {
        "SQUEEZY_PROVIDER" => Some("claude".to_string()),
        "SQUEEZY_MODEL" => Some("claude-test".to_string()),
        "ANTHROPIC_BASE_URL" => Some("https://anthropic.example.test/v1".to_string()),
        "SQUEEZY_STORE_RESPONSES" => Some("true".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "claude-test");
    assert!(!config.store_responses);
    match config.provider {
        ProviderConfig::Anthropic(anthropic) => {
            assert_eq!(anthropic.base_url, "https://anthropic.example.test/v1");
        }
        ProviderConfig::OpenAi(_) => panic!("expected Anthropic provider"),
    }
}

#[test]
fn permission_mode_parses_expected_values() {
    assert_eq!(PermissionMode::parse("allow"), Some(PermissionMode::Allow));
    assert_eq!(PermissionMode::parse("ASK"), Some(PermissionMode::Ask));
    assert_eq!(PermissionMode::parse("deny"), Some(PermissionMode::Deny));
    assert_eq!(PermissionMode::parse("maybe"), None);
}
