use std::sync::atomic::AtomicU64;

use super::*;

static CONFIG_TEST_NONCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn turn_id_displays_stably() {
    assert_eq!(TurnId::new(42).to_string(), "turn-42");
}

#[test]
fn default_instructions_do_not_reference_hidden_task_state_tool() {
    assert!(!DEFAULT_INSTRUCTIONS.contains("update_task_state"));
}

#[test]
fn default_instructions_keep_ui_rendering_contract_out_of_prompt() {
    assert!(!DEFAULT_INSTRUCTIONS.contains("Do not repeat raw tool output"));
    assert!(!DEFAULT_INSTRUCTIONS.contains("command in cwd"));
    assert!(!DEFAULT_INSTRUCTIONS.contains("Markdown fences"));
}

#[test]
fn transcript_constructors_set_roles() {
    assert_eq!(TranscriptItem::user("hello").role, Role::User);
    assert_eq!(TranscriptItem::assistant("hi").role, Role::Assistant);
    assert_eq!(TranscriptItem::system("rules").role, Role::System);
}

#[test]
fn context_attachment_detection_handles_common_text_artifacts() {
    assert_eq!(
        detect_context_attachment_kind(
            Some("panic.txt"),
            b"thread 'main' panicked\nstack backtrace:\n 0: foo\n",
            Some("thread 'main' panicked\nstack backtrace:\n 0: foo\n")
        ),
        ContextAttachmentKind::StackTrace
    );
    assert_eq!(
        detect_context_attachment_kind(
            Some("server.log"),
            b"2026-05-24 ERROR failed\n2026-05-24 WARN retry\n",
            Some("2026-05-24 ERROR failed\n2026-05-24 WARN retry\n")
        ),
        ContextAttachmentKind::Log
    );
    assert_eq!(
        detect_context_attachment_kind(
            Some("settings.toml"),
            b"provider = \"openai\"\nmodel = \"gpt-test\"\n",
            Some("provider = \"openai\"\nmodel = \"gpt-test\"\n")
        ),
        ContextAttachmentKind::Config
    );
}

#[test]
fn context_attachment_detection_rejects_binary_and_images() {
    assert_eq!(
        detect_context_attachment_kind(Some("screenshot.png"), b"\x89PNG\r\n\x1a\nbytes", None),
        ContextAttachmentKind::UnsupportedImage
    );
    assert_eq!(
        detect_context_attachment_kind(Some("blob.bin"), b"abc\0def", Some("abc\0def")),
        ContextAttachmentKind::UnsupportedBinary
    );
}

#[test]
fn context_attachment_preview_respects_utf8_boundary() {
    let (preview, truncated) = context_attachment_preview("abécd", 4);

    assert_eq!(preview, "abé");
    assert!(truncated);
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
    let config = AppConfig::from_env_vars(None, |_| None);
    assert_eq!(config.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(config.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(config.permissions, PermissionPolicy::default());
    assert_eq!(config.permissions.edit, PermissionMode::Allow);
    assert!(!config.permissions.ai_reviewer.enabled);
    assert_eq!(
        config.permissions.ai_reviewer.allow_capabilities,
        vec![PermissionCapability::Read, PermissionCapability::Search]
    );
    assert_eq!(
        config.permissions.shell_sandbox.protected_metadata_names,
        [".git", ".squeezy", ".agents"]
    );
    assert_eq!(config.session_mode, SessionMode::Build);
    assert!(config.hardening.disable_core_dumps);
    assert!(config.hardening.deny_debug_attach);
    assert!(!config.store_responses);
    assert!(config.exploration_compiler);
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
    assert_eq!(
        config.max_tool_calls_per_turn,
        DEFAULT_MAX_TOOL_CALLS_PER_TURN
    );
    assert_eq!(
        config.max_tool_bytes_read_per_turn,
        DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN
    );
    assert_eq!(
        config.max_search_files_per_turn,
        DEFAULT_MAX_SEARCH_FILES_PER_TURN
    );
    assert!(!config.checkpoints_enabled);
    assert!(config.tools.lazy_schema_loading);
    assert!(config.tools.core.contains(&"grep".to_string()));
    assert!(config.tools.core.contains(&"plan_patch".to_string()));
    assert!(config.tools.core.contains(&"apply_patch".to_string()));
    // Control tools are always-core and intentionally absent from the
    // configurable `core` list; `squeezy_agent::request_tool_specs` forces
    // them into the request by name.
    assert!(!config.tools.core.contains(&"load_tool_schema".to_string()));
    assert!(!config.tools.core.contains(&"update_task_state".to_string()));
    assert!(!config.tools.core.contains(&"delegate".to_string()));
    assert!(!config.tools.core.contains(&"explore".to_string()));
    assert!(config.tools.discoverable.is_empty());
    assert_eq!(
        config.context_compaction,
        ContextCompactionConfig::default()
    );
    assert_eq!(config.subagents, SubagentConfig::default());
    assert_eq!(config.telemetry, TelemetryConfig::default());
    assert!(config.skills.user_dir.ends_with(DEFAULT_SQUEEZY_SKILLS_DIR));
    assert!(
        config
            .skills
            .compat_user_dir
            .ends_with(DEFAULT_AGENT_COMPAT_SKILLS_DIR)
    );
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key_env, "SQUEEZY_OPENAI_KEY");
            assert_eq!(openai.base_url, DEFAULT_OPENAI_BASE_URL);
        }
        _ => panic!("expected OpenAI provider"),
    }
}

#[test]
fn context_compaction_config_reads_settings_and_env() {
    let settings = SettingsFile::from_toml_str(
        r#"
[context]
compaction_enabled = false
compaction_estimated_tokens = 1234
compaction_min_items = 7
compaction_recent_items = 3
compaction_max_summary_bytes = 4096
"#,
        "test",
    )
    .expect("settings");
    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_CONTEXT_COMPACTION_ENABLED" => Some("true".to_string()),
        "SQUEEZY_CONTEXT_COMPACTION_ESTIMATED_TOKENS" => Some("2048".to_string()),
        _ => None,
    });

    assert!(config.context_compaction.enabled);
    assert_eq!(config.context_compaction.estimated_tokens, 2048);
    assert_eq!(config.context_compaction.min_items, 7);
    assert_eq!(config.context_compaction.recent_items, 3);
    assert_eq!(config.context_compaction.max_summary_bytes, 4096);
    assert!(config.inspect_redacted().contains("[context]"));
}

#[test]
fn subagent_config_defaults_stay_within_cost_broker_ceiling() {
    let defaults = SubagentConfig::default();
    assert!(
        defaults.max_tool_bytes_read_per_call <= 100_000_000,
        "default max_tool_bytes_read_per_call = {} exceeds 100MB ceiling",
        defaults.max_tool_bytes_read_per_call
    );
    assert!(
        defaults.max_search_files_per_call <= 50_000,
        "default max_search_files_per_call = {} exceeds 50K ceiling",
        defaults.max_search_files_per_call
    );
}

#[test]
fn subagent_config_reads_settings_and_env() {
    let settings = SettingsFile::from_toml_str(
        r#"
[subagents]
enabled = true
explore_enabled = false
explore_model = "cheap-from-settings"
max_tool_calls_per_call = 9
max_tool_bytes_read_per_call = 1000
max_search_files_per_call = 11
max_model_rounds = 2
max_summary_tokens = 333
"#,
        "test",
    )
    .expect("settings");
    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_EXPLORE_SUBAGENT_ENABLED" => Some("true".to_string()),
        "SQUEEZY_EXPLORE_MODEL" => Some("cheap-from-env".to_string()),
        "SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL" => Some("12".to_string()),
        _ => None,
    });

    assert!(config.subagents.enabled);
    assert!(config.subagents.explore_enabled);
    assert_eq!(
        config.subagents.explore_model.as_deref(),
        Some("cheap-from-env")
    );
    assert_eq!(config.subagents.max_tool_calls_per_call, 12);
    assert_eq!(config.subagents.max_tool_bytes_read_per_call, 1000);
    assert_eq!(config.subagents.max_search_files_per_call, 11);
    assert_eq!(config.subagents.max_model_rounds, 2);
    assert_eq!(config.subagents.max_summary_tokens, 333);
    let inspect = config.inspect_redacted();
    assert!(inspect.contains("[subagents]"));
    let round_tripped =
        SettingsFile::from_toml_str(&inspect, "round-trip").expect("inspect parses");
    let round_tripped_config = AppConfig::from_settings_and_env_vars(round_tripped, |_| None);
    assert_eq!(round_tripped_config.subagents, config.subagents);
}

