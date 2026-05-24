use std::sync::atomic::AtomicU64;

use super::*;

static CONFIG_TEST_NONCE: AtomicU64 = AtomicU64::new(0);

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
    let config = AppConfig::from_env_vars(None, |_| None);
    assert_eq!(config.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(config.max_output_tokens, Some(DEFAULT_MAX_OUTPUT_TOKENS));
    assert_eq!(config.permissions, PermissionPolicy::default());
    assert_eq!(config.session_mode, SessionMode::Build);
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
            assert_eq!(openai.api_key_env, "OPENAI_API_KEY");
            assert_eq!(openai.base_url, DEFAULT_OPENAI_BASE_URL);
        }
        _ => panic!("expected OpenAI provider"),
    }
}

#[test]
fn shell_sandbox_settings_parse_and_round_trip() {
    let settings = SettingsFile::from_toml_str(
        r#"
[permissions.shell_sandbox]
mode = "best_effort"
network = "allow_when_approved"
audit = false
kill_grace_ms = 500
env_allowlist = ["PATH", "LC_*"]
sensitive_path_patterns = [".ssh/**", ".env*"]
replace_sensitive_path_patterns = true
"#,
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
        config.permissions.shell_sandbox.sensitive_path_patterns,
        [".ssh/**", ".env*"]
    );

    let inspect = config.inspect_redacted();
    assert!(inspect.contains("[permissions.shell_sandbox]"));
    assert!(inspect.contains("mode = \"best_effort\""));
    let round_tripped = SettingsFile::from_toml_str(&inspect, "round-trip")
        .expect("inspect output parses back as settings");
    let round_tripped_config = AppConfig::from_settings_and_env_vars(round_tripped, |_| None);
    assert_eq!(
        round_tripped_config.permissions.shell_sandbox, config.permissions.shell_sandbox,
        "inspect output must round-trip to the same effective sandbox config",
    );
}

#[test]
fn config_reads_supported_env_overrides() {
    let config = AppConfig::from_env_vars(None, |name| match name {
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
        "SQUEEZY_MAX_TOOL_CALLS_PER_TURN" => Some("12".to_string()),
        "SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN" => Some("3456".to_string()),
        "SQUEEZY_MAX_SEARCH_FILES_PER_TURN" => Some("78".to_string()),
        "SQUEEZY_TELEMETRY" => Some("off".to_string()),
        "SQUEEZY_TELEMETRY_ENDPOINT" => Some("https://telemetry.example/v1/batch".to_string()),
        "SQUEEZY_SESSION_MODE" => Some("plan".to_string()),
        "SQUEEZY_SKILLS_USER_DIR" => Some("/tmp/squeezy-skills".to_string()),
        "SQUEEZY_SKILLS_COMPAT_USER_DIR" => Some("/tmp/agent-skills".to_string()),
        _ => None,
    });

    assert_eq!(config.model, "custom-model");
    assert_eq!(config.permissions.edit, PermissionMode::Allow);
    assert_eq!(config.permissions.shell, PermissionMode::Deny);
    assert_eq!(config.permissions.web, PermissionMode::Allow);
    assert_eq!(config.session_mode, SessionMode::Plan);
    assert!(config.store_responses);
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
fn config_can_select_anthropic_provider_defaults() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(config.model, DEFAULT_ANTHROPIC_MODEL);
    match config.provider {
        ProviderConfig::Anthropic(anthropic) => {
            assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
            assert_eq!(anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        }
        _ => panic!("expected Anthropic provider"),
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
base_url = "http://ollama.example/api"
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
            assert_eq!(ollama.base_url, "http://ollama.example/api");
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
max_output_tokens = 512
store_responses = true

[budgets]
max_parallel_tools = 3
tool_spill_threshold_bytes = 1000
tool_preview_bytes = 200
max_tool_result_bytes_per_round = 3000
tool_output_retention_days = 2
max_tool_calls_per_turn = 4
max_tool_bytes_read_per_turn = 5000
max_search_files_per_turn = 6

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

[tui]
tick_rate_ms = 75
status_verbosity = "verbose"

[mcp.servers.docs]
enabled = true
transport = "http"
url = "https://docs.example/mcp"
timeout_ms = 5000
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
    assert_eq!(config.max_output_tokens, Some(512));
    assert!(config.store_responses);
    assert_eq!(config.session_mode, SessionMode::Plan);
    assert_eq!(config.max_parallel_tools, 3);
    assert_eq!(config.tool_spill_threshold_bytes, 1000);
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
    assert_eq!(config.tui.tick_rate_ms, 75);
    assert_eq!(config.tui.status_verbosity, StatusVerbosity::Verbose);
    assert_eq!(config.mcp_servers["docs"].transport, McpTransport::Http);
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
fn config_source_labels_strip_paths() {
    let mut config = AppConfig::from_env_vars(None, |_| None);
    config.config_sources = vec![
        "defaults".to_string(),
        "user:/home/me/.squeezy/settings.toml".to_string(),
        "project:/repo/squeezy.toml".to_string(),
        "env".to_string(),
        "cli".to_string(),
    ];

    assert_eq!(
        config.config_source_labels(),
        vec!["defaults", "user", "project", "env", "cli"],
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
    assert!(inspect.contains("read = \"deny\""));
    assert!(inspect.contains("status_verbosity = \"compact\""));
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

    let (settings, sources) =
        load_settings_from_paths(Some(user_path.as_path()), Some(project_path.as_path()))
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

    let (settings, sources) =
        load_settings_from_paths(Some(user_path.as_path()), Some(project_path.as_path()))
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