#[test]
fn shell_sandbox_settings_parse_and_round_trip() {
    let root = std::env::temp_dir().join(format!(
        "squeezy_shell_roots_{}_{}",
        std::process::id(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    let read_root = root.join("read");
    let write_root = root.join("write");
    std::fs::create_dir_all(&read_root).expect("read root");
    std::fs::create_dir_all(&write_root).expect("write root");
    let read_root = std::fs::canonicalize(&read_root).expect("canonical read root");
    let write_root = std::fs::canonicalize(&write_root).expect("canonical write root");
    let settings = SettingsFile::from_toml_str(
        &format!(
            r#"
[permissions.shell_sandbox]
mode = "best_effort"
network = "allow_when_approved"
audit = false
kill_grace_ms = 500
env_allowlist = ["PATH", "LC_*"]
read_roots = [{}]
write_roots = [{}]
sensitive_path_patterns = [".ssh/**", ".env*"]
replace_sensitive_path_patterns = true
protected_metadata_names = [".git", ".custom"]
"#,
            toml_string(&read_root.display().to_string()),
            toml_string(&write_root.display().to_string()),
        ),
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(
        config.permissions.shell_sandbox.mode,
        ShellSandboxMode::BestEffort
    );
    assert_eq!(
        config.permissions.shell_sandbox.network,
        ShellSandboxNetworkPolicy::AllowWhenApproved
    );
    assert!(!config.permissions.shell_sandbox.audit);
    assert_eq!(config.permissions.shell_sandbox.kill_grace_ms, 500);
    assert_eq!(
        config.permissions.shell_sandbox.env_allowlist,
        ["PATH", "LC_*"]
    );
    assert_eq!(
        config.permissions.shell_sandbox.read_roots,
        vec![read_root.clone()]
    );
    assert_eq!(
        config.permissions.shell_sandbox.write_roots,
        vec![write_root.clone()]
    );
    assert_eq!(
        config.permissions.shell_sandbox.sensitive_path_patterns,
        [".ssh/**", ".env*"]
    );
    assert_eq!(
        config.permissions.shell_sandbox.protected_metadata_names,
        [".git", ".custom"]
    );

    let inspect = config.inspect_redacted();
    assert!(inspect.contains("[permissions.shell_sandbox]"));
    assert!(inspect.contains("mode = \"best_effort\""));
    assert!(inspect.contains("protected_metadata_names = [\".git\", \".custom\"]"));
    let round_tripped = SettingsFile::from_toml_str(&inspect, "round-trip")
        .expect("inspect output parses back as settings");
    let round_tripped_config = AppConfig::from_settings_and_env_vars(round_tripped, |_| None);
    assert_eq!(
        round_tripped_config.permissions.shell_sandbox, config.permissions.shell_sandbox,
        "inspect output must round-trip to the same effective sandbox config",
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shell_sandbox_defaults_to_best_effort() {
    let config = AppConfig::from_settings_and_env_vars(SettingsFile::default(), |_| None);

    assert_eq!(
        config.permissions.shell_sandbox.mode,
        ShellSandboxMode::BestEffort
    );
}

#[test]
fn sandbox_ai_reviewer_and_hardening_settings_parse_and_inspect() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions.ai_reviewer]
enabled = true
model = "reviewer-model"
allow_capabilities = ["read", "search", "edit"]
policy_file = "docs/approval.md"
timeout_secs = 7

[permissions.shell_sandbox]
mode = "external"
protected_metadata_names = [".git", ".meta"]

[hardening]
disable_core_dumps = false
deny_debug_attach = false
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(
        config.permissions.shell_sandbox.mode,
        ShellSandboxMode::External
    );
    assert_eq!(
        config.permissions.shell_sandbox.protected_metadata_names,
        [".git", ".meta"]
    );
    assert!(config.permissions.ai_reviewer.enabled);
    assert_eq!(
        config.permissions.ai_reviewer.model.as_deref(),
        Some("reviewer-model")
    );
    assert_eq!(config.permissions.ai_reviewer.timeout_secs, 7);
    assert_eq!(
        config.permissions.ai_reviewer.allow_capabilities,
        vec![
            PermissionCapability::Read,
            PermissionCapability::Search,
            PermissionCapability::Edit,
        ]
    );
    assert_eq!(
        config.permissions.ai_reviewer.policy_file.as_deref(),
        Some(Path::new("docs/approval.md"))
    );
    assert!(!config.hardening.disable_core_dumps);
    assert!(!config.hardening.deny_debug_attach);

    let inspect = config.inspect_redacted();
    assert!(inspect.contains("[permissions.ai_reviewer]"));
    assert!(inspect.contains("enabled = true"));
    assert!(inspect.contains("mode = \"external\""));
    assert!(inspect.contains("[hardening]"));
    SettingsFile::from_toml_str(&inspect, "round-trip").expect("inspect parses");
}

#[test]
fn config_reads_supported_env_overrides() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_MODEL" => Some("custom-model".to_string()),
        "OPENAI_BASE_URL" => Some("https://example.test/v1".to_string()),
        "SQUEEZY_EDIT_PERMISSION" => Some("allow".to_string()),
        "SQUEEZY_SHELL_PERMISSION" => Some("deny".to_string()),
        "SQUEEZY_STORE_RESPONSES" => Some("true".to_string()),
        "SQUEEZY_MAX_OUTPUT_TOKENS" => Some("64000".to_string()),
        "SQUEEZY_MAX_PARALLEL_TOOLS" => Some("3".to_string()),
        "SQUEEZY_WEB_PERMISSION" => Some("allow".to_string()),
        "SQUEEZY_EXA_MCP_URL" => Some("https://search.example/mcp".to_string()),
        "SQUEEZY_EXA_API_KEY_ENV" => Some("CUSTOM_EXA_KEY".to_string()),
        "SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES" => Some("1234".to_string()),
        "SQUEEZY_TOOL_PREVIEW_BYTES" => Some("456".to_string()),
        "SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND" => Some("7890".to_string()),
        "SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS" => Some("2".to_string()),
        "SQUEEZY_MAX_TOOL_CALLS_PER_TURN" => Some("12".to_string()),
        "SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN" => Some("3456".to_string()),
        "SQUEEZY_MAX_SEARCH_FILES_PER_TURN" => Some("78".to_string()),
        "SQUEEZY_TELEMETRY" => Some("off".to_string()),
        "SQUEEZY_TELEMETRY_ENDPOINT" => Some("https://telemetry.example/v1/batch".to_string()),
        "SQUEEZY_SESSION_MODE" => Some("plan".to_string()),
        "SQUEEZY_CHECKPOINTS_ENABLED" => Some("true".to_string()),
        "SQUEEZY_SKILLS_USER_DIR" => Some("/tmp/squeezy-skills".to_string()),
        "SQUEEZY_SKILLS_COMPAT_USER_DIR" => Some("/tmp/agent-skills".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "custom-model");
    assert_eq!(config.permissions.edit, PermissionMode::Allow);
    assert_eq!(config.permissions.shell, PermissionMode::Deny);
    assert_eq!(config.permissions.web, PermissionMode::Allow);
    assert_eq!(config.session_mode, SessionMode::Plan);
    assert!(config.checkpoints_enabled);
    assert!(config.store_responses);
    assert_eq!(config.max_output_tokens, Some(64_000));
    assert_eq!(config.max_parallel_tools, 3);
    assert_eq!(config.exa_mcp_url, "https://search.example/mcp");
    assert_eq!(config.exa_api_key_env, "CUSTOM_EXA_KEY");
    assert_eq!(config.tool_spill_threshold_bytes, 1234);
    assert_eq!(config.tool_preview_bytes, 456);
    assert_eq!(config.max_tool_result_bytes_per_round, 7890);
    assert_eq!(config.tool_output_retention_days, 2);
    assert_eq!(config.max_tool_calls_per_turn, 12);
    assert_eq!(config.max_tool_bytes_read_per_turn, 3456);
    assert_eq!(config.max_search_files_per_turn, 78);
    assert_eq!(
        config.telemetry,
        TelemetryConfig {
            enabled: false,
            endpoint: "https://telemetry.example/v1/batch".to_string()
        }
    );
    assert_eq!(config.skills.user_dir, PathBuf::from("/tmp/squeezy-skills"));
    assert_eq!(
        config.skills.compat_user_dir,
        PathBuf::from("/tmp/agent-skills")
    );
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.base_url, "https://example.test/v1");
        }
        _ => panic!("expected OpenAI provider"),
    }
}

#[test]
fn config_reads_skill_dirs_from_settings_file() {
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
user_dir = "/custom/squeezy-skills"
compat_user_dir = "/custom/agent-skills"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(
        config.skills.user_dir,
        PathBuf::from("/custom/squeezy-skills")
    );
    assert_eq!(
        config.skills.compat_user_dir,
        PathBuf::from("/custom/agent-skills")
    );
}

#[test]
fn config_expands_tilde_skill_dirs_from_settings_file() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
user_dir = "~/.squeezy/skills"
compat_user_dir = "~/.agents/skills"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(config.skills.user_dir, home.join(".squeezy/skills"));
    assert_eq!(config.skills.compat_user_dir, home.join(".agents/skills"));
}

#[test]
fn config_reads_skill_budgets_preamble_and_overrides() {
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
user_dir = "/custom/squeezy-skills"
compat_user_dir = "/custom/agent-skills"
active_budget_chars = 1234
active_body_cap_chars = 5678
preamble_enabled = false
preamble_budget_chars = 321

[[skills.config]]
name = "rust-nav"
enabled = false

[[skills.config]]
path = "/project/.squeezy/skills/rust-nav"
enabled = true
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(config.skills.active_budget_chars, 1234);
    assert_eq!(config.skills.active_body_cap_chars, 5678);
    assert!(!config.skills.preamble_enabled);
    assert_eq!(config.skills.preamble_budget_chars, 321);
    assert_eq!(config.skills.config.len(), 2);
    assert_eq!(config.skills.config[0].name.as_deref(), Some("rust-nav"));
    assert!(!config.skills.config[0].enabled);
    assert_eq!(
        config.skills.config[1].path.as_deref(),
        Some(Path::new("/project/.squeezy/skills/rust-nav"))
    );
    assert!(config.skills.config[1].enabled);

    let inspect = config.inspect_redacted();
    let round_tripped =
        SettingsFile::from_toml_str(&inspect, "round-trip").expect("inspect parses");
    let round_tripped_config = AppConfig::from_settings_and_env_vars(round_tripped, |_| None);
    assert_eq!(
        round_tripped_config.skills.active_budget_chars,
        config.skills.active_budget_chars
    );
    assert_eq!(round_tripped_config.skills.config, config.skills.config);
}

#[test]
fn skills_budget_mode_context_percent_scales_with_window() {
    // 200K-token model with 2% percent: 200_000 * 4 * 0.02 = 16_000.
    let mode = SkillsBudgetMode::ContextPercent { percent: 2.0 };
    assert_eq!(mode.effective_chars(Some(200_000), 4_000), 16_000);
    // 32K-token model with the same percent: 32_000 * 4 * 0.02 = 2_560.
    assert_eq!(mode.effective_chars(Some(32_000), 4_000), 2_560);
    // Without a window, the legacy chars cap is used so behavior stays
    // predictable when the user has not configured the active model size.
    assert_eq!(mode.effective_chars(None, 4_000), 4_000);
}

#[test]
fn skills_budget_mode_chars_ignores_window() {
    // Explicit Chars override pins the budget regardless of context size.
    let mode = SkillsBudgetMode::Chars { chars: 8_000 };
    assert_eq!(mode.effective_chars(Some(32_000), 4_000), 8_000);
    assert_eq!(mode.effective_chars(Some(200_000), 4_000), 8_000);
    assert_eq!(mode.effective_chars(None, 4_000), 8_000);
}

#[test]
fn skills_default_budget_mode_resolves_to_two_percent_of_context_window() {
    let config = SkillsConfig {
        model_context_window: Some(200_000),
        ..Default::default()
    };
    assert!(matches!(
        config.active_budget_mode,
        SkillsBudgetMode::ContextPercent { percent } if (percent - 2.0).abs() < f32::EPSILON
    ));
    assert_eq!(config.active_budget_effective_chars(), 16_000);
    assert_eq!(config.preamble_budget_effective_chars(), 16_000);
}

#[test]
fn skills_legacy_chars_setting_is_honored_when_mode_unset() {
    // A user who only set `active_budget_chars` in TOML should keep the
    // pre-mode behaviour: that field becomes the absolute cap, even when a
    // 200K-token window is configured.
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
active_budget_chars = 1234
preamble_budget_chars = 321

[context]
model_context_window = 200000
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(config.skills.active_budget_chars, 1234);
    assert_eq!(config.skills.active_budget_effective_chars(), 1234);
    assert_eq!(config.skills.preamble_budget_effective_chars(), 321);
}

#[test]
fn skills_explicit_mode_table_parses() {
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
active_budget_mode = { chars = 9000 }
preamble_budget_mode = { context_percent = 5.0 }

[context]
model_context_window = 50000
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(config.skills.active_budget_effective_chars(), 9_000);
    // 50_000 tokens * 4 chars/token * 0.05 = 10_000 chars.
    assert_eq!(config.skills.preamble_budget_effective_chars(), 10_000);
}

#[test]
fn skills_mode_table_rejects_both_keys() {
    let err = SettingsFile::from_toml_str(
        r#"
[skills]
active_budget_mode = { chars = 9000, context_percent = 2.0 }
"#,
        "test",
    )
    .expect_err("conflicting keys must error");
    assert!(err.to_string().contains("set exactly one"));
}

#[test]
fn config_can_select_anthropic_provider_defaults() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(config.model, DEFAULT_ANTHROPIC_MODEL);
    match config.provider {
        ProviderConfig::Anthropic(anthropic) => {
            assert_eq!(anthropic.api_key_env, "SQUEEZY_ANTHROPIC_KEY");
            assert_eq!(anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        }
        _ => panic!("expected Anthropic provider"),
    }
}

#[test]
fn config_disables_exploration_compiler_via_env_var() {
    for value in ["off", "false", "0", "no", "disabled"] {
        let config = AppConfig::from_env_vars(None, |name| match name {
            "SQUEEZY_EXPLORATION_COMPILER" => Some(value.to_string()),
            _ => None,
        });
        assert!(
            !config.exploration_compiler,
            "SQUEEZY_EXPLORATION_COMPILER={value:?} must disable the planner",
        );
    }
}

#[test]
fn config_keeps_exploration_compiler_default_on_for_unknown_env_values() {
    // The planner defaults to on, so non-disabling env-var values (typos, empty
    // strings, or aliases like `enabled`) must not silently flip the default.
    for value in ["", "enabled", "on", "yes", "true", "1", "garbage"] {
        let config = AppConfig::from_env_vars(None, |name| match name {
            "SQUEEZY_EXPLORATION_COMPILER" => Some(value.to_string()),
            _ => None,
        });
        assert!(
            config.exploration_compiler,
            "SQUEEZY_EXPLORATION_COMPILER={value:?} should not silently disable the planner",
        );
    }
}

#[test]
fn config_reads_anthropic_env_overrides() {
    let config = AppConfig::from_env_vars(None, |name| match name {
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
        _ => panic!("expected Anthropic provider"),
    }
}

#[test]
fn config_reads_settings_file_provider_defaults() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "ollama"
profile = "cheap"

[providers.ollama]
base_url = "http://127.0.0.1:11434/api"
default_model = "llama-local"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(config.model, "llama-local");
    assert_eq!(config.profile, ModelProfile::Cheap);
    match config.provider {
        ProviderConfig::Ollama(ollama) => {
            assert_eq!(ollama.base_url, "http://127.0.0.1:11434/api");
        }
        _ => panic!("expected Ollama provider"),
    }
}

#[test]
fn env_overrides_settings_file_provider_and_model() {
    let settings = SettingsFile::from_toml_str(
        r#"
provider = "ollama"
model = "llama-local"

[providers.google]
api_key_env = "CUSTOM_GEMINI_KEY"
base_url = "https://gemini.example/v1"
default_model = "gemini-local"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_PROVIDER" => Some("google".to_string()),
        "SQUEEZY_MODEL" => Some("gemini-env".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "gemini-env");
    match config.provider {
        ProviderConfig::Google(google) => {
            assert_eq!(google.api_key_env, "CUSTOM_GEMINI_KEY");
            assert_eq!(google.base_url, "https://gemini.example/v1");
        }
        _ => panic!("expected Google provider"),
    }
}

#[test]
fn provider_override_uses_selected_provider_settings_and_default_model() {
    let settings = SettingsFile::from_toml_str(
        r#"
provider = "openai"

[providers.openai]
default_model = "openai-settings-model"

[providers.anthropic]
api_key_env = "CUSTOM_ANTHROPIC_KEY"
base_url = "https://anthropic.example/v1"
default_model = "claude-settings-model"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "claude-settings-model");
    match config.provider {
        ProviderConfig::Anthropic(anthropic) => {
            assert_eq!(anthropic.api_key_env, "CUSTOM_ANTHROPIC_KEY");
            assert_eq!(anthropic.base_url, "https://anthropic.example/v1");
        }
        _ => panic!("expected Anthropic provider"),
    }
}

#[test]
fn config_can_select_azure_bedrock_and_ollama_defaults() {
    let azure = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("azure_openai".to_string()),
        "AZURE_OPENAI_BASE_URL" => Some("https://resource.openai.azure.com/openai/v1".to_string()),
        _ => None,
    });
    assert!(matches!(azure.provider, ProviderConfig::AzureOpenAi(_)));
    assert_eq!(azure.model, DEFAULT_AZURE_OPENAI_MODEL);

    let bedrock = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("bedrock".to_string()),
        _ => None,
    });
    assert!(matches!(bedrock.provider, ProviderConfig::Bedrock(_)));
    assert_eq!(bedrock.model, DEFAULT_BEDROCK_MODEL);

    let ollama = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("ollama".to_string()),
        _ => None,
    });
    assert!(matches!(ollama.provider, ProviderConfig::Ollama(_)));
    assert_eq!(ollama.model, DEFAULT_OLLAMA_MODEL);
}

#[test]
fn config_resolves_opus_alias_to_full_id() {
    let anthropic = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        "SQUEEZY_MODEL" => Some("opus".to_string()),
        _ => None,
    });
    assert!(matches!(anthropic.provider, ProviderConfig::Anthropic(_)));
    assert_eq!(anthropic.model, DEFAULT_ANTHROPIC_MODEL);

    let openai = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openai".to_string()),
        "SQUEEZY_MODEL" => Some("opus".to_string()),
        _ => None,
    });
    assert_eq!(openai.model, DEFAULT_OPENAI_MODEL);

    // Full IDs pass through untouched.
    let passthrough = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        "SQUEEZY_MODEL" => Some("claude-sonnet-4-6".to_string()),
        _ => None,
    });
    assert_eq!(passthrough.model, "claude-sonnet-4-6");
}

#[test]
fn permission_mode_parses_expected_values() {
    assert_eq!(PermissionMode::parse("allow"), Some(PermissionMode::Allow));
    assert_eq!(PermissionMode::parse("ASK"), Some(PermissionMode::Ask));
    assert_eq!(PermissionMode::parse("deny"), Some(PermissionMode::Deny));
    assert_eq!(PermissionMode::parse("maybe"), None);
}

#[test]
fn permission_policy_matches_last_rule_and_reports_source() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions]
shell = "ask"

[[permissions.rules]]
capability = "shell"
target = "cargo test:*"
action = "deny"
source = "user"

[[permissions.rules]]
capability = "shell"
target = "cargo test:*"
action = "allow"
source = "project"
reason = "project allows tests"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let verdict = config.permissions.evaluate(&shell_request("cargo test:*"));

    assert_eq!(verdict.action, PermissionAction::Allow);
    assert_eq!(verdict.reason, "project allows tests");
    assert_eq!(
        verdict.matched_rule.as_ref().map(|rule| rule.source),
        Some(PermissionRuleSource::Project)
    );
}

#[test]
fn wildcard_match_anchors_prefix_and_suffix_with_multiple_stars() {
    assert!(wildcard_match("cargo test --workspace", "cargo *"));
    assert!(wildcard_match("path:src/foo.rs", "path:*"));
    assert!(wildcard_match("path:src/foo.rs", "path:*.rs"));
    assert!(wildcard_match("path:src/lib/foo.rs", "path:*/foo.rs"));
    assert!(wildcard_match("path:src/foo.rs", "*"));
    assert!(wildcard_match("anything", "*"));

    assert!(!wildcard_match("path:src/foo.rs", "src/foo.rs"));
    assert!(!wildcard_match("rm -rf /", "git *"));
    assert!(!wildcard_match("path:src/foo.txt", "path:*.rs"));
    assert!(!wildcard_match("ab", "a*b*c"));
}

#[test]
fn permission_policy_evaluate_with_extra_lets_session_rules_override_config() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions]
shell = "ask"

[[permissions.rules]]
capability = "shell"
target = "cargo test:*"
action = "deny"
source = "user"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    let request = shell_request("cargo test:*");
    let baseline = config.permissions.evaluate(&request);
    assert_eq!(baseline.action, PermissionAction::Deny);

    let session_rule = PermissionRule::new(
        "shell",
        "cargo test:*",
        PermissionAction::Allow,
        PermissionRuleSource::Session,
        Some("session approved cargo test".to_string()),
    );
    let layered = config
        .permissions
        .evaluate_with_extra(&request, std::slice::from_ref(&session_rule));
    assert_eq!(layered.action, PermissionAction::Allow);
    assert_eq!(
        layered.matched_rule.as_ref().map(|rule| rule.source),
        Some(PermissionRuleSource::Session)
    );
}

#[test]
fn permission_policy_refuses_allow_on_destructive_at_load_time() {
    let result = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "destructive"
target = "rm:*"
action = "allow"
source = "user"
"#,
        "test",
    );
    let err = result.expect_err("destructive Allow rule should be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("destructive capability"),
        "unexpected message: {msg}"
    );
}

#[test]
fn permission_policy_refuses_allow_on_bare_star_target_at_load_time() {
    let result = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "shell"
target = "*"
action = "allow"
source = "user"
"#,
        "test",
    );
    let err = result.expect_err("bare-* Allow rule should be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("bare wildcard target"),
        "unexpected message: {msg}"
    );

    // Functionally identical "**" target must be refused too.
    let result_double = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "shell"
target = "**"
action = "allow"
source = "user"
"#,
        "test",
    );
    result_double.expect_err("** Allow rule should also be rejected");
}

#[test]
fn target_is_effectively_wildcard_helper_recognizes_match_everything() {
    use super::target_is_effectively_wildcard as helper;
    assert!(helper("*"));
    assert!(helper("**"));
    assert!(helper("* *"));
    assert!(helper(" * "));
    assert!(helper("***"));
    assert!(helper(""));
    assert!(!helper("cargo test:*"));
    assert!(!helper("shell:*"));
    assert!(!helper("path:src/foo.rs"));
}

#[test]
fn permission_policy_downgrades_allow_on_bare_star_runtime_safety_net() {
    let session_rule = PermissionRule::new(
        "shell",
        "**",
        PermissionAction::Allow,
        PermissionRuleSource::Session,
        Some("session opt-in".to_string()),
    );
    let policy = PermissionPolicy::default();
    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: "rm:*".to_string(),
        risk: PermissionRisk::High,
        summary: "rm -rf target".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    let verdict = policy.evaluate_with_extra(&request, std::slice::from_ref(&session_rule));
    assert_eq!(verdict.action, PermissionAction::Ask);
    assert!(
        verdict.reason.contains("bare wildcard target"),
        "verdict reason should explain downgrade: {}",
        verdict.reason,
    );
}

fn try_app_config(toml: &str) -> Result<AppConfig> {
    let settings = SettingsFile::from_toml_str(toml, "test").expect("settings parse");
    AppConfig::try_from_settings_and_env_vars(settings, None, |_| None)
}

#[test]
fn shell_sandbox_config_validates_kill_grace_bounds_and_glob_patterns() {
    let err = try_app_config(
        r#"
[permissions.shell_sandbox]
kill_grace_ms = 1
"#,
    )
    .expect_err("kill_grace_ms = 1 must be rejected");
    assert!(format!("{err}").contains("kill_grace_ms"));

    try_app_config(
        r#"
[permissions.shell_sandbox]
kill_grace_ms = 999999
"#,
    )
    .expect_err("kill_grace_ms above ceiling must be rejected");

    let err = try_app_config(
        r#"
[permissions.shell_sandbox]
env_allowlist = ["*_PROXY"]
"#,
    )
    .expect_err("env_allowlist *_PROXY must be rejected");
    assert!(format!("{err}").contains("env_allowlist"));

    let err = try_app_config(
        r#"
[permissions.shell_sandbox]
sensitive_path_patterns = ["**"]
"#,
    )
    .expect_err("sensitive_path_patterns ** must be rejected");
    assert!(format!("{err}").contains("sensitive_path_patterns"));
}

#[test]
fn shell_sandbox_config_validates_extra_roots() {
    let root = std::env::temp_dir().join(format!(
        "squeezy_shell_root_validation_{}_{}",
        std::process::id(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    let read_root = root.join("read");
    let file_root = root.join("file");
    let ssh_root = root.join(".ssh");
    std::fs::create_dir_all(&read_root).expect("read root");
    std::fs::create_dir_all(&ssh_root).expect("ssh root");
    std::fs::write(&file_root, "not a dir").expect("file root");

    let relative = try_app_config(
        r#"
[permissions.shell_sandbox]
read_roots = ["relative"]
"#,
    )
    .expect_err("relative roots must be rejected");
    assert!(format!("{relative}").contains("read_roots"));

    let missing = try_app_config(&format!(
        r#"
[permissions.shell_sandbox]
read_roots = [{}]
"#,
        toml_string(&root.join("missing").display().to_string())
    ))
    .expect_err("missing roots must be rejected");
    assert!(format!("{missing}").contains("not accessible"));

    let file = try_app_config(&format!(
        r#"
[permissions.shell_sandbox]
read_roots = [{}]
"#,
        toml_string(&file_root.display().to_string())
    ))
    .expect_err("file roots must be rejected");
    assert!(format!("{file}").contains("not a directory"));

    let duplicate = try_app_config(&format!(
        r#"
[permissions.shell_sandbox]
read_roots = [{root}]
write_roots = [{root}]
"#,
        root = toml_string(&read_root.display().to_string())
    ))
    .expect_err("duplicate read/write roots must be rejected");
    assert!(format!("{duplicate}").contains("both read_roots and write_roots"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shell_sandbox_invalid_mode_is_rejected() {
    let err = try_app_config(
        r#"
[permissions.shell_sandbox]
mode = "loose"
"#,
    )
    .expect_err("invalid mode must be rejected");
    assert!(format!("{err}").contains("mode"));

    try_app_config(
        r#"
[permissions.shell_sandbox]
network = "open"
"#,
    )
    .expect_err("invalid network must be rejected");
}

#[test]
fn shell_sandbox_sensitive_path_patterns_default_to_union_with_floor() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions.shell_sandbox]
sensitive_path_patterns = ["secrets/**"]
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let patterns = &config.permissions.shell_sandbox.sensitive_path_patterns;
    // The user pattern is present.
    assert!(patterns.iter().any(|p| p == "secrets/**"));
    // The default floor is still present.
    for floor in [".ssh/**", ".aws/**", ".env*", ".netrc"] {
        assert!(
            patterns.iter().any(|p| p == floor),
            "default floor pattern {floor} must remain after union",
        );
    }
}

#[test]
fn shell_sandbox_replace_sensitive_path_patterns_opts_out_of_union() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions.shell_sandbox]
sensitive_path_patterns = ["secrets/**"]
replace_sensitive_path_patterns = true
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let patterns = &config.permissions.shell_sandbox.sensitive_path_patterns;
    assert_eq!(patterns.as_slice(), ["secrets/**"]);
}

#[test]
fn permission_policy_downgrades_allow_on_destructive_runtime_safety_net() {
    // Build a policy by hand that wires an Allow rule onto Destructive, then
    // confirm evaluate_with_extra refuses to honor it. This exercises the
    // belt-and-suspenders safety net regardless of how the rule reached the
    // in-memory policy.
    let session_rule = PermissionRule::new(
        "destructive",
        "rm:*",
        PermissionAction::Allow,
        PermissionRuleSource::Session,
        Some("session opt-in".to_string()),
    );
    let policy = PermissionPolicy::default();
    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Destructive,
        target: "rm:*".to_string(),
        risk: PermissionRisk::Critical,
        summary: "rm -rf node_modules".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };

    let verdict = policy.evaluate_with_extra(&request, std::slice::from_ref(&session_rule));
    assert_eq!(verdict.action, PermissionAction::Ask);
    assert!(
        verdict.reason.contains("destructive"),
        "verdict reason should explain downgrade: {}",
        verdict.reason
    );
    assert!(verdict.matched_rule.is_some());
}

#[test]
fn inspect_redacted_round_trips_with_permission_rules() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions]
shell = "ask"
shell_classifier = true

[session]
mode = "plan"

[[permissions.rules]]
capability = "shell"
target = "cargo test:*"
action = "allow"
source = "user"
reason = "tests are safe"

[[permissions.rules]]
capability = "network"
target = "domain:docs.rs"
action = "allow"
source = "project"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let inspect = config.inspect_redacted();

    assert!(inspect.contains("shell_classifier = true"));
    assert!(inspect.contains("[session]"));
    assert!(inspect.contains("mode = \"plan\""));
    assert!(inspect.contains("[[permissions.rules]]"));
    assert!(inspect.contains("target = \"cargo test:*\""));
    assert!(inspect.contains("target = \"domain:docs.rs\""));

    let round_tripped = SettingsFile::from_toml_str(&inspect, "round-trip")
        .expect("inspect output parses back as settings");
    let round_tripped_config = AppConfig::from_settings_and_env_vars(round_tripped, |_| None);
    assert_eq!(round_tripped_config.session_mode, SessionMode::Plan);
    assert_eq!(round_tripped_config.permissions.rules.len(), 2);
    assert!(round_tripped_config.permissions.shell_classifier);
}

fn shell_request(target: &str) -> PermissionRequest {
    PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: target.to_string(),
        risk: PermissionRisk::Medium,
        summary: format!("shell target={target}"),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    }
}

#[test]
fn section_settings_cover_budgets_permissions_graph_cache_tui_and_mcp() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"
model = "gpt-custom"
reasoning_effort = "high"
max_output_tokens = 512
store_responses = true
selection_version = 1

[providers.openai]
stream_idle_timeout_ms = 1234

[agent]
exploration_compiler = false

[budgets]
max_parallel_tools = 3
tool_spill_threshold_bytes = 1000
tool_preview_bytes = 200
max_tool_result_bytes_per_round = 3000
tool_output_retention_days = 2
max_tool_calls_per_turn = 4
max_tool_bytes_read_per_turn = 5000
max_search_files_per_turn = 6

[subagents]
enabled = true
explore_enabled = true
explore_model = "gpt-cheap"
max_tool_calls_per_call = 7
max_tool_bytes_read_per_call = 8000
max_search_files_per_call = 9
max_model_rounds = 2
max_summary_tokens = 444

[session]
mode = "plan"

[permissions]
read = "allow"
edit = "deny"
shell = "ask"
ignored_search = "allow"
web = "deny"

[telemetry]
enabled = false
endpoint = "https://telemetry.example/batch"

[web]
exa_mcp_url = "https://search.example/mcp"
exa_api_key_env = "CUSTOM_EXA_KEY"

[graph]
languages = ["rust", "csharp"]
max_file_bytes = 42
include_hidden = true
require_indexing_signal = false
include = ["vendor/allowed/**"]
exclude = ["fixtures/generated/**"]
include_classes = ["lockfile"]
exclude_classes = ["generated"]

[cache]
root = ".squeezy/cache"
tool_outputs = ".squeezy/tool_outputs"

[tools]
checkpoints_enabled = true
lazy_schema_loading = true
core = ["webfetch"]
discoverable = ["read_file"]

[tui]
tick_rate_ms = 75
status_verbosity = "verbose"
response_verbosity = "concise"
tool_output_verbosity = "normal"
transcript_default = "expanded"
alternate_screen = "always"
show_reasoning_usage = false

[mcp.servers.docs]
enabled = true
transport = "http"
url = "https://docs.example/mcp"
timeout_ms = 5000
discovery_timeout_ms = 45000
tool_call_timeout_ms = 120000
env = { TOKEN = "secret" }

[mcp.servers.docs.permissions]
default = "ask"

[[mcp.servers.docs.permissions.rules]]
target = "lookup:*"
action = "allow"
source = "project"
reason = "docs lookups are safe"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(config.model, "gpt-custom");
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(config.max_output_tokens, Some(512));
    assert_eq!(config.stream_idle_timeout, Duration::from_millis(1234));
    assert!(config.store_responses);
    assert!(!config.exploration_compiler);
    assert_eq!(config.session_mode, SessionMode::Plan);
    assert_eq!(config.max_parallel_tools, 3);
    assert_eq!(config.tool_spill_threshold_bytes, 1000);
    assert_eq!(config.subagents.explore_model.as_deref(), Some("gpt-cheap"));
    assert_eq!(config.subagents.max_tool_calls_per_call, 7);
    assert_eq!(config.subagents.max_tool_bytes_read_per_call, 8000);
    assert_eq!(config.subagents.max_search_files_per_call, 9);
    assert_eq!(config.subagents.max_model_rounds, 2);
    assert_eq!(config.subagents.max_summary_tokens, 444);
    assert_eq!(config.permissions.edit, PermissionMode::Deny);
    assert!(!config.telemetry.enabled);
    assert_eq!(config.exa_api_key_env, "CUSTOM_EXA_KEY");
    assert_eq!(config.graph.languages, vec!["rust", "csharp"]);
    assert_eq!(config.graph.include, vec!["vendor/allowed/**"]);
    assert_eq!(config.graph.exclude, vec!["fixtures/generated/**"]);
    assert_eq!(config.graph.include_classes, vec!["lockfile"]);
    assert_eq!(config.graph.exclude_classes, vec!["generated"]);
    assert_eq!(
        config.cache.tool_outputs,
        Some(PathBuf::from(".squeezy/tool_outputs"))
    );
    assert!(config.checkpoints_enabled);
    assert!(config.tools.lazy_schema_loading);
    assert!(config.tools.core.contains(&"webfetch".to_string()));
    assert!(!config.tools.core.contains(&"read_file".to_string()));
    assert_eq!(config.tools.discoverable, vec!["read_file"]);
    assert_eq!(config.tui.tick_rate_ms, 75);
    assert_eq!(config.tui.status_verbosity, StatusVerbosity::Verbose);
    assert_eq!(config.tui.response_verbosity, ResponseVerbosity::Concise);
    assert_eq!(
        config.tui.tool_output_verbosity,
        ToolOutputVerbosity::Normal
    );
    assert_eq!(config.tui.transcript_default, TranscriptDefault::Expanded);
    assert_eq!(config.tui.alternate_screen, TuiAlternateScreen::Always);
    assert!(!config.tui.show_reasoning_usage);
    assert_eq!(config.mcp_servers["docs"].transport, McpTransport::Http);
    assert_eq!(config.mcp_servers["docs"].timeout_ms, Some(5_000));
    assert_eq!(
        config.mcp_servers["docs"].discovery_timeout_ms,
        Some(45_000)
    );
    assert_eq!(
        config.mcp_servers["docs"].tool_call_timeout_ms,
        Some(120_000)
    );
    assert_eq!(
        config.mcp_servers["docs"].permissions.default,
        Some(PermissionMode::Ask)
    );
    assert!(
        config
            .permissions
            .rules
            .iter()
            .any(|rule| rule.capability == "mcp"
                && rule.target == "docs/lookup:*"
                && rule.action == PermissionMode::Allow)
    );
}

#[test]
fn tools_settings_reject_explicit_core_discoverable_overlap() {
    let settings = SettingsFile::from_toml_str(
        r#"
[tools]
core = ["webfetch"]
discoverable = ["webfetch"]
"#,
        "test",
    )
    .expect("settings parse");

    let error =
        AppConfig::try_from_settings_and_env_vars(settings, None, |_| None).expect_err("overlap");
    assert!(
        error
            .to_string()
            .contains("[tools] core and discoverable overlap")
    );
}

#[test]
fn explicit_user_mcp_rules_outrank_per_server_defaults() {
    // A user-declared `[[permissions.rules]]` deny must remain the last
    // word over a per-server `default = "allow"`. We assert this by
    // resolving the rule list in order and verifying that the final
    // matching rule for `docs/risky` is the user's Deny.
    let settings = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "mcp"
target = "docs/risky"
action = "deny"
source = "user"
reason = "operator-pinned deny"

[mcp.servers.docs]
enabled = true
transport = "stdio"
command = "docs-mcp"

[mcp.servers.docs.permissions]
default = "allow"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let rules: Vec<_> = config
        .permissions
        .rules
        .iter()
        .filter(|rule| rule.capability == "mcp")
        .collect();
    // The MCP-derived `docs/*` allow rule must be inserted *before* the
    // user's explicit `docs/risky` deny so last-write-wins matching
    // ultimately returns Deny.
    let server_default_idx = rules
        .iter()
        .position(|rule| rule.target == "docs/*")
        .expect("server-default rule present");
    let user_deny_idx = rules
        .iter()
        .position(|rule| rule.target == "docs/risky" && rule.action == PermissionMode::Deny)
        .expect("user deny present");
    assert!(
        server_default_idx < user_deny_idx,
        "user `[[permissions.rules]]` must come after MCP-derived rules so explicit policy wins"
    );
}

#[test]
fn invalid_reasoning_and_tui_verbosity_values_are_rejected() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
reasoning_effort = "xhigh"
"#,
        "test",
    )
    .expect("xhigh reasoning effort");
    assert_eq!(
        settings.model_settings.unwrap().reasoning_effort,
        Some(ReasoningEffort::XHigh)
    );

    let reasoning = SettingsFile::from_toml_str(
        r#"
[model]
reasoning_effort = "extreme"
"#,
        "test",
    )
    .expect_err("invalid reasoning effort");
    assert!(reasoning.to_string().contains("reasoning_effort"));

    let response = SettingsFile::from_toml_str(
        r#"
[tui]
response_verbosity = "chatty"
"#,
        "test",
    )
    .expect_err("invalid response verbosity");
    assert!(response.to_string().contains("response_verbosity"));

    let tool = SettingsFile::from_toml_str(
        r#"
[tui]
tool_output_verbosity = "full"
"#,
        "test",
    )
    .expect_err("invalid tool output verbosity");
    assert!(tool.to_string().contains("tool_output_verbosity"));
}

#[test]
fn project_settings_override_user_settings_with_deep_provider_merge() {
    let mut user = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"

[providers.openai]
api_key_env = "USER_OPENAI_KEY"
base_url = "https://user.example/v1"
default_model = "user-model"
"#,
        "user",
    )
    .expect("user settings");
    let project = SettingsFile::from_toml_str(
        r#"
[model]
model = "project-model"

[providers.openai]
default_model = "project-default"
"#,
        "project",
    )
    .expect("project settings");

    user.merge(project);
    let config = AppConfig::from_settings_and_env_vars(user, |_| None);

    assert_eq!(config.model, "project-model");
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key_env, "USER_OPENAI_KEY");
            assert_eq!(openai.base_url, "https://user.example/v1");
        }
        _ => panic!("expected OpenAI provider"),
    }
}

#[test]
fn config_validation_reports_source_and_path() {
    let error = SettingsFile::from_toml_str(
        r#"
[permissions]
shell = "sometimes"
"#,
        "squeezy.toml",
    )
    .expect_err("invalid permission should fail");

    let message = error.to_string();
    assert!(message.contains("squeezy.toml"));
    assert!(message.contains("permissions.shell"));
}

#[test]
fn session_mode_validation_reports_source_and_path() {
    let error = SettingsFile::from_toml_str(
        r#"
[session]
mode = "maybe"
"#,
        "squeezy.toml",
    )
    .expect_err("invalid session mode should fail");

    let message = error.to_string();
    assert!(message.contains("squeezy.toml"));
    assert!(message.contains("session.mode"));
    assert!(message.contains("expected plan or build"));
}

#[test]
fn inspect_redacts_sensitive_config_values() {
    let settings = SettingsFile::from_toml_str(
        r#"
[web]
exa_api_key_env = "CUSTOM_EXA_KEY"

[redaction]
custom_patterns = ["internal-[a-z0-9]+"]

[mcp.servers.docs]
env = { TOKEN = "secret-value" }
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let inspect = config.inspect_redacted();

    assert!(inspect.contains("<redacted>"));
    assert!(!inspect.contains("CUSTOM_EXA_KEY"));
    assert!(!inspect.contains("internal-[a-z0-9]+"));
    assert!(!inspect.contains("secret-value"));
    SettingsFile::from_toml_str(&inspect, "inspect").expect("redacted inspect output parses");
}

#[test]
fn feedback_config_parses_endpoints_and_size_limits() {
    let settings = SettingsFile::from_toml_str(
        r#"
[feedback]
enabled = true
feedback_endpoint = "https://collector.example/v1/feedback"
report_endpoint = "https://collector.example/v1/report"
max_feedback_bytes = 4096
max_report_bytes = 1048576
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert!(config.feedback.enabled);
    assert_eq!(
        config.feedback.feedback_endpoint,
        "https://collector.example/v1/feedback"
    );
    assert_eq!(
        config.feedback.report_endpoint,
        "https://collector.example/v1/report"
    );
    assert_eq!(config.feedback.max_feedback_bytes, 4096);
    assert_eq!(config.feedback.max_report_bytes, 1_048_576);
    let inspect = config.inspect_redacted();
    assert!(inspect.contains("[feedback]"));
    assert!(inspect.contains("max_feedback_bytes = 4096"));
}

#[test]
fn redactor_masks_builtin_secret_patterns_with_stable_markers() {
    let redactor = RedactionConfig::default().redactor().expect("redactor");
    let input = concat!(
        "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz ",
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz ",
        "https://user:pass@example.com?token=secret-token-value ",
        "github=ghp_abcdefghijklmnopqrstuvwxyz"
    );

    let redacted = redactor.redact(input);

    assert!(redacted.redactions >= 4);
    assert!(!redacted.text.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!redacted.text.contains("abcdefghijklmnopqrstuvwxyz "));
    assert!(!redacted.text.contains("user:pass"));
    assert!(!redacted.text.contains("secret-token-value"));
    assert!(!redacted.text.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
    assert!(redacted.text.contains("OPENAI_API_KEY="));
    assert!(redacted.text.contains("<redacted:"));
    assert!(redacted.text.contains("bytes="));
}

#[test]
fn redactor_applies_custom_patterns_and_reuses_ordinals() {
    let settings = SettingsFile::from_toml_str(
        r#"
[redaction]
custom_patterns = ["internal-[0-9]+"]
"#,
        "test",
    )
    .expect("settings");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let redactor = config.redaction.redactor().expect("redactor");

    let redacted = redactor.redact("internal-123 and internal-123");

    assert_eq!(redacted.redactions, 2);
    assert!(!redacted.text.contains("internal-123"));
    assert_eq!(redacted.text.matches("<redacted:custom#1").count(), 2);
}

#[test]
fn redactor_returns_unchanged_text_without_allocating_on_no_match() {
    let redactor = RedactionConfig::default().redactor().expect("redactor");

    let unchanged = redactor.redact("nothing to see here");

    assert_eq!(unchanged.text, "nothing to see here");
    assert_eq!(unchanged.redactions, 0);
}

#[test]
fn redactor_value_capture_excludes_trailing_punctuation() {
    let redactor = RedactionConfig::default().redactor().expect("redactor");

    let redacted = redactor.redact("MY_API_KEY=foo) MY_API_KEY=bar]");

    assert!(redacted.text.contains(") "));
    assert!(redacted.text.ends_with(']'));
    assert!(!redacted.text.contains("foo"));
    assert!(!redacted.text.contains("bar"));
}

#[test]
fn stream_redactor_emits_safe_prefix_only_after_tail_grows() {
    use std::sync::Arc;
    let redactor = Arc::new(RedactionConfig::default().redactor().expect("redactor"));
    let mut stream = StreamRedactor::new(redactor);

    let short = stream.push("hello world");
    assert!(short.is_empty(), "small inputs stay buffered");

    let padded = "x".repeat(2_048);
    let chunk = stream.push(&padded);
    assert!(
        !chunk.text.is_empty(),
        "once buffer exceeds tail, prefix is released"
    );
    assert!(chunk.text.starts_with("hello world"));

    let tail = stream.finish();
    let combined = format!("{}{}", chunk.text, tail.text);
    assert_eq!(combined.len(), "hello world".len() + padded.len());
}

#[test]
fn stream_redactor_redacts_secret_split_across_chunks() {
    use std::sync::Arc;
    let redactor = Arc::new(RedactionConfig::default().redactor().expect("redactor"));
    let mut stream = StreamRedactor::new(redactor);

    let first = stream.push("here is sk-abcdefghij");
    assert!(first.is_empty(), "short partial secret must not leak");

    let second = stream.push("klmnopqrstuvwxyz token");
    assert!(second.is_empty(), "still inside tail buffer");

    let final_chunk = stream.finish();
    // Intentionally do not include `final_chunk.text` in the panic
    // message: the CodeQL "cleartext logging" rule flags assertion
    // messages that interpolate test-fixture secrets even though the
    // assertion only fires when redaction has already failed.
    assert!(
        !final_chunk.text.contains("sk-abcdefghijklmnopqrstuvwxyz"),
        "the full secret should be redacted on finish",
    );
    assert!(final_chunk.text.contains("<redacted:openai_key"));
    assert!(stream.total_redactions() >= 1);
}

#[test]
fn stream_redactor_holds_emission_until_pem_end_marker_arrives() {
    use std::sync::Arc;
    let redactor = Arc::new(RedactionConfig::default().redactor().expect("redactor"));
    let mut stream = StreamRedactor::new(redactor);

    // Build the PEM marker pieces at runtime so this test source file
    // does not itself trigger lexical secret scanners.
    let dashes = "-".repeat(5);
    let pem_begin = format!("{dashes}BEGIN PRIVATE KEY{dashes}");
    let pem_end = format!("{dashes}END PRIVATE KEY{dashes}");

    let begin = stream.push("preface ");
    assert!(begin.is_empty());
    let pem_open = stream.push(&format!("{pem_begin}\nAAAAA\n"));
    assert!(
        pem_open.is_empty(),
        "PEM open should suppress all emission until END appears"
    );
    let mid_padding = stream.push(&"B".repeat(3_000));
    assert!(
        mid_padding.is_empty(),
        "long PEM body must keep buffering, not leak"
    );
    let pem_close = stream.push(&format!("{pem_end}\ntrailer"));
    // After END appears the PEM lock releases and a non-empty redacted
    // chunk should be available either inline or on finish.
    let tail = stream.finish();
    let combined = format!("{}{}", pem_close.text, tail.text);
    // See note in stream_redactor_redacts_secret_split_across_chunks
    // about avoiding interpolated fixtures in panic messages.
    assert!(!combined.contains("AAAAA"), "PEM body must be redacted");
    assert!(combined.contains("<redacted:private_key"));
    assert!(combined.contains("preface"));
    assert!(combined.contains("trailer"));
}

#[test]
fn stream_redactor_does_not_split_a_redaction_marker_across_emits() {
    use std::sync::Arc;
    let redactor = Arc::new(RedactionConfig::default().redactor().expect("redactor"));
    let mut stream = StreamRedactor::new(redactor);

    // Lots of word-character padding so a single emit boundary exists, then
    // a properly-bounded secret so the openai_key pattern actually fires.
    let pad = "lorem ipsum ".repeat(100);
    let _ = stream.push(&pad);
    let with_secret = stream.push("see sk-abcdefghijklmnopqrstuvwxyz tail");
    let trailing = stream.finish();

    let combined = format!("{}{}", with_secret.text, trailing.text);
    // Keep the panic message free of `combined` so the CodeQL
    // cleartext-logging analysis stays clean.
    assert!(
        !combined.contains("sk-abcdefghijklmnopqrstuvwxyz"),
        "raw secret leaked through stream emit",
    );
    // Whenever a marker opens within the emitted portion it must also close
    // there; the unemitted tail may not start mid-marker.
    let open_count = with_secret.text.matches("<redacted:").count();
    let close_count = with_secret.text.matches('>').count();
    assert!(open_count <= close_count);
}

#[test]
fn invalid_custom_redaction_pattern_fails_config_loading() {
    let settings = SettingsFile::from_toml_str(
        r#"
[redaction]
custom_patterns = ["["]
"#,
        "test",
    )
    .expect("settings");

    let error = AppConfig::try_from_settings_and_env_vars(settings, None, |_| None)
        .expect_err("invalid pattern must fail");

    assert!(error.to_string().contains("redaction.custom_patterns.0"));
}

#[test]
fn generated_templates_parse() {
    SettingsFile::from_toml_str(user_settings_template(), "user template")
        .expect("user template parses");
    SettingsFile::from_toml_str(project_settings_template(), "project template")
        .expect("project template parses");
}

#[test]
fn cli_provider_does_not_get_tagged_as_env() {
    let settings = SettingsFile::default();
    let config = AppConfig::try_from_settings_and_env_vars(settings, Some("openai"), |_| None)
        .expect("config builds");

    assert_eq!(config.config_sources, vec!["defaults", "cli"]);
    assert!(matches!(config.provider, ProviderConfig::OpenAi(_)));
}

#[test]
fn env_provider_tagged_as_env_not_cli() {
    let settings = SettingsFile::default();
    let config = AppConfig::try_from_settings_and_env_vars(settings, None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openai".to_string()),
        _ => None,
    })
    .expect("config builds");

    assert_eq!(config.config_sources, vec!["defaults", "env"]);
}

#[test]
fn cli_and_env_both_tag_when_both_set() {
    let settings = SettingsFile::default();
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("anthropic"), |name| match name {
            "SQUEEZY_MODEL" => Some("claude-env".to_string()),
            _ => None,
        })
        .expect("config builds");

    assert_eq!(config.config_sources, vec!["defaults", "env", "cli"]);
    assert!(matches!(config.provider, ProviderConfig::Anthropic(_)));
    assert_eq!(config.model, "claude-env");
}

#[test]
fn openrouter_provider_resolves_to_compatible_variant_with_preset_defaults() {
    let settings = SettingsFile::default();
    let config =
        AppConfig::try_from_settings_and_env_vars(
            settings,
            Some("openrouter"),
            |name| match name {
                "OPENROUTER_BASE_URL" => None,
                _ => None,
            },
        )
        .expect("config builds");

    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("openrouter must map to OpenAiCompatible variant");
    };
    assert_eq!(compatible.preset, OpenAiCompatiblePreset::OpenRouter);
    assert_eq!(compatible.api_key_env, "OPENROUTER_API_KEY");
    assert_eq!(compatible.base_url, DEFAULT_OPENROUTER_BASE_URL);
    assert_eq!(config.model, DEFAULT_OPENROUTER_MODEL);
    // Aggregator presets must not enable `store_responses` even when the user
    // requests it; the OpenAI-Responses-only flag would be silently dropped
    // by the chat-completions endpoint and confuse the cost meter.
    assert!(!config.store_responses);
}

#[test]
fn vertex_preset_templates_base_url_from_project_and_location() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vertex".to_string(),
        ProviderSettings {
            vertex_project: Some("my-project".to_string()),
            vertex_location: Some("europe-west4".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("vertex"), |name| match name {
            "VERTEX_ACCESS_TOKEN" => Some("ya29.fake".to_string()),
            _ => None,
        })
        .expect("vertex config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vertex must map to OpenAiCompatible");
    };
    assert_eq!(compatible.preset, OpenAiCompatiblePreset::Vertex);
    assert_eq!(
        compatible.base_url,
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/my-project/locations/europe-west4/endpoints/openapi"
    );
    assert_eq!(config.model, DEFAULT_VERTEX_MODEL);
}

#[test]
fn vertex_preset_rejects_missing_project() {
    let settings = SettingsFile::default();
    let error = AppConfig::try_from_settings_and_env_vars(settings, Some("vertex"), |_| None)
        .expect_err("vertex requires project");
    assert!(
        format!("{error}").contains("vertex_project"),
        "error must explain the missing field, got: {error}"
    );
}

#[test]
fn cloudflare_workers_ai_preset_templates_base_url_from_account_id() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "cloudflare_workers_ai".to_string(),
        ProviderSettings {
            cloudflare_account_id: Some("acct-abc".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("cloudflare_workers_ai"),
        |name| match name {
            "CLOUDFLARE_API_KEY" => Some("cf-key".to_string()),
            _ => None,
        },
    )
    .expect("workers AI config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_workers_ai must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible.preset,
        OpenAiCompatiblePreset::CloudflareWorkersAi
    );
    assert_eq!(
        compatible.base_url,
        "https://api.cloudflare.com/client/v4/accounts/acct-abc/ai/v1"
    );
    assert_eq!(compatible.api_key_env, "CLOUDFLARE_API_KEY");
    assert_eq!(config.model, DEFAULT_CLOUDFLARE_WORKERS_AI_MODEL);
}

#[test]
fn cloudflare_workers_ai_preset_rejects_missing_account_id() {
    let settings = SettingsFile::default();
    let error =
        AppConfig::try_from_settings_and_env_vars(settings, Some("cloudflare_workers_ai"), |_| {
            None
        })
        .expect_err("workers AI requires account_id");
    assert!(
        format!("{error}").contains("cloudflare_account_id"),
        "error must explain the missing field, got: {error}"
    );
}

#[test]
fn cloudflare_ai_gateway_preset_templates_base_url_and_injects_dual_auth_header() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "cloudflare_ai_gateway".to_string(),
        ProviderSettings {
            cloudflare_account_id: Some("acct-abc".to_string()),
            cloudflare_gateway_id: Some("my-gateway".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("cloudflare_ai_gateway"),
        |name| match name {
            "CLOUDFLARE_API_KEY" => Some("cf-key".to_string()),
            "CF_AIG_TOKEN" => Some("gateway-secret".to_string()),
            _ => None,
        },
    )
    .expect("AI Gateway config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_ai_gateway must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible.preset,
        OpenAiCompatiblePreset::CloudflareAiGateway
    );
    assert_eq!(
        compatible.base_url,
        "https://gateway.ai.cloudflare.com/v1/acct-abc/my-gateway/compat"
    );
    // Dual auth: standard bearer goes through `api_key_env`; gateway token is
    // injected as `cf-aig-authorization` so the compat layer authenticates
    // both the upstream provider and the gateway itself.
    assert_eq!(compatible.api_key_env, "CLOUDFLARE_API_KEY");
    assert_eq!(
        compatible
            .extra_headers
            .get("cf-aig-authorization")
            .map(String::as_str),
        Some("Bearer gateway-secret"),
    );
}

#[test]
fn cloudflare_ai_gateway_defaults_gateway_id_when_omitted() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "cloudflare_ai_gateway".to_string(),
        ProviderSettings {
            cloudflare_account_id: Some("acct-abc".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("cloudflare_ai_gateway"),
        |name| match name {
            "CLOUDFLARE_API_KEY" => Some("cf-key".to_string()),
            _ => None,
        },
    )
    .expect("AI Gateway falls back to the `default` gateway id");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_ai_gateway must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible.base_url,
        "https://gateway.ai.cloudflare.com/v1/acct-abc/default/compat"
    );
    // No CF_AIG_TOKEN supplied → no `cf-aig-authorization` header injected;
    // the gateway runs in "open" / upstream-auth-only mode.
    assert!(
        !compatible
            .extra_headers
            .contains_key("cf-aig-authorization")
    );
}

#[test]
fn cloudflare_ai_gateway_user_supplied_header_wins_over_env_token() {
    let mut providers = std::collections::BTreeMap::new();
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "cf-aig-authorization".to_string(),
        "Bearer user-supplied".to_string(),
    );
    providers.insert(
        "cloudflare_ai_gateway".to_string(),
        ProviderSettings {
            cloudflare_account_id: Some("acct-abc".to_string()),
            headers: Some(headers),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("cloudflare_ai_gateway"),
        |name| match name {
            "CLOUDFLARE_API_KEY" => Some("cf-key".to_string()),
            "CF_AIG_TOKEN" => Some("env-token-should-lose".to_string()),
            _ => None,
        },
    )
    .expect("config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_ai_gateway must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible
            .extra_headers
            .get("cf-aig-authorization")
            .map(String::as_str),
        Some("Bearer user-supplied"),
    );
}

#[test]
fn aliases_map_to_compatible_presets() {
    for (alias, expected) in [
        ("vercel_ai", OpenAiCompatiblePreset::Vercel),
        ("grok", OpenAiCompatiblePreset::XAi),
        ("deep_seek", OpenAiCompatiblePreset::DeepSeek),
        ("port_key", OpenAiCompatiblePreset::PortKey),
        ("vertex_ai", OpenAiCompatiblePreset::Vertex),
        ("google_vertex", OpenAiCompatiblePreset::Vertex),
        ("workers_ai", OpenAiCompatiblePreset::CloudflareWorkersAi),
        ("cf_workers_ai", OpenAiCompatiblePreset::CloudflareWorkersAi),
        ("ai_gateway", OpenAiCompatiblePreset::CloudflareAiGateway),
        (
            "cloudflare_gateway",
            OpenAiCompatiblePreset::CloudflareAiGateway,
        ),
        ("custom", OpenAiCompatiblePreset::Custom),
    ] {
        // Some presets need extra fields filled in before the config can
        // build: `custom` requires an explicit base_url, `vertex` requires
        // a project so we can template the regional URL.
        let mut providers = std::collections::BTreeMap::new();
        if expected == OpenAiCompatiblePreset::Custom {
            providers.insert(
                "openai_compatible".to_string(),
                ProviderSettings {
                    api_key_env: Some("CUSTOM_KEY".to_string()),
                    base_url: Some("https://custom.example/v1".to_string()),
                    ..Default::default()
                },
            );
        }
        if expected == OpenAiCompatiblePreset::Vertex {
            providers.insert(
                "vertex".to_string(),
                ProviderSettings {
                    vertex_project: Some("alias-project".to_string()),
                    vertex_location: Some("us-central1".to_string()),
                    ..Default::default()
                },
            );
        }
        if expected == OpenAiCompatiblePreset::CloudflareWorkersAi {
            providers.insert(
                "cloudflare_workers_ai".to_string(),
                ProviderSettings {
                    cloudflare_account_id: Some("alias-acct".to_string()),
                    ..Default::default()
                },
            );
        }
        if expected == OpenAiCompatiblePreset::CloudflareAiGateway {
            providers.insert(
                "cloudflare_ai_gateway".to_string(),
                ProviderSettings {
                    cloudflare_account_id: Some("alias-acct".to_string()),
                    cloudflare_gateway_id: Some("alias-gw".to_string()),
                    ..Default::default()
                },
            );
        }
        let settings = SettingsFile {
            providers: Some(providers),
            ..Default::default()
        };
        let config = AppConfig::try_from_settings_and_env_vars(settings, Some(alias), |_| None)
            .unwrap_or_else(|err| panic!("alias {alias} should map: {err}"));
        let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
            panic!("alias {alias} must map to OpenAiCompatible");
        };
        assert_eq!(compatible.preset, expected, "alias {alias}");
    }
}

#[test]
fn config_source_labels_strip_paths() {
    let mut config = AppConfig::from_env_vars(None, |_| None);
    config.config_sources = vec![
        "defaults".to_string(),
        "user:/home/me/.squeezy/settings.toml".to_string(),
        "project:/repo/squeezy.toml".to_string(),
        "repo:/home/me/.squeezy/projects/demo-0123456789abcdef/settings.toml".to_string(),
        "env".to_string(),
        "cli".to_string(),
    ];

    assert_eq!(
        config.config_source_labels(),
        vec!["defaults", "user", "project", "repo", "env", "cli"],
    );
}

#[test]
fn inspect_output_is_valid_toml_and_uses_lowercase_enum_strings() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "anthropic"
profile = "strong"
model = "claude-test"

[permissions]
read = "deny"

[mcp.servers.docs]
transport = "http"
url = "https://docs.example/mcp"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    let inspect = config.inspect_redacted();

    assert!(inspect.contains("profile = \"strong\""));
    assert!(inspect.contains("provider = \"anthropic\""));
    assert!(inspect.contains("# max_output_tokens = unset"));
    assert!(inspect.contains("read = \"deny\""));
    assert!(inspect.contains("status_verbosity = \"compact\""));
    assert!(inspect.contains("response_verbosity = \"normal\""));
    assert!(inspect.contains("tool_output_verbosity = \"compact\""));
    assert!(inspect.contains("transcript_default = \"compact\""));
    assert!(inspect.contains("alternate_screen = \"auto\""));
    assert!(inspect.contains("show_reasoning_usage = true"));
    assert!(inspect.contains("transport = \"http\""));
    assert!(!inspect.contains("Balanced"));
    assert!(!inspect.contains("None"));

    SettingsFile::from_toml_str(&inspect, "inspect roundtrip")
        .expect("inspect output parses as TOML");
}

#[test]
fn inspect_omits_optional_cache_keys_when_unset() {
    let config = AppConfig::from_env_vars(None, |_| None);
    let inspect = config.inspect_redacted();

    assert!(inspect.contains("[cache]"));
    assert!(!inspect.contains("root ="));
    assert!(!inspect.contains("tool_outputs ="));
}

#[test]
fn load_settings_from_paths_merges_user_then_project() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy_config_paths_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let user_path = dir.join("user.toml");
    let project_path = dir.join("squeezy.toml");
    std::fs::write(
        &user_path,
        r#"
[model]
provider = "openai"
model = "user-model"

[providers.openai]
api_key_env = "USER_OPENAI_KEY"
base_url = "https://user.example/v1"
"#,
    )
    .expect("write user file");
    std::fs::write(
        &project_path,
        r#"
[model]
model = "project-model"

[providers.openai]
default_model = "project-default"
"#,
    )
    .expect("write project file");

    let (settings, sources) = load_settings_from_paths(
        Some(user_path.as_path()),
        Some(project_path.as_path()),
        None,
    )
    .expect("merge sources");

    assert_eq!(sources[0], "defaults");
    assert!(sources[1].starts_with("user:"));
    assert!(sources[1].contains("user.toml"));
    assert!(sources[2].starts_with("project:"));
    assert!(sources[2].contains("squeezy.toml"));

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(config.model, "project-model");
    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key_env, "USER_OPENAI_KEY");
            assert_eq!(openai.base_url, "https://user.example/v1");
        }
        _ => panic!("expected OpenAI provider"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_settings_from_paths_merges_repo_after_project() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy_config_repo_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let project_path = dir.join("squeezy.toml");
    let repo_path = dir.join("repo-settings.toml");
    std::fs::write(
        &project_path,
        r#"
[model]
model = "project-model"
"#,
    )
    .expect("write project file");
    std::fs::write(
        &repo_path,
        r#"
[model]
model = "repo-model"
"#,
    )
    .expect("write repo file");

    let (settings, sources) = load_settings_from_paths(
        None,
        Some(project_path.as_path()),
        Some(repo_path.as_path()),
    )
    .expect("merge sources");

    assert_eq!(sources[0], "defaults");
    assert!(sources[1].starts_with("project:"));
    assert!(sources[2].starts_with("repo:"));
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(config.model, "repo-model");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_settings_from_paths_skips_missing_files() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy_config_missing_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let user_path = dir.join("does_not_exist.toml");
    let project_path = dir.join("also_missing.toml");

    let (settings, sources) = load_settings_from_paths(
        Some(user_path.as_path()),
        Some(project_path.as_path()),
        Some(dir.join("repo_missing.toml").as_path()),
    )
    .expect("merge sources");

    assert_eq!(sources, vec!["defaults".to_string()]);
    assert!(settings.providers.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_log_settings_parse_defaults_and_overrides() {
    let settings = SettingsFile::from_toml_str(
        r#"
[session]
mode = "plan"
log_dir = ".squeezy/history"
log_retention_days = 45
max_event_bytes = 1234
max_session_bytes = 5678
"#,
        "test",
    )
    .expect("parse settings");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(config.session_mode, SessionMode::Plan);
    assert_eq!(config.session_logs.log_dir, Some(".squeezy/history".into()));
    assert_eq!(config.session_logs.log_retention_days, 45);
    assert_eq!(config.session_logs.max_event_bytes, 1234);
    assert_eq!(config.session_logs.max_session_bytes, 5678);
}

#[test]
fn init_user_template_contains_no_uncommented_assignments() {
    for line in user_settings_template().lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            continue;
        }
        panic!("user template line should be commented or sectional, got: {trimmed:?}");
    }
}

#[test]
fn tier_source_contains_path_walks_nested_tables() {
    let doc: toml_edit::DocumentMut =
        "[model]\nprovider = \"openai\"\n[mcp.servers.docs]\ncommand = \"x\"\n"
            .parse()
            .unwrap();
    let tier = TierSource {
        path: PathBuf::from("/tmp/nope.toml"),
        doc,
    };
    assert!(tier.contains_path(&["model", "provider"]));
    assert!(!tier.contains_path(&["model", "model"]));
    assert!(tier.contains_path(&["mcp", "servers", "docs", "command"]));
    assert!(!tier.contains_path(&["mcp", "servers", "other", "command"]));
    assert!(!tier.contains_path(&[]));
}

#[test]
fn resolve_field_source_uses_repo_then_project_then_user() {
    let user_doc: toml_edit::DocumentMut = "[model]\nprovider = \"openai\"\nmodel = \"gpt-5\"\n"
        .parse()
        .unwrap();
    let project_doc: toml_edit::DocumentMut = "[model]\nmodel = \"gpt-4\"\n".parse().unwrap();
    let repo_doc: toml_edit::DocumentMut = "".parse().unwrap();
    let sources = SeparatedSources {
        user: Some(TierSource {
            path: PathBuf::from("/u.toml"),
            doc: user_doc,
        }),
        project: Some(TierSource {
            path: PathBuf::from("/p.toml"),
            doc: project_doc,
        }),
        repo: Some(TierSource {
            path: PathBuf::from("/r.toml"),
            doc: repo_doc,
        }),
        user_path_default: PathBuf::from("/u.toml"),
        project_path_default: PathBuf::from("/p.toml"),
        repo_path_default: PathBuf::from("/r.toml"),
    };
    let models = &config_schema::CONFIG_SECTIONS[0];
    let provider_field = &models.fields[0]; // provider
    let model_field = &models.fields[1]; // model
    let profile_field = &models.fields[2]; // profile

    // Env-override fields might be set in the test environment; clear them so
    // the test asserts the tier precedence, not env precedence.
    // SAFETY: tests run single-threaded by default for this module.
    unsafe {
        std::env::remove_var("SQUEEZY_PROVIDER");
        std::env::remove_var("SQUEEZY_MODEL");
        std::env::remove_var("SQUEEZY_PROFILE");
    }

    assert_eq!(
        resolve_field_source(&sources, provider_field),
        config_schema::FieldSource::User
    );
    assert_eq!(
        resolve_field_source(&sources, model_field),
        config_schema::FieldSource::Project
    );
    assert_eq!(
        resolve_field_source(&sources, profile_field),
        config_schema::FieldSource::Default
    );
}

#[test]
fn resolve_field_source_returns_env_when_env_var_set() {
    let sources = SeparatedSources {
        user: None,
        project: None,
        repo: None,
        user_path_default: PathBuf::from("/u.toml"),
        project_path_default: PathBuf::from("/p.toml"),
        repo_path_default: PathBuf::from("/r.toml"),
    };
    let provider_field = &config_schema::CONFIG_SECTIONS[0].fields[0];
    assert_eq!(
        provider_field.env_override,
        Some("SQUEEZY_PROVIDER"),
        "provider field should declare env_override; precondition for this test"
    );
    // SAFETY: tests run single-threaded by default for this module.
    unsafe { std::env::set_var("SQUEEZY_PROVIDER", "anthropic") };
    let resolved = resolve_field_source(&sources, provider_field);
    unsafe { std::env::remove_var("SQUEEZY_PROVIDER") };
    assert_eq!(resolved, config_schema::FieldSource::Env);
}

#[test]
fn tui_theme_parses_lowercase_and_aliases_auto_to_system() {
    assert_eq!(TuiTheme::parse("system"), Some(TuiTheme::System));
    assert_eq!(TuiTheme::parse("dark"), Some(TuiTheme::Dark));
    assert_eq!(TuiTheme::parse("light"), Some(TuiTheme::Light));
    // `auto` is the historical / config-screen equivalent of "system".
    assert_eq!(TuiTheme::parse("auto"), Some(TuiTheme::System));
    // Named themes with distinct accent identities; aliases keep the slash
    // command forgiving when users guess hyphen / underscore variants.
    assert_eq!(TuiTheme::parse("catppuccin"), Some(TuiTheme::Catppuccin));
    assert_eq!(TuiTheme::parse("mauve"), Some(TuiTheme::Catppuccin));
    assert_eq!(
        TuiTheme::parse("high-contrast"),
        Some(TuiTheme::HighContrast),
    );
    assert_eq!(
        TuiTheme::parse("high_contrast"),
        Some(TuiTheme::HighContrast),
    );
    assert_eq!(TuiTheme::parse("hc"), Some(TuiTheme::HighContrast));
    // Whitespace and uppercase are tolerated so users typing `/theme Dark`
    // hit the same branch as the canonical `/theme dark` form.
    assert_eq!(TuiTheme::parse("  Dark  "), Some(TuiTheme::Dark));
    assert_eq!(TuiTheme::parse("LIGHT"), Some(TuiTheme::Light));
    assert_eq!(TuiTheme::parse("solarized"), None);
    assert_eq!(TuiTheme::parse(""), None);
}

#[test]
fn tui_theme_round_trips_through_settings_toml() {
    let parsed = SettingsFile::from_toml_str(
        r#"
[tui]
theme = "dark"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(parsed, |_| None);
    assert_eq!(config.tui.theme, TuiTheme::Dark);

    // Emit and re-parse to confirm the writer persists the field.
    let emitted = config.inspect_redacted();
    assert!(
        emitted.contains("theme = \"dark\""),
        "inspect should emit the theme leaf, got: {emitted}"
    );
    let reparsed = SettingsFile::from_toml_str(&emitted, "round trip").expect("inspect re-parse");
    let reloaded = AppConfig::from_settings_and_env_vars(reparsed, |_| None);
    assert_eq!(reloaded.tui.theme, TuiTheme::Dark);
}

#[test]
fn tui_theme_as_str_round_trips_through_parse_for_every_variant() {
    for theme in [
        TuiTheme::System,
        TuiTheme::Dark,
        TuiTheme::Light,
        TuiTheme::Catppuccin,
        TuiTheme::HighContrast,
    ] {
        let s = theme.as_str();
        assert_eq!(
            TuiTheme::parse(s),
            Some(theme),
            "as_str→parse must round-trip for {theme:?} (got {s:?})",
        );
    }
}

#[test]
fn tui_theme_defaults_to_system_when_unset() {
    let parsed =
        SettingsFile::from_toml_str("[tui]\ntick_rate_ms = 50\n", "test").expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(parsed, |_| None);
    assert_eq!(config.tui.theme, TuiTheme::System);
}

#[test]
fn tui_theme_rejects_unknown_string() {
    let result = SettingsFile::from_toml_str(
        r#"
[tui]
theme = "solarized"
"#,
        "test",
    );
    let err = result.expect_err("invalid theme should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid TUI theme") || msg.contains("solarized"),
        "expected invalid-theme diagnostic, got: {msg}"
    );
}

#[test]
fn unknown_fields_are_warned_and_removed_from_settings_file() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-unknown-fields-{}-{}",
        std::process::id(),
        CONFIG_TEST_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("settings.toml");
    // Use a deliberately invented key so the test stays meaningful as the
    // real schema grows. `tick_rate_ms` is the known control we expect to
    // survive untouched. (The earlier draft seeded `status_line_use_colors`
    // here, which became a real known key after #97 and made the assertion
    // unsatisfiable.)
    std::fs::write(
        &path,
        "[tui]\nlegacy_widget_padding = true\ntick_rate_ms = 100\n",
    )
    .expect("write seed settings");

    let (_settings, _sources) =
        SettingsFile::load_optional_source(&path, "test").expect("load_optional_source");

    let cleaned = std::fs::read_to_string(&path).expect("read cleaned settings");
    assert!(
        !cleaned.contains("legacy_widget_padding"),
        "unknown key should be stripped, got: {cleaned}"
    );
    assert!(
        cleaned.contains("tick_rate_ms = 100"),
        "known key should be preserved, got: {cleaned}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inline_api_key_parses_from_toml_and_flows_into_openai_config() {
    let settings = SettingsFile::from_toml_str(
        r#"
provider = "openai"

[providers.openai]
api_key = "sk-test-inline-12345"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    match config.provider {
        ProviderConfig::OpenAi(openai) => {
            assert_eq!(openai.api_key.as_deref(), Some("sk-test-inline-12345"));
        }
        _ => panic!("expected OpenAi provider"),
    }
}

#[test]
fn inline_api_key_is_redacted_on_serde_serialize() {
    let settings = ProviderSettings {
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        api_key: Some("sk-secret-do-not-leak".to_string()),
        ..ProviderSettings::default()
    };
    let emitted = toml::to_string(&settings).expect("serialize");
    assert!(
        !emitted.contains("sk-secret-do-not-leak"),
        "serialize must not leak plaintext; got: {emitted}"
    );
    assert!(
        emitted.contains("<redacted>"),
        "serialize must emit the redaction marker; got: {emitted}"
    );
}

#[test]
fn local_inline_api_key_overrides_user_inline_api_key() {
    let mut user = ProviderSettings {
        api_key: Some("from-user".to_string()),
        ..ProviderSettings::default()
    };
    let local = ProviderSettings {
        api_key: Some("from-local".to_string()),
        ..ProviderSettings::default()
    };
    user.merge(local);
    assert_eq!(user.api_key.as_deref(), Some("from-local"));
}

#[test]
fn provider_settings_accepts_both_api_key_and_api_key_env() {
    SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "sk-test"
api_key_env = "OPENAI_API_KEY"
"#,
        "test",
    )
    .expect("api_key + api_key_env both parse cleanly");
}

#[test]
fn memory_scope_doc_records_deferred_tool_decision() {
    let scope_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("internal")
        .join("MEMORY_SCOPE.md");
    let body = std::fs::read_to_string(&scope_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", scope_path.display()));
    assert!(
        body.contains("user_memory_max_bytes"),
        "scope doc must anchor to the existing config field"
    );
    assert!(
        body.contains("declines to ship a tool-mediated memory pipeline"),
        "scope doc must state the deferred decision in absolute terms"
    );
    assert!(
        body.contains("memory_append"),
        "scope doc must name the staged tool surface for future adoption"
    );
}

#[test]
fn small_fast_model_resolves_per_provider_default() {
    assert_eq!(
        small_fast_model_for_provider("anthropic"),
        Some(ANTHROPIC_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("openai"),
        Some(OPENAI_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("google"),
        Some(GOOGLE_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("bedrock"),
        Some(BEDROCK_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("azure_openai"),
        Some(AZURE_OPENAI_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("openrouter"),
        Some(OPENROUTER_SMALL_FAST_MODEL)
    );
    // Ollama serves a single local model; no separate cheap tier.
    assert_eq!(small_fast_model_for_provider("ollama"), None);
    assert_eq!(small_fast_model_for_provider("unknown"), None);
}

#[test]
fn resolved_small_fast_model_prefers_config_override() {
    let mut config = AppConfig::from_env_vars(Some("anthropic"), |_| None);
    config.small_fast_model = Some("claude-haiku-custom".to_string());
    assert_eq!(
        config.resolved_small_fast_model().as_deref(),
        Some("claude-haiku-custom")
    );
}

#[test]
fn resolved_small_fast_model_falls_back_to_provider_default() {
    let config = AppConfig::from_env_vars(Some("anthropic"), |_| None);
    assert_eq!(
        config.resolved_small_fast_model().as_deref(),
        Some(ANTHROPIC_SMALL_FAST_MODEL)
    );
}

#[test]
fn resolved_small_fast_model_returns_none_for_ollama_without_override() {
    let config = AppConfig::from_env_vars(Some("ollama"), |_| None);
    assert_eq!(config.resolved_small_fast_model(), None);
}

#[test]
fn small_fast_model_reads_env_var() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "SQUEEZY_SMALL_FAST_MODEL".to_string(),
        "haiku-from-env".to_string(),
    );
    overrides.insert("SQUEEZY_PROVIDER".to_string(), "anthropic".to_string());
    let config = AppConfig::from_env_vars(None, |name| overrides.get(name).cloned());
    assert_eq!(config.small_fast_model.as_deref(), Some("haiku-from-env"));
    assert_eq!(
        config.resolved_small_fast_model().as_deref(),
        Some("haiku-from-env")
    );
}

#[test]
fn small_fast_model_reads_toml_setting() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "anthropic"
small_fast_model = "claude-haiku-from-toml"
fn config_rejects_http_base_url_for_non_loopback_host() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"

[providers.openai]
base_url = "http://attacker.example.com/v1"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(
        config.small_fast_model.as_deref(),
        Some("claude-haiku-from-toml")
    );

    let error = AppConfig::try_from_settings_and_env_vars(settings, None, |_| None)
        .expect_err("non-loopback http base_url must be rejected");
    let msg = error.to_string();
    assert!(msg.contains("providers.openai.base_url"), "{msg}");
    assert!(msg.contains("https://"), "{msg}");
}

#[test]
fn config_accepts_http_base_url_for_loopback_hosts() {
    for host in ["localhost", "127.0.0.1", "127.5.6.7", "[::1]"] {
        let toml = format!(
            r#"
[model]
provider = "ollama"

[providers.ollama]
base_url = "http://{host}:11434/api"
"#
        );
        let settings = SettingsFile::from_toml_str(&toml, "test").expect("settings parse");
        AppConfig::try_from_settings_and_env_vars(settings, None, |_| None)
            .unwrap_or_else(|err| panic!("loopback host {host:?} must be accepted: {err}"));
    }
}

#[test]
fn config_rejects_http_base_url_for_private_lan_host() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"

[providers.openai]
base_url = "http://192.168.1.50:8080/v1"
"#,
        "test",
    )
    .expect("settings parse");

    let error = AppConfig::try_from_settings_and_env_vars(settings, None, |_| None)
        .expect_err("LAN http base_url must be rejected");
    assert!(error.to_string().contains("https://"));
}
