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
fn context_attachment_detection_matches_large_stack_markers_case_insensitively() {
    let text = format!("{}\nSTACK BACKTRACE:\n 0: foo\n", "prefix\n".repeat(1_024));
    assert_eq!(
        detect_context_attachment_kind(Some("panic.txt"), text.as_bytes(), Some(&text)),
        ContextAttachmentKind::StackTrace
    );
}

#[test]
fn context_attachment_detection_routes_canonical_images_to_vision_kind() {
    // PNG magic bytes round-trip into the routable `Image` kind so
    // F18 paste/file attachments can fan into `LlmInputItem::Image`
    // when the active model advertises vision.
    assert_eq!(
        detect_context_attachment_kind(Some("screenshot.png"), b"\x89PNG\r\n\x1a\nbytes", None),
        ContextAttachmentKind::Image
    );
    let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
    assert_eq!(
        detect_context_attachment_kind(Some("photo.jpg"), &jpeg, None),
        ContextAttachmentKind::Image
    );
    // Label-only image-shape (e.g. `.heic`) with non-canonical body
    // stays `UnsupportedImage` so we don't ship a non-vision payload
    // to a provider that can't decode it.
    assert_eq!(
        detect_context_attachment_kind(Some("snapshot.heic"), b"not real heic bytes", None),
        ContextAttachmentKind::UnsupportedImage
    );
    assert_eq!(
        detect_context_attachment_kind(Some("blob.bin"), b"abc\0def", Some("abc\0def")),
        ContextAttachmentKind::UnsupportedBinary
    );
}

#[test]
fn detect_image_mime_recognises_each_vision_format() {
    assert_eq!(
        detect_image_mime(b"\x89PNG\r\n\x1a\nrest"),
        Some("image/png")
    );
    assert_eq!(detect_image_mime(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
    assert_eq!(detect_image_mime(b"GIF87axxxx"), Some("image/gif"));
    assert_eq!(detect_image_mime(b"GIF89axxxx"), Some("image/gif"));
    let mut webp = Vec::new();
    webp.extend_from_slice(b"RIFF");
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(b"WEBPVP8 ");
    assert_eq!(detect_image_mime(&webp), Some("image/webp"));
    assert_eq!(detect_image_mime(b"plain text content"), None);
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
    // `contains_byte` is a point-membership test and stays inclusive on
    // `end_byte`: callers probe an exclusive half-open boundary (another
    // span's `end_byte`) and need a child exactly filling its parent to read
    // as inside. Span-vs-span containment is half-open via `contains_span`.
    assert!(span.contains_byte(20));
    assert!(!span.contains_byte(21));
}

fn span(start: u32, end: u32) -> SourceSpan {
    SourceSpan::new(start, end, SourcePoint::new(0, 0), SourcePoint::new(0, 0))
}

#[test]
fn source_span_contains_span_is_half_open_at_boundary() {
    let parent = span(10, 20);

    // A reference exactly filling the parent is contained: `end == end` is
    // fine because the child's last addressed byte is `end_byte - 1`.
    assert!(parent.contains_span(span(10, 20)));
    assert!(parent.contains_span(span(12, 18)));
    assert!(parent.contains_span(span(10, 11)));

    // Boundary touch is NOT containment: a span `[a, b)` does not contain a
    // span starting exactly at `b` (here the parent's exclusive end, 20).
    assert!(!parent.contains_span(span(20, 25)));
    // Nor a zero-width span sitting on the exclusive boundary.
    assert!(!parent.contains_span(span(20, 20)));
    // Spilling past either edge is excluded.
    assert!(!parent.contains_span(span(5, 12)));
    assert!(!parent.contains_span(span(18, 22)));

    // An empty parent addresses no bytes and contains nothing.
    assert!(!span(10, 10).contains_span(span(10, 10)));
}

#[test]
fn source_span_overlaps_is_half_open_at_boundary() {
    let base = span(10, 20);

    assert!(base.overlaps(span(15, 25)));
    assert!(base.overlaps(span(5, 12)));
    assert!(base.overlaps(span(12, 18)));

    // Boundary touch is NOT an overlap: a span ending exactly where another
    // starts (`end == start`) shares no addressed byte.
    assert!(!base.overlaps(span(20, 30)));
    assert!(!base.overlaps(span(0, 10)));
    // Empty spans never overlap.
    assert!(!base.overlaps(span(15, 15)));
}

#[test]
fn config_without_env_uses_openai_provider_defaults() {
    let config = AppConfig::from_env_vars(None, |_| None);
    assert_eq!(config.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(config.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(config.permissions, PermissionPolicy::default());
    // Opt-in default: the shipped preset is Default (human prompts), with the
    // LLM reviewer off until the user selects Auto-review.
    assert_eq!(config.permissions.mode, PermissionPolicyMode::Default);
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
    assert_eq!(config.max_parallel_tools, DEFAULT_MAX_PARALLEL_TOOLS);
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
fn per_provider_routing_settings_parse_and_stay_per_provider() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"

[routing]
heuristic = false
follow_up_max_chars = 40

[providers.openai]
cheap_model = "gpt-5.4-nano"
judge_model = "gpt-5.4-mini"
expensive_models = "gpt-5|codex"

[providers.anthropic]
cheap_model = "claude-haiku-4-5"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    // Global toggles land on RoutingConfig.
    assert!(!config.routing.heuristic);
    assert_eq!(config.routing.follow_up_max_chars, 40);

    // Per-provider routing is retained per provider and never crosses over.
    let openai = config.providers.get("openai").expect("openai entry");
    assert_eq!(openai.judge_model.as_deref(), Some("gpt-5.4-mini"));
    assert_eq!(openai.expensive_models.as_deref(), Some("gpt-5|codex"));
    assert_eq!(
        config
            .providers
            .get("anthropic")
            .and_then(|p| p.cheap_model.as_deref()),
        Some("claude-haiku-4-5")
    );
    // Anthropic carries no judge override — it inherits its own default later.
    assert!(
        config
            .providers
            .get("anthropic")
            .and_then(|p| p.judge_model.as_deref())
            .is_none()
    );

    // Inspect output is still valid TOML.
    SettingsFile::from_toml_str(&config.inspect_redacted(), "round-trip")
        .expect("inspect output parses back");
}

#[test]
fn routing_config_fields_display_resolved_defaults_not_blanks() {
    // On OpenAI with no per-provider overrides, the Routing /config fields show
    // the resolved values in effect (not empty) — the provider banner, the
    // built-in cheap and judge models (both the mini tier), and the built-in
    // judge prompt.
    let settings =
        SettingsFile::from_toml_str("[model]\nprovider = \"openai\"\n", "test").expect("parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    let routing = crate::config_schema::CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == crate::config_schema::SectionId::Routing)
        .expect("routing section");
    let display = |label: &str| -> String {
        let f = routing
            .fields
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("field {label}"));
        (f.get)(&config).as_display()
    };

    assert!(
        display("provider").contains("openai"),
        "{}",
        display("provider")
    );
    assert_eq!(display("cheap_model"), "gpt-5.4-mini");
    assert_eq!(display("judge_model"), "gpt-5.4-mini");
    assert!(
        display("judge_prompt")
            .to_lowercase()
            .contains("routing classifier"),
        "judge_prompt should show the built-in prompt in effect"
    );
}

#[test]
fn reroute_filter_negates_and_defaults_reflect_reality() {
    use crate::{default_reroute_filter, parent_is_reroute_eligible};

    // openai/azure: flagships reroute, the mini/nano tiers are skipped.
    let openai = default_reroute_filter("openai");
    assert!(parent_is_reroute_eligible("gpt-5.5", openai));
    assert!(parent_is_reroute_eligible("gpt-5.4-codex", openai));
    assert!(!parent_is_reroute_eligible("gpt-5.4-mini", openai));
    assert!(!parent_is_reroute_eligible("gpt-5.4-nano", openai));

    // anthropic: opus AND sonnet reroute (the dropped-opus regression), haiku
    // is skipped — including the bedrock-prefixed id, proving name-based scales.
    let anthropic = default_reroute_filter("anthropic");
    assert!(parent_is_reroute_eligible("claude-opus-4-6", anthropic));
    assert!(parent_is_reroute_eligible("claude-sonnet-4-6", anthropic));
    assert!(!parent_is_reroute_eligible(
        "claude-haiku-4-5-20251001",
        anthropic
    ));
    let bedrock = default_reroute_filter("bedrock");
    assert!(parent_is_reroute_eligible(
        "anthropic.claude-opus-4-6-v1:0",
        bedrock
    ));
    assert!(!parent_is_reroute_eligible(
        "anthropic.claude-haiku-4-5-20251001-v1:0",
        bedrock
    ));

    // google/vertex: pro reroutes, flash and flash-lite are skipped.
    let google = default_reroute_filter("google");
    assert!(parent_is_reroute_eligible("gemini-2.5-pro", google));
    assert!(!parent_is_reroute_eligible("gemini-2.5-flash", google));
    assert!(!parent_is_reroute_eligible("gemini-2.5-flash-lite", google));

    // A positive regex restricts to matches; a negative lookahead excludes;
    // empty = reroute any. Plain standard regexes (lookaround supported).
    assert!(parent_is_reroute_eligible("claude-opus-4-6", "opus|sonnet"));
    assert!(!parent_is_reroute_eligible(
        "claude-haiku-4-5",
        "opus|sonnet"
    ));
    assert!(!parent_is_reroute_eligible(
        "gpt-5.4-codex",
        "^(?!.*codex).*"
    ));
    assert!(parent_is_reroute_eligible("gpt-5.5", "^(?!.*codex).*"));
    assert!(parent_is_reroute_eligible("anything", ""));

    // The "ge[mini]" trap: a gateway serving gemini-pro must NOT be excluded by
    // the generic "-mini" tier filter, while the real cheap tiers still are.
    let gateway = default_reroute_filter("openrouter");
    assert!(parent_is_reroute_eligible("google/gemini-2.5-pro", gateway));
    assert!(!parent_is_reroute_eligible(
        "google/gemini-2.5-flash",
        gateway
    ));
    assert!(!parent_is_reroute_eligible("openai/gpt-5.4-mini", gateway));
    assert!(!parent_is_reroute_eligible(
        "anthropic/claude-haiku-4-5",
        gateway
    ));
    assert!(parent_is_reroute_eligible(
        "anthropic/claude-opus-4-6",
        gateway
    ));
}

#[test]
fn resolved_reroute_filter_precedence() {
    use crate::{ProviderSettings, default_reroute_filter, resolved_reroute_filter};
    let mut cfg = AppConfig::default();
    // No override + empty global → built-in per-provider default.
    assert_eq!(
        resolved_reroute_filter(&cfg, "openai"),
        default_reroute_filter("openai")
    );
    // Non-empty global overrides the built-in default.
    cfg.routing.expensive_models = "gpt-5".to_string();
    assert_eq!(resolved_reroute_filter(&cfg, "openai"), "gpt-5");
    // A per-provider override wins, including an explicit empty string.
    cfg.providers.insert(
        "openai".to_string(),
        ProviderSettings {
            expensive_models: Some(String::new()),
            ..Default::default()
        },
    );
    assert!(resolved_reroute_filter(&cfg, "openai").is_empty());
}

#[test]
fn subagent_config_defaults_match_parent_per_turn_caps() {
    // The cost broker — not these per-call caps — is the load-bearing
    // safeguard on runaway subagent cost. Keeping subagent defaults at
    // parity with the parent per-turn caps avoids surprising aborts on
    // real workloads (a general-purpose explore subagent should not get
    // 10x/20x less headroom than the turn that spawned it).
    let defaults = SubagentConfig::default();
    assert_eq!(
        defaults.max_tool_calls_per_call,
        DEFAULT_MAX_TOOL_CALLS_PER_TURN
    );
    assert_eq!(
        defaults.max_tool_bytes_read_per_call,
        DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN
    );
    assert_eq!(
        defaults.max_search_files_per_call,
        DEFAULT_MAX_SEARCH_FILES_PER_TURN
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
policy = "Never auto-approve writes to generated files."
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
    assert_eq!(
        config.permissions.ai_reviewer.policy.as_deref(),
        Some("Never auto-approve writes to generated files.")
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
fn config_reads_skill_extra_roots_from_settings_file() {
    // Operators ship the same `extra_roots` value via a shared settings
    // file (network drive, vendored submodule). The loader must accept a
    // string array and pass each path through tilde expansion so a team
    // root like `~/team-skills` resolves the same way `user_dir` would.
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let settings = SettingsFile::from_toml_str(
        r#"
[skills]
extra_roots = ["/mnt/team-skills", "~/team-skills"]
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(
        config.skills.extra_roots,
        vec![PathBuf::from("/mnt/team-skills"), home.join("team-skills"),]
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
fn model_table_provider_and_model_override_defaults() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
reasoning_effort = "medium"
"#,
        "test",
    )
    .expect("settings parse");

    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    assert_eq!(config.model, "claude-haiku-4-5-20251001");
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Medium));
    assert!(matches!(config.provider, ProviderConfig::Anthropic(_)));
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
fn config_can_select_github_copilot_oauth_provider() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("github-copilot".to_string()),
        _ => None,
    });
    assert!(matches!(config.provider, ProviderConfig::GitHubCopilot(_)));
    assert_eq!(config.model, DEFAULT_GITHUB_COPILOT_MODEL);
}

#[test]
fn config_resolves_opus_alias_to_full_id() {
    let anthropic = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("anthropic".to_string()),
        "SQUEEZY_MODEL" => Some("opus".to_string()),
        _ => None,
    });
    assert!(matches!(anthropic.provider, ProviderConfig::Anthropic(_)));
    assert_eq!(anthropic.model, "claude-opus-4-7");

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
fn permission_policy_modes_apply_presets() {
    let implicit = AppConfig::from_env_vars(None, |_| None);
    // Opt-in default: implicit config is the Default preset with the reviewer off.
    assert_eq!(implicit.permissions.mode, PermissionPolicyMode::Default);
    assert_eq!(implicit.permissions.read, PermissionMode::Allow);
    assert_eq!(implicit.permissions.edit, PermissionMode::Allow);
    assert_eq!(implicit.permissions.shell, PermissionMode::Allow);
    assert_eq!(implicit.permissions.web, PermissionMode::Ask);
    assert!(!implicit.permissions.ai_reviewer.enabled);
    assert_eq!(
        implicit.permissions.shell_sandbox.network,
        ShellSandboxNetworkPolicy::AllowWhenApproved
    );

    let default = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "default"
"#,
        "test",
    )
    .expect("settings parse");
    let default = AppConfig::from_settings_and_env_vars(default, |_| None);
    assert_eq!(default.permissions.mode, PermissionPolicyMode::Default);
    assert_eq!(default.permissions.read, PermissionMode::Allow);
    assert_eq!(default.permissions.edit, PermissionMode::Allow);
    assert_eq!(default.permissions.shell, PermissionMode::Allow);
    assert_eq!(default.permissions.web, PermissionMode::Ask);
    assert!(!default.permissions.ai_reviewer.enabled);
    assert_eq!(
        default.permissions.shell_sandbox.network,
        ShellSandboxNetworkPolicy::AllowWhenApproved
    );

    let auto_review = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "auto_review"
"#,
        "test",
    )
    .expect("settings parse");
    let auto_review = AppConfig::from_settings_and_env_vars(auto_review, |_| None);
    assert_eq!(
        auto_review.permissions.mode,
        PermissionPolicyMode::AutoReview
    );
    assert!(auto_review.permissions.ai_reviewer.enabled);
    assert_eq!(
        auto_review.permissions.ai_reviewer.allow_capabilities,
        vec![
            PermissionCapability::Read,
            PermissionCapability::Search,
            PermissionCapability::Network,
            PermissionCapability::Mcp,
            PermissionCapability::Edit,
            PermissionCapability::Shell,
            PermissionCapability::Git,
            PermissionCapability::Compiler,
        ]
    );
    // Auto-review routes the workspace-write capabilities through the reviewer.
    assert_eq!(auto_review.permissions.shell, PermissionMode::Ask);
    assert_eq!(auto_review.permissions.edit, PermissionMode::Ask);
    assert_eq!(auto_review.permissions.git, PermissionMode::Ask);
    assert_eq!(auto_review.permissions.compiler, PermissionMode::Ask);

    let full_access = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "full_access"
"#,
        "test",
    )
    .expect("settings parse");
    let full_access = AppConfig::from_settings_and_env_vars(full_access, |_| None);
    assert_eq!(
        full_access.permissions.mode,
        PermissionPolicyMode::FullAccess
    );
    assert_eq!(full_access.permissions.web, PermissionMode::Allow);
    assert_eq!(full_access.permissions.mcp, PermissionMode::Allow);
    assert_eq!(full_access.permissions.destructive, PermissionMode::Allow);
    assert_eq!(
        full_access.permissions.shell_sandbox.mode,
        ShellSandboxMode::Off
    );

    let mut outside = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "read_file".to_string(),
        capability: PermissionCapability::Read,
        target: "path:/tmp/outside.txt".to_string(),
        risk: PermissionRisk::Medium,
        summary: "read outside".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    outside
        .metadata
        .insert("outside_workspace".to_string(), "true".to_string());
    assert_eq!(
        default.permissions.evaluate(&outside).action,
        PermissionAction::Ask
    );
    assert_eq!(
        full_access.permissions.evaluate(&outside).action,
        PermissionAction::Allow
    );
}

#[test]
fn shell_writes_outside_workspace_escalate_unless_full_access() {
    fn policy(mode: &str) -> AppConfig {
        let settings =
            SettingsFile::from_toml_str(&format!("[permissions]\nmode = \"{mode}\"\n"), "test")
                .expect("settings parse");
        AppConfig::from_settings_and_env_vars(settings, |_| None)
    }

    fn shell_request(outside: bool) -> PermissionRequest {
        let mut metadata = BTreeMap::new();
        metadata.insert("command".to_string(), "cp secret /etc/passwd".to_string());
        if outside {
            metadata.insert("outside_workspace".to_string(), "true".to_string());
        }
        PermissionRequest {
            call_id: "call".to_string(),
            tool_name: "shell".to_string(),
            capability: PermissionCapability::Shell,
            target: "shell:cp:*".to_string(),
            risk: PermissionRisk::High,
            summary: "shell write".to_string(),
            metadata,
            suggested_rules: Vec::new(),
        }
    }

    // Default keeps shell = Allow, so an in-workspace shell write auto-allows,
    // but an out-of-workspace write escalates to a prompt (the shell hole that
    // previously auto-allowed `cp secret /etc/passwd`).
    let default = policy("default");
    assert_eq!(
        default.permissions.evaluate(&shell_request(false)).action,
        PermissionAction::Allow,
        "default: in-workspace shell write should auto-allow"
    );
    assert_eq!(
        default.permissions.evaluate(&shell_request(true)).action,
        PermissionAction::Ask,
        "default: out-of-workspace shell write should escalate"
    );

    // Auto-review routes shell through the reviewer, so both in- and
    // out-of-workspace writes reach Ask (the reviewer adjudicates in-workspace
    // ones; out-of-workspace is never auto-approved).
    let auto_review = policy("auto_review");
    assert_eq!(
        auto_review
            .permissions
            .evaluate(&shell_request(false))
            .action,
        PermissionAction::Ask,
        "auto_review: in-workspace shell routes to the reviewer"
    );
    assert_eq!(
        auto_review
            .permissions
            .evaluate(&shell_request(true))
            .action,
        PermissionAction::Ask,
        "auto_review: out-of-workspace shell escalates"
    );

    // Full access never prompts, even for an out-of-workspace shell write.
    assert_eq!(
        policy("full_access")
            .permissions
            .evaluate(&shell_request(true))
            .action,
        PermissionAction::Allow
    );
}

#[test]
fn custom_permissions_and_legacy_defaults_remain_granular() {
    let custom = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "custom"

[permissions.custom]
shell = "deny"
network = "allow"
destructive = "deny"
"#,
        "test",
    )
    .expect("settings parse");
    let custom = AppConfig::from_settings_and_env_vars(custom, |_| None);
    assert_eq!(custom.permissions.mode, PermissionPolicyMode::Custom);
    assert_eq!(custom.permissions.shell, PermissionMode::Deny);
    assert_eq!(custom.permissions.web, PermissionMode::Allow);
    assert_eq!(custom.permissions.destructive, PermissionMode::Deny);

    let custom_table_without_mode = SettingsFile::from_toml_str(
        r#"
[permissions.custom]
shell = "deny"
"#,
        "test",
    )
    .expect("settings parse");
    let custom_table_without_mode =
        AppConfig::from_settings_and_env_vars(custom_table_without_mode, |_| None);
    assert_eq!(
        custom_table_without_mode.permissions.mode,
        PermissionPolicyMode::Custom
    );
    assert_eq!(
        custom_table_without_mode.permissions.shell,
        PermissionMode::Deny
    );
    assert_eq!(
        custom_table_without_mode.permissions.git,
        PermissionMode::Allow
    );
    assert_eq!(
        custom_table_without_mode.permissions.compiler,
        PermissionMode::Allow
    );
    assert_eq!(
        custom_table_without_mode.permissions.destructive,
        PermissionMode::Ask
    );

    let explicit_custom_top_level_shell = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "custom"
shell = "deny"
"#,
        "test",
    )
    .expect("settings parse");
    let explicit_custom_top_level_shell =
        AppConfig::from_settings_and_env_vars(explicit_custom_top_level_shell, |_| None);
    assert_eq!(
        explicit_custom_top_level_shell.permissions.mode,
        PermissionPolicyMode::Custom
    );
    assert_eq!(
        explicit_custom_top_level_shell.permissions.shell,
        PermissionMode::Deny
    );
    assert_eq!(
        explicit_custom_top_level_shell.permissions.git,
        PermissionMode::Allow
    );
    assert_eq!(
        explicit_custom_top_level_shell.permissions.compiler,
        PermissionMode::Allow
    );
    assert_eq!(
        explicit_custom_top_level_shell.permissions.destructive,
        PermissionMode::Ask
    );

    let legacy = SettingsFile::from_toml_str(
        r#"
[permissions]
shell = "deny"
"#,
        "test",
    )
    .expect("settings parse");
    let legacy = AppConfig::from_settings_and_env_vars(legacy, |_| None);
    assert_eq!(legacy.permissions.mode, PermissionPolicyMode::Custom);
    assert_eq!(legacy.permissions.shell, PermissionMode::Deny);
    assert_eq!(legacy.permissions.git, PermissionMode::Deny);
    assert_eq!(legacy.permissions.compiler, PermissionMode::Deny);
    assert_eq!(legacy.permissions.destructive, PermissionMode::Deny);
}

#[test]
fn permission_env_overrides_apply_after_mode_presets() {
    let auto_review = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "auto_review"

[permissions.ai_reviewer]
enabled = false
allow_capabilities = ["read"]
"#,
        "test",
    )
    .expect("settings parse");
    let auto_review = AppConfig::from_settings_and_env_vars(auto_review, |name| match name {
        "SQUEEZY_WEB_PERMISSION" => Some("deny".to_string()),
        "SQUEEZY_GIT_PERMISSION" => Some("ask".to_string()),
        "SQUEEZY_SHELL_PERMISSION" => Some("deny".to_string()),
        _ => None,
    });
    assert_eq!(
        auto_review.permissions.mode,
        PermissionPolicyMode::AutoReview
    );
    // Selecting Auto-review enables the reviewer even though the TOML set
    // enabled = false (the preset governs the toggle)...
    assert!(auto_review.permissions.ai_reviewer.enabled);
    // ...but a configured allow_capabilities is respected (tunable remit),
    // rather than being force-reset to the Auto-review default set.
    assert_eq!(
        auto_review.permissions.ai_reviewer.allow_capabilities,
        vec![PermissionCapability::Read]
    );
    assert_eq!(auto_review.permissions.web, PermissionMode::Deny);
    assert_eq!(auto_review.permissions.git, PermissionMode::Ask);
    assert_eq!(auto_review.permissions.shell, PermissionMode::Deny);

    let full_access = SettingsFile::from_toml_str(
        r#"
[permissions]
mode = "full_access"
"#,
        "test",
    )
    .expect("settings parse");
    let full_access = AppConfig::from_settings_and_env_vars(full_access, |name| match name {
        "SQUEEZY_WEB_PERMISSION" => Some("ask".to_string()),
        "SQUEEZY_DESTRUCTIVE_PERMISSION" => Some("deny".to_string()),
        _ => None,
    });
    assert_eq!(
        full_access.permissions.mode,
        PermissionPolicyMode::FullAccess
    );
    assert_eq!(full_access.permissions.web, PermissionMode::Ask);
    assert_eq!(full_access.permissions.destructive, PermissionMode::Deny);
    assert_eq!(
        full_access.permissions.shell_sandbox.mode,
        ShellSandboxMode::Off
    );
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
    assert!(!wildcard_match("acd", "a*c*cd"));
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

#[test]
fn silent_rule_evaluates_to_silent_deny() {
    let settings = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "destructive"
target = "rm:-rf:/"
action = "deny"
source = "user"
silent = true
reason = "absolute deny rule for rm -rf /"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);

    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Destructive,
        target: "rm:-rf:/".to_string(),
        risk: PermissionRisk::High,
        summary: "rm -rf /".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };

    let verdict = config.permissions.evaluate(&request);
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(verdict.silent, "silent rule must produce silent verdict");
    assert_eq!(
        verdict.matched_rule.as_ref().map(|rule| rule.silent),
        Some(true),
    );
    assert!(
        verdict.reason.contains("absolute deny"),
        "verdict reason should retain rule reason: {}",
        verdict.reason,
    );
}

#[test]
fn non_silent_deny_rule_leaves_verdict_silent_false() {
    let settings = SettingsFile::from_toml_str(
        r#"
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

    let verdict = config.permissions.evaluate(&shell_request("cargo test:*"));
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(
        !verdict.silent,
        "deny rule without silent=true must not produce a silent verdict",
    );
}

#[test]
fn silent_true_on_non_deny_rule_is_rejected_at_load_time() {
    let result = SettingsFile::from_toml_str(
        r#"
[[permissions.rules]]
capability = "shell"
target = "ls:*"
action = "ask"
source = "user"
silent = true
"#,
        "test",
    );
    let err = result.expect_err("silent=true on ask rule must fail to load");
    let msg = format!("{err}");
    assert!(
        msg.contains("silent = true is only valid on Deny rules"),
        "unexpected message: {msg}",
    );
}

#[test]
fn silent_deny_propagates_via_session_layer_in_evaluate_with_extra() {
    let policy = PermissionPolicy::default();
    let session_rule = PermissionRule::new(
        "shell",
        "shred:*",
        PermissionAction::Deny,
        PermissionRuleSource::Session,
        Some("policy: shred is forbidden".to_string()),
    )
    .with_silent(true);
    let verdict = policy.evaluate_with_extra(
        &shell_request("shred:*"),
        std::slice::from_ref(&session_rule),
    );
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(verdict.silent);
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
env_allowlist = ["*"]
"#,
    )
    .expect_err("env_allowlist bare * must be rejected");
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

    let sensitive = ShellSandboxConfig::from_settings(
        Some(ShellSandboxSettings {
            read_roots: Some(vec![ssh_root.display().to_string()]),
            ..ShellSandboxSettings::default()
        }),
        "test",
        &root,
    )
    .expect_err("workspace-sensitive roots must be rejected");
    assert!(format!("{sensitive}").contains("inside sensitive path"));

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
    assert_eq!(tail.text.len(), STREAM_TAIL_BYTES);
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
fn openai_carries_org_project_and_service_tier_from_env() {
    // Env vars beat TOML on these knobs so a per-shell override of
    // `OPENAI_PROJECT_ID` (e.g. `direnv`) doesn't get masked by a
    // checked-in repo `[providers.openai]` block. The provider then
    // emits `OpenAI-Organization` / `OpenAI-Project` headers and a
    // `service_tier` body field so spend attribution and tier
    // selection both land correctly without forking the provider.
    let openai = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openai".to_string()),
        "OPENAI_ORG_ID" => Some("org-PAYG".to_string()),
        "OPENAI_PROJECT_ID" => Some("proj_abc".to_string()),
        "OPENAI_SERVICE_TIER" => Some("flex".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAi(config) = &openai.provider else {
        panic!("expected openai");
    };
    assert_eq!(config.organization.as_deref(), Some("org-PAYG"));
    assert_eq!(config.project.as_deref(), Some("proj_abc"));
    assert_eq!(config.service_tier.as_deref(), Some("flex"));
}

#[test]
fn openai_falls_back_to_toml_for_org_project_service_tier() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderSettings {
            organization: Some("org-fromtoml".to_string()),
            project: Some("proj_fromtoml".to_string()),
            service_tier: Some("priority".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(settings, Some("openai"), |_| None)
        .expect("openai config builds");
    let ProviderConfig::OpenAi(openai) = &config.provider else {
        panic!("expected openai");
    };
    assert_eq!(openai.organization.as_deref(), Some("org-fromtoml"));
    assert_eq!(openai.project.as_deref(), Some("proj_fromtoml"));
    assert_eq!(openai.service_tier.as_deref(), Some("priority"));
}

#[test]
fn openai_org_project_service_tier_default_to_none() {
    let openai = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openai".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAi(config) = &openai.provider else {
        panic!("expected openai");
    };
    assert!(config.organization.is_none());
    assert!(config.project.is_none());
    assert!(config.service_tier.is_none());
}

#[test]
fn azure_openai_carries_extra_headers_from_settings() {
    // Operators wire `Apim-Subscription-Key` (and Entra ID `Authorization`
    // overrides) through the standard `[providers.azure_openai.headers]`
    // table so squeezy can front API-Management-protected endpoints
    // without forking the provider. The new `extra_headers` slot mirrors
    // the OpenAI-compatible preset shape so callers learn one TOML
    // convention.
    let mut providers = std::collections::BTreeMap::new();
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "Apim-Subscription-Key".to_string(),
        "apim-secret".to_string(),
    );
    headers.insert(
        "x-ms-client-request-id".to_string(),
        "trace-123".to_string(),
    );
    providers.insert(
        "azure_openai".to_string(),
        ProviderSettings {
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
        Some("azure_openai"),
        |name| match name {
            "AZURE_OPENAI_BASE_URL" => {
                Some("https://resource.openai.azure.com/openai/v1".to_string())
            }
            _ => None,
        },
    )
    .expect("azure config builds");
    let ProviderConfig::AzureOpenAi(azure) = &config.provider else {
        panic!("azure_openai must map to AzureOpenAi");
    };
    assert_eq!(
        azure
            .extra_headers
            .get("Apim-Subscription-Key")
            .map(String::as_str),
        Some("apim-secret"),
    );
    assert_eq!(
        azure
            .extra_headers
            .get("x-ms-client-request-id")
            .map(String::as_str),
        Some("trace-123"),
    );
}

#[test]
fn azure_openai_opts_into_entra_id_when_bearer_token_present() {
    // Operators that pre-populate `AZURE_OPENAI_BEARER_TOKEN` (via
    // `az account get-access-token`, IMDS, or a sidecar) don't need to
    // separately flip `use_entra_id`: presence of the token is enough
    // signal that the api-key path is the wrong default. The provider
    // surface then swaps `api-key` for `Authorization: Bearer …`.
    let azure = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("azure_openai".to_string()),
        "AZURE_OPENAI_BASE_URL" => Some("https://resource.openai.azure.com/openai/v1".to_string()),
        "AZURE_OPENAI_BEARER_TOKEN" => Some("entra-jwt".to_string()),
        _ => None,
    });
    let ProviderConfig::AzureOpenAi(config) = &azure.provider else {
        panic!("expected azure");
    };
    assert!(config.use_entra_id, "bearer token must imply Entra ID");
    assert_eq!(
        config.entra_bearer_token.as_deref(),
        Some("entra-jwt"),
        "bearer token must flow through the config so the provider can emit it",
    );
}

#[test]
fn azure_openai_use_entra_id_setting_persists_without_bearer_token() {
    // The TOML flag stays sticky even when the bearer token has not yet
    // been issued — the LLM provider's `from_azure_config` is responsible
    // for surfacing an explicit error rather than silently degrading to
    // the api-key path, so config-build cannot lose that signal.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "azure_openai".to_string(),
        ProviderSettings {
            use_entra_id: Some(true),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("azure_openai"),
        |name| match name {
            "AZURE_OPENAI_BASE_URL" => {
                Some("https://resource.openai.azure.com/openai/v1".to_string())
            }
            _ => None,
        },
    )
    .expect("azure config builds");
    let ProviderConfig::AzureOpenAi(azure) = &config.provider else {
        panic!("expected azure");
    };
    assert!(azure.use_entra_id);
    assert!(azure.entra_bearer_token.is_none());
}

#[test]
fn azure_openai_extra_headers_default_to_empty_map() {
    let azure = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("azure_openai".to_string()),
        "AZURE_OPENAI_BASE_URL" => Some("https://resource.openai.azure.com/openai/v1".to_string()),
        _ => None,
    });
    let ProviderConfig::AzureOpenAi(config) = &azure.provider else {
        panic!("expected azure");
    };
    assert!(
        config.extra_headers.is_empty(),
        "missing TOML section must leave the map empty so callers cannot accidentally forward stale headers",
    );
}

#[test]
fn vertex_base_url_uses_bare_host_for_global_location() {
    // Gemini 3.x is GA only via the `global` location, which lives at
    // bare `aiplatform.googleapis.com` (Google does not run a regional
    // Anycast frontend named `global`). The historical
    // `{location}-aiplatform.googleapis.com` shape DNS-fails for
    // `global`, so the helper must special-case it.
    assert_eq!(
        vertex_base_url("my-proj", "global"),
        "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/endpoints/openapi",
    );
    // Casing is normalized so a config that writes `Global` keeps
    // working.
    assert_eq!(
        vertex_base_url("my-proj", "Global"),
        "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/endpoints/openapi",
    );
}

#[test]
fn vertex_base_url_keeps_regional_shape_for_named_regions() {
    // Regions (and continental pseudo-regions like `us`/`eu`) keep the
    // historical `{location}-aiplatform.googleapis.com` host so existing
    // production deployments are unchanged.
    assert_eq!(
        vertex_base_url("my-proj", "us-central1"),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/endpoints/openapi",
    );
    assert_eq!(
        vertex_base_url("my-proj", "europe-west4"),
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/my-proj/locations/europe-west4/endpoints/openapi",
    );
}

#[test]
fn vertex_preset_resolves_global_location_to_bare_host() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vertex".to_string(),
        ProviderSettings {
            vertex_project: Some("my-project".to_string()),
            vertex_location: Some("global".to_string()),
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
        .expect("vertex config builds with global location");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vertex must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible.base_url,
        "https://aiplatform.googleapis.com/v1/projects/my-project/locations/global/endpoints/openapi",
        "the `global` location must resolve to the bare host, not `https://global-aiplatform.googleapis.com/...`",
    );
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
fn vertex_preset_opts_into_oauth_when_setting_set() {
    // The new `use_oauth = true` TOML setting flips the config flag
    // so the LLM client can construct a refreshing `VertexOAuthSource`
    // instead of snapshotting `VERTEX_ACCESS_TOKEN` at startup.
    // squeezy-core only surfaces the intent; the OAuth implementation
    // lives in squeezy-llm to keep the core layer transport-agnostic.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vertex".to_string(),
        ProviderSettings {
            vertex_project: Some("my-project".to_string()),
            vertex_location: Some("us-central1".to_string()),
            use_oauth: Some(true),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config = AppConfig::try_from_settings_and_env_vars(settings, Some("vertex"), |_| None)
        .expect("vertex config builds with oauth opt-in");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vertex must map to OpenAiCompatible");
    };
    assert!(compatible.use_oauth);
}

#[test]
fn vertex_preset_infers_oauth_from_application_credentials_env() {
    // When the operator has `GOOGLE_APPLICATION_CREDENTIALS` pointed at
    // a service-account JSON and has NOT pasted a static
    // `VERTEX_ACCESS_TOKEN`, infer that they want OAuth. This matches
    // gcloud / opencode behavior and removes the per-session token
    // refresh chore from the user's workflow.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vertex".to_string(),
        ProviderSettings {
            vertex_project: Some("my-project".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("vertex"), |name| match name {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some("/path/to/sa.json".to_string()),
            _ => None,
        })
        .expect("vertex config builds with ADC env");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vertex must map to OpenAiCompatible");
    };
    assert!(
        compatible.use_oauth,
        "ADC-style env without a static token must imply OAuth intent",
    );
}

#[test]
fn vertex_preset_static_token_disables_oauth_inference() {
    // The inference path defers to the user's choice: a static
    // `VERTEX_ACCESS_TOKEN` means they're managing refresh themselves
    // (e.g. a sidecar that rewrites the env every 50 minutes), so
    // we keep the legacy static-snapshot behavior.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vertex".to_string(),
        ProviderSettings {
            vertex_project: Some("my-project".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("vertex"), |name| match name {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some("/path/to/sa.json".to_string()),
            "VERTEX_ACCESS_TOKEN" => Some("ya29.live".to_string()),
            _ => None,
        })
        .expect("vertex config builds with both ADC env and static token");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vertex must map to OpenAiCompatible");
    };
    assert!(!compatible.use_oauth);
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
fn cerebras_preset_emits_max_completion_tokens_migration_warning() {
    // Cerebras' chat-completions v1 accepts `max_tokens`; v2
    // (default 2026-07-21) tightens validation to require
    // `max_completion_tokens`. The config layer surfaces a soft
    // warning when operators ship a config that pre-dates the
    // switch so they're not blindsided by the cutover.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "cerebras".to_string(),
        ProviderSettings {
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        model_settings: Some(ModelSettings {
            max_output_tokens: Some(4096),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("cerebras"), |name| match name {
            "CEREBRAS_API_KEY" => Some("cb-key".to_string()),
            _ => None,
        })
        .expect("cerebras config builds");
    assert!(
        config
            .config_warnings
            .iter()
            .any(|w| w.source == "providers.cerebras" && w.field.contains("max_completion_tokens")),
        "cerebras + max_output_tokens must surface the v2-cutover warning",
    );
}

#[test]
fn cerebras_preset_without_explicit_max_output_tokens_skips_warning() {
    // The v2-cutover warning is gated on the *resolved* `max_output_tokens`
    // being `Some(..)`. Because `DEFAULT_MAX_OUTPUT_TOKENS` is `None`, a
    // Cerebras config that sets no explicit cap resolves to `None` and must
    // stay silent — otherwise every default-budget Cerebras user would eat a
    // warning-storm. This pins that contract directly on a real Cerebras
    // config; if a future refactor flips `DEFAULT_MAX_OUTPUT_TOKENS` to
    // `Some(..)`, this test will fail loudly rather than the regression
    // shipping silently.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "cerebras".to_string(),
        ProviderSettings {
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        // No `model_settings.max_output_tokens`: leave the resolved value at
        // the `DEFAULT_MAX_OUTPUT_TOKENS` (`None`) so the warning gate stays
        // closed.
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("cerebras"), |name| match name {
            "CEREBRAS_API_KEY" => Some("cb-key".to_string()),
            _ => None,
        })
        .expect("cerebras config builds without an explicit max_output_tokens");
    assert!(
        config
            .config_warnings
            .iter()
            .all(|w| w.source != "providers.cerebras"),
        "cerebras with no resolved max_output_tokens must not surface the v2-cutover warning",
    );
}

#[test]
fn non_cerebras_preset_never_emits_cerebras_warning() {
    // Control for `cerebras_preset_emits_max_completion_tokens_migration_warning`:
    // the `providers.cerebras` warning is provider-scoped, so a non-Cerebras
    // provider must never emit it even when an explicit `max_output_tokens`
    // would have tripped the gate on Cerebras.
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openrouter".to_string()),
        "OPENROUTER_API_KEY" => Some("or-key".to_string()),
        "SQUEEZY_MAX_OUTPUT_TOKENS" => Some("4096".to_string()),
        _ => None,
    });
    assert!(
        config
            .config_warnings
            .iter()
            .all(|w| w.source != "providers.cerebras"),
        "non-Cerebras providers must not emit the Cerebras v2 warning",
    );
}

#[test]
fn vercel_preset_falls_back_to_oidc_token_env() {
    // Vercel runtimes inject `VERCEL_OIDC_TOKEN` (12h TTL) into every
    // function deployment. When the user has not pasted an
    // `AI_GATEWAY_API_KEY`, fall back to the OIDC token so a squeezy
    // session inside a Vercel function authenticates against AI Gateway
    // without per-deploy env juggling.
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("vercel".to_string()),
        "VERCEL_OIDC_TOKEN" => Some("eyJ.fake.oidc".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vercel must map to OpenAiCompatible");
    };
    assert_eq!(
        compatible.api_key_env, "VERCEL_OIDC_TOKEN",
        "missing AI_GATEWAY_API_KEY must fall back to VERCEL_OIDC_TOKEN",
    );
}

#[test]
fn vercel_preset_canonical_env_wins_over_oidc_alias() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("vercel".to_string()),
        "AI_GATEWAY_API_KEY" => Some("user-set".to_string()),
        "VERCEL_OIDC_TOKEN" => Some("eyJ.fake.oidc".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("vercel must map to OpenAiCompatible");
    };
    assert_eq!(compatible.api_key_env, "AI_GATEWAY_API_KEY");
}

#[test]
fn cloudflare_presets_honor_api_token_alias() {
    // Cloudflare's dashboard names the credential `CLOUDFLARE_API_TOKEN`
    // while squeezy historically reads `CLOUDFLARE_API_KEY`. Honor the
    // dashboard name as a fallback so operators don't have to rename a
    // CI secret on the way in.
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("cloudflare_workers_ai".to_string()),
        "CLOUDFLARE_ACCOUNT_ID" => Some("acct-abc".to_string()),
        "CLOUDFLARE_API_TOKEN" => Some("token-value".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("workers AI must map to OpenAiCompatible");
    };
    assert_eq!(compatible.api_key_env, "CLOUDFLARE_API_TOKEN");
}

#[test]
fn deepinfra_preset_honors_token_alias() {
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("deepinfra".to_string()),
        "DEEPINFRA_TOKEN" => Some("token-value".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("deepinfra must map to OpenAiCompatible");
    };
    assert_eq!(compatible.api_key_env, "DEEPINFRA_TOKEN");
}

#[test]
fn baseten_preset_carries_deployment_id_for_per_deployment_url() {
    // Baseten dedicated deployments live behind per-deployment hosts
    // (`https://model-{deployment_id}.api.baseten.co/...`). The
    // placeholder substitution lives in the LLM client; the config
    // builder's job is to surface the id without making users
    // downgrade to the `Custom` preset (which would lose
    // `BASETEN_API_KEY` autoload + the `baseten` registry label).
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "baseten".to_string(),
        ProviderSettings {
            deployment_id: Some("4qj0wr".to_string()),
            base_url: Some(
                "https://model-{deployment_id}.api.baseten.co/environments/production/sync/v1"
                    .to_string(),
            ),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("baseten"), |name| match name {
            "BASETEN_API_KEY" => Some("baseten-key".to_string()),
            _ => None,
        })
        .expect("baseten config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("baseten must map to OpenAiCompatible");
    };
    assert_eq!(compatible.preset, OpenAiCompatiblePreset::Baseten);
    assert_eq!(compatible.deployment_id.as_deref(), Some("4qj0wr"));
    assert!(
        compatible.base_url.contains("{deployment_id}"),
        "config layer keeps the placeholder template; substitution lives in the LLM client",
    );
}

#[test]
fn baseten_env_override_beats_toml_deployment_id() {
    // `BASETEN_DEPLOYMENT_ID` lets operators pivot between deployments
    // per-shell without editing committed config — common when shipping
    // a release-candidate alongside a baseline.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "baseten".to_string(),
        ProviderSettings {
            deployment_id: Some("from-toml".to_string()),
            ..Default::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..Default::default()
    };
    let config =
        AppConfig::try_from_settings_and_env_vars(settings, Some("baseten"), |name| match name {
            "BASETEN_API_KEY" => Some("baseten-key".to_string()),
            "BASETEN_DEPLOYMENT_ID" => Some("from-env".to_string()),
            _ => None,
        })
        .expect("baseten config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("baseten must map to OpenAiCompatible");
    };
    assert_eq!(compatible.deployment_id.as_deref(), Some("from-env"));
}

#[test]
fn non_baseten_presets_ignore_deployment_id_env() {
    // The env-var read is preset-scoped: a stray `BASETEN_DEPLOYMENT_ID`
    // in a shell that's also running an OpenRouter session must not
    // leak into the OpenRouter config.
    let config = AppConfig::from_env_vars(None, |name| match name {
        "SQUEEZY_PROVIDER" => Some("openrouter".to_string()),
        "OPENROUTER_API_KEY" => Some("or-key".to_string()),
        "BASETEN_DEPLOYMENT_ID" => Some("stray".to_string()),
        _ => None,
    });
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("openrouter must map to OpenAiCompatible");
    };
    assert!(
        compatible.deployment_id.is_none(),
        "the env-var read must be preset-scoped to Baseten",
    );
}

#[test]
fn cloudflare_workers_ai_preset_carries_account_id_and_placeholder_template() {
    // The Workers AI preset keeps the `{account_id}` placeholder in the
    // resolved `base_url` and flows `cloudflare_account_id` through as a
    // typed field on `OpenAiCompatibleConfig`. The substitution itself
    // lives in the LLM client (`substitute_url_placeholders` in
    // `squeezy-llm::compatible`) so a user override of `base_url` that
    // keeps the placeholder syntax — say, fronting the API through a
    // reverse proxy — gets the same treatment for free.
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
        compatible.base_url, "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
        "config layer must keep the placeholder template; substitution lives in the LLM client",
    );
    assert_eq!(
        compatible.account_id.as_deref(),
        Some("acct-abc"),
        "account_id must flow through to the provider config",
    );
    assert_eq!(compatible.gateway_id, None);
    // The eager helper still resolves the same URL, so any caller that
    // reads it directly (CLI inspect output, integration tests) stays
    // consistent with the LLM client's runtime substitution.
    assert_eq!(
        cloudflare_workers_ai_base_url("acct-abc"),
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
fn cloudflare_ai_gateway_preset_carries_account_and_gateway_ids_and_injects_dual_auth_header() {
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
        "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat",
        "config layer keeps the placeholder template; substitution lives in the LLM client",
    );
    assert_eq!(compatible.account_id.as_deref(), Some("acct-abc"));
    assert_eq!(compatible.gateway_id.as_deref(), Some("my-gateway"));
    assert_eq!(
        cloudflare_ai_gateway_base_url("acct-abc", "my-gateway"),
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
        "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat",
        "template stays templated; the LLM client substitutes at provider build time",
    );
    assert_eq!(compatible.account_id.as_deref(), Some("acct-abc"));
    assert_eq!(
        compatible.gateway_id.as_deref(),
        Some(DEFAULT_CLOUDFLARE_AI_GATEWAY_ID),
        "missing cloudflare_gateway_id must fall back to the `default` gateway slug",
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
fn cloudflare_ai_gateway_parses_typed_cf_aig_knobs() {
    // The typed surface lives in `[providers.cloudflare_ai_gateway.cf_ai_gateway]`
    // so users no longer have to paste raw `cf-aig-*` headers into
    // `headers` — caching, observability, and per-request cost
    // overrides each map to a named field that the LLM client
    // projects onto a `cf-aig-*` header at request time.
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.cloudflare_ai_gateway]
cloudflare_account_id = "acct-abc"

[providers.cloudflare_ai_gateway.cf_ai_gateway]
cache_ttl = 600
skip_cache = false
event_id = "evt_run_42"
step = "plan"
collect_log = true
skip_log = false
metadata = "{\"team\":\"sre\"}"
cache_key = "user-42:probe"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::try_from_settings_and_env_vars(
        settings,
        Some("cloudflare_ai_gateway"),
        |name| match name {
            "CLOUDFLARE_API_KEY" => Some("cf-key".to_string()),
            _ => None,
        },
    )
    .expect("AI Gateway config builds with typed knobs");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_ai_gateway must map to OpenAiCompatible");
    };
    let cf = compatible
        .cf_ai_gateway
        .as_ref()
        .expect("cf_ai_gateway block must round-trip");
    assert_eq!(cf.cache_ttl, Some(600));
    assert!(!cf.skip_cache);
    assert_eq!(cf.event_id.as_deref(), Some("evt_run_42"));
    assert_eq!(cf.step.as_deref(), Some("plan"));
    assert!(cf.collect_log);
    assert!(!cf.skip_log);
    assert_eq!(cf.metadata.as_deref(), Some("{\"team\":\"sre\"}"));
    assert_eq!(cf.cache_key.as_deref(), Some("user-42:probe"));
}

#[test]
fn cloudflare_ai_gateway_typed_knobs_default_to_none() {
    // Omitting the `[…cf_ai_gateway]` block leaves the typed surface
    // unset so the gateway's configured defaults stay in effect —
    // we never invent traffic the user didn't ask for.
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
    .expect("AI Gateway config builds");
    let ProviderConfig::OpenAiCompatible(compatible) = &config.provider else {
        panic!("cloudflare_ai_gateway must map to OpenAiCompatible");
    };
    assert!(compatible.cf_ai_gateway.is_none());
}

#[test]
fn non_cf_ai_gateway_presets_drop_cf_aig_knobs() {
    // A stray `[providers.workers_ai.cf_ai_gateway]` table on the
    // Workers AI preset (which talks to Cloudflare directly, no
    // gateway in front) must not flow through — those headers
    // would be silent-no-ops there.
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.cloudflare_workers_ai]
cloudflare_account_id = "acct-abc"

[providers.cloudflare_workers_ai.cf_ai_gateway]
cache_ttl = 600
"#,
        "test",
    )
    .expect("settings parse");
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
    assert!(
        compatible.cf_ai_gateway.is_none(),
        "cf_ai_gateway knob surface must only light up for the AI Gateway preset",
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

    let (settings, sources, warnings) = load_settings_from_paths(
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
    assert!(warnings.is_empty());

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

    let (settings, sources, warnings) = load_settings_from_paths(
        None,
        Some(project_path.as_path()),
        Some(repo_path.as_path()),
    )
    .expect("merge sources");

    assert_eq!(sources[0], "defaults");
    assert!(sources[1].starts_with("project:"));
    assert!(sources[2].starts_with("repo:"));
    assert!(warnings.is_empty());
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

    let (settings, sources, warnings) = load_settings_from_paths(
        Some(user_path.as_path()),
        Some(project_path.as_path()),
        Some(dir.join("repo_missing.toml").as_path()),
    )
    .expect("merge sources");

    assert_eq!(sources, vec!["defaults".to_string()]);
    assert!(warnings.is_empty());
    assert!(settings.providers.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_log_settings_parse_defaults_and_overrides() {
    let settings = SettingsFile::from_toml_str(
        r#"
[session]
mode = "plan"
resume_picker = "never"
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
    assert_eq!(config.session_resume_picker, SessionResumePicker::Never);
    assert_eq!(config.session_logs.log_dir, Some(".squeezy/history".into()));
    assert_eq!(config.session_logs.log_retention_days, 45);
    assert_eq!(config.session_logs.max_event_bytes, 1234);
    assert_eq!(config.session_logs.max_session_bytes, 5678);
}

#[test]
fn session_resume_picker_validation_reports_source_and_path() {
    let error = SettingsFile::from_toml_str(
        r#"
[session]
resume_picker = "sometimes"
"#,
        "squeezy.toml",
    )
    .expect_err("invalid resume picker should fail");

    let message = error.to_string();
    assert!(message.contains("squeezy.toml"));
    assert!(message.contains("session.resume_picker"));
    assert!(message.contains("expected ask or never"));
}

#[test]
fn session_log_dir_env_var_overrides_settings_file_value() {
    let settings = SettingsFile::from_toml_str(
        r#"
[session]
log_dir = ".squeezy/from-config"
"#,
        "test",
    )
    .expect("parse settings");

    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_SESSION_DIR" => Some("/tmp/from-env/sessions".to_string()),
        _ => None,
    });

    assert_eq!(
        config.session_logs.log_dir,
        Some(PathBuf::from("/tmp/from-env/sessions"))
    );
    assert!(
        config.config_sources.iter().any(|source| source == "env"),
        "env source should be tagged when SQUEEZY_SESSION_DIR is consumed; got {:?}",
        config.config_sources,
    );
}

#[test]
fn session_log_dir_env_var_resolves_when_settings_absent() {
    let config =
        AppConfig::from_settings_and_env_vars(SettingsFile::default(), |name| match name {
            "SQUEEZY_SESSION_DIR" => Some("  /var/log/squeezy  ".to_string()),
            _ => None,
        });

    // Whitespace is trimmed so shell pipelines that append `\n` (e.g.
    // `$(printf '%s\n' "$dir")`) don't end up writing into a literal
    // ".../squeezy\n/" directory.
    assert_eq!(
        config.session_logs.log_dir,
        Some(PathBuf::from("/var/log/squeezy"))
    );
}

#[test]
fn session_log_dir_env_var_is_ignored_when_blank() {
    let settings = SettingsFile::from_toml_str(
        r#"
[session]
log_dir = ".squeezy/from-config"
"#,
        "test",
    )
    .expect("parse settings");

    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        // A user can unset the var via `unset SQUEEZY_SESSION_DIR`, but they
        // can also accidentally `export SQUEEZY_SESSION_DIR=` and expect the
        // settings.toml value to remain in force.
        "SQUEEZY_SESSION_DIR" => Some("   ".to_string()),
        _ => None,
    });

    assert_eq!(
        config.session_logs.log_dir,
        Some(PathBuf::from(".squeezy/from-config"))
    );
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
fn tui_theme_names_normalize_aliases_and_custom_slugs() {
    assert_eq!(
        normalize_tui_theme_name("system").as_deref(),
        Some("default")
    );
    assert_eq!(normalize_tui_theme_name("dark").as_deref(), Some("default"));
    assert_eq!(normalize_tui_theme_name("light").as_deref(), Some("bright"));
    assert_eq!(normalize_tui_theme_name("auto").as_deref(), Some("default"));
    assert_eq!(
        normalize_tui_theme_name("catppuccin").as_deref(),
        Some("catppuccin")
    );
    assert_eq!(
        normalize_tui_theme_name("mauve").as_deref(),
        Some("catppuccin")
    );
    assert_eq!(
        normalize_tui_theme_name("high-contrast").as_deref(),
        Some("high-contrast"),
    );
    assert_eq!(
        normalize_tui_theme_name("high_contrast").as_deref(),
        Some("high-contrast"),
    );
    assert_eq!(
        normalize_tui_theme_name("hc").as_deref(),
        Some("high-contrast")
    );
    assert_eq!(
        normalize_tui_theme_name("  Dark  ").as_deref(),
        Some("default")
    );
    assert_eq!(normalize_tui_theme_name("LIGHT").as_deref(), Some("bright"));
    assert_eq!(
        normalize_tui_theme_name("solarized").as_deref(),
        Some("solarized")
    );
    assert_eq!(
        normalize_tui_theme_name("my_theme").as_deref(),
        Some("my-theme")
    );
    assert_eq!(normalize_tui_theme_name(""), None);
    assert_eq!(normalize_tui_theme_name("bad theme!"), None);
}

#[test]
fn tui_theme_round_trips_through_settings_toml() {
    let parsed = SettingsFile::from_toml_str(
        r#"
[tui]
theme = "solarized"

[tui.themes.solarized.colors]
palette.accent = [1, 2, 3]
ui.foreground = [250, 251, 252]
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(parsed, |_| None);
    assert_eq!(config.tui.theme, "solarized");
    assert_eq!(
        config
            .tui
            .themes
            .get("solarized")
            .and_then(|theme| theme.colors.get("palette.accent"))
            .copied(),
        Some([1, 2, 3])
    );
    assert_eq!(
        config
            .tui
            .themes
            .get("solarized")
            .and_then(|theme| theme.colors.get("ui.foreground"))
            .copied(),
        Some([250, 251, 252])
    );

    // Emit and re-parse to confirm the writer persists the field.
    let emitted = config.inspect_redacted();
    assert!(
        emitted.contains("theme = \"solarized\""),
        "inspect should emit the theme leaf, got: {emitted}"
    );
    assert!(
        emitted.contains("\"palette.accent\" = [1, 2, 3]"),
        "inspect should emit theme color overrides, got: {emitted}"
    );
    let reparsed = SettingsFile::from_toml_str(&emitted, "round trip").expect("inspect re-parse");
    let reloaded = AppConfig::from_settings_and_env_vars(reparsed, |_| None);
    assert_eq!(reloaded.tui.theme, "solarized");
    assert_eq!(
        reloaded
            .tui
            .themes
            .get("solarized")
            .and_then(|theme| theme.colors.get("palette.accent"))
            .copied(),
        Some([1, 2, 3])
    );
}

#[test]
fn tui_theme_defaults_to_default_when_unset() {
    let parsed =
        SettingsFile::from_toml_str("[tui]\ntick_rate_ms = 50\n", "test").expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(parsed, |_| None);
    assert_eq!(config.tui.theme, DEFAULT_TUI_THEME_NAME);
}

#[test]
fn tui_theme_rejects_invalid_name() {
    let result = SettingsFile::from_toml_str(
        r#"
[tui]
theme = "bad theme!"
"#,
        "test",
    );
    let err = result.expect_err("invalid theme should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid TUI theme") || msg.contains("bad theme"),
        "expected invalid-theme diagnostic, got: {msg}"
    );
}

#[test]
fn unknown_fields_are_ignored_warned_and_preserved_in_settings_file() {
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

    let (settings, sources, warnings) =
        SettingsFile::load_optional_source(&path, "test").expect("load_optional_source");

    assert_eq!(
        settings.tui.as_ref().and_then(|tui| tui.tick_rate_ms),
        Some(100)
    );
    assert_eq!(
        sources,
        vec!["defaults".to_string(), format!("test:{}", path.display())]
    );
    assert_eq!(
        warnings,
        vec![ConfigWarning {
            source: format!("test:{}", path.display()),
            field: "tui.legacy_widget_padding".to_string(),
        }]
    );

    let cleaned = std::fs::read_to_string(&path).expect("read settings");
    assert!(
        cleaned.contains("legacy_widget_padding"),
        "unknown key should be preserved while ignored, got: {cleaned}"
    );
    assert!(
        cleaned.contains("tick_rate_ms = 100"),
        "known key should be preserved, got: {cleaned}"
    );

    let config = AppConfig::try_from_settings_and_env_vars_with_sources_and_warnings(
        settings,
        sources,
        warnings.clone(),
        None,
        |_| None,
    )
    .expect("config loads with unknown field warnings");
    assert_eq!(config.config_warnings, warnings);

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
fn extra_headers_values_are_redacted_on_serde_serialize() {
    // M-63: the Custom-preset workaround for non-Bearer auth is to
    // smuggle the secret through `[providers.<section>.headers]`
    // (LiteLLM `x-litellm-key`, PortKey `x-portkey-api-key`, vLLM
    // bearer, corporate `x-api-key` / `api-key`). Without redaction
    // those values flow verbatim through any code path that calls
    // serde Serialize on a `ProviderSettings`, so a panic envelope or
    // bug-report dump leaks the user's credential. Pin the contract:
    // the *key* of each header stays visible (so an operator can see
    // which slots are wired) and the *value* is masked to
    // `"<redacted>"`. Cover three common shapes — an OpenRouter
    // attribution header (not actually secret but treated uniformly),
    // a LiteLLM virtual-key header, and a PortKey virtual-key header.
    let mut headers = BTreeMap::new();
    headers.insert(
        "x-litellm-key".to_string(),
        "lk-litellm-do-not-leak".to_string(),
    );
    headers.insert(
        "x-portkey-api-key".to_string(),
        "pk-portkey-do-not-leak".to_string(),
    );
    headers.insert(
        "HTTP-Referer".to_string(),
        "https://example.com".to_string(),
    );
    let settings = ProviderSettings {
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        headers: Some(headers),
        ..ProviderSettings::default()
    };
    let emitted = toml::to_string(&settings).expect("serialize");
    for plaintext in [
        "lk-litellm-do-not-leak",
        "pk-portkey-do-not-leak",
        "https://example.com",
    ] {
        assert!(
            !emitted.contains(plaintext),
            "serialize must not leak header value {plaintext:?}; got: {emitted}",
        );
    }
    // Header *names* stay visible so operators can audit which slots
    // are wired. Without this assertion a future regression that
    // dropped the whole `headers` table on serialize would slip past.
    for header_name in ["x-litellm-key", "x-portkey-api-key", "HTTP-Referer"] {
        assert!(
            emitted.contains(header_name),
            "serialize must keep header name {header_name:?} visible; got: {emitted}",
        );
    }
    assert!(
        emitted.contains("<redacted>"),
        "serialize must emit the redaction marker; got: {emitted}"
    );
}

#[test]
fn extra_headers_none_serializes_without_a_headers_table() {
    // M-63 must preserve the `None` distinction: a provider with no
    // `[providers.<section>.headers]` block should serialize without
    // any `headers` key at all, so the round-trip back through TOML
    // keeps the unset state. (`toml::to_string` skips `Option::None`
    // fields by default, and the redactor must not accidentally upgrade
    // them to `Some(empty_map)`.)
    let settings = ProviderSettings {
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        headers: None,
        ..ProviderSettings::default()
    };
    let emitted = toml::to_string(&settings).expect("serialize");
    assert!(
        !emitted.contains("headers"),
        "None headers must not emit a [headers] table; got: {emitted}"
    );
}

#[test]
fn custom_preset_emits_allow_list_warning_at_config_load() {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    // M-64 contract: when the `Custom` OpenAI-compatible preset is
    // configured we emit exactly one `WARN` on
    // `squeezy_core::config` carrying the resolved `base_url`. The
    // warning is intentionally non-blocking — an operator who imports
    // a project-local `./squeezy.toml` from an untrusted source needs
    // to *see* the destination of every Bearer-token-carrying request
    // before traffic flows. Curated presets (OpenAI proper, Anthropic,
    // Cloudflare AI Gateway, …) all run against published default
    // base_urls so the same warning would be noise; only Custom takes
    // a fully user-controlled URL and bypasses every other check.
    #[derive(Default, Clone)]
    struct Capturing {
        events: Arc<Mutex<Vec<(String, String)>>>,
    }
    struct MsgVisitor<'a>(&'a mut String);
    impl Visit for MsgVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.0.push_str(value);
            }
        }
    }
    impl Subscriber for Capturing {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= &Level::WARN
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let target = event.metadata().target().to_string();
            let mut message = String::new();
            event.record(&mut MsgVisitor(&mut message));
            self.events
                .lock()
                .expect("events lock poisoned")
                .push((target, message));
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    let subscriber = Capturing::default();
    let toml = r#"
[model]
provider = "openai_compatible"

[providers.openai_compatible]
base_url = "https://internal.example.com/v1"
api_key_env = "FAKE_KEY"
"#;
    let settings = SettingsFile::from_toml_str(toml, "test").expect("settings parse");
    let config = tracing::subscriber::with_default(subscriber.clone(), || {
        AppConfig::try_from_settings_and_env_vars(settings, None, |name| {
            (name == "FAKE_KEY").then(|| "k".to_string())
        })
    })
    .expect("Custom preset must build with explicit base_url + api_key_env");

    // M-64: besides the tracing WARN, the same advisory must surface as a
    // structured `ConfigWarning` (mirroring the M-58 Cerebras path) so it
    // reaches the operator even when tracing is unconfigured.
    assert!(
        config.config_warnings.iter().any(|w| {
            w.source == "providers.custom"
                && w.field.contains("https://internal.example.com/v1")
                && w.field.contains("bypasses")
        }),
        "Custom preset must push a structured ConfigWarning naming the base_url; \
         got: {:?}",
        config.config_warnings
    );

    let captured: Vec<(String, String)> =
        std::mem::take(&mut *subscriber.events.lock().expect("events lock poisoned"));
    let matches: Vec<_> = captured
        .iter()
        .filter(|(target, message)| {
            target == "squeezy_core::config" && message.contains("Custom preset bypasses")
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one Custom-preset allow-list warning on \
         squeezy_core::config; captured: {captured:?}"
    );
    let (_, message) = matches[0];
    assert!(
        message.contains("https://internal.example.com/v1"),
        "warning must name the resolved base_url; got: {message}"
    );
}

#[test]
fn extra_headers_reject_crlf_at_config_load() {
    // M-65: a header value containing CR/LF is request-smuggling
    // shrapnel — http::HeaderValue refuses anything outside
    // [0x20..0x7E] ∪ {0x09} at request-construction time, but the
    // failure mode there is a deferred reqwest builder error with no
    // field name, surfacing mid-stream as a confusing "invalid HTTP
    // header value". Pin the contract that config-load catches the
    // offending TOML path up-front and surfaces a usable hint. Cover
    // the canonical request-smuggling shape (`\r\n` + spliced Host
    // header) and the lone-`\n` flavor that bypasses sloppy CR/LF
    // string-search filters.
    for (literal, label) in [
        (r#""value\r\nHost: attacker.example""#, "x-evil-crlf"),
        (r#""value\nX-Smuggled: true""#, "x-evil-lf"),
        (r#""value\rsplit""#, "x-evil-cr"),
    ] {
        let toml = format!(
            r#"
[model]
provider = "openai_compatible"

[providers.openai_compatible]
base_url = "https://api.example.com/v1"
api_key_env = "FAKE_KEY"

[providers.openai_compatible.headers]
"{label}" = {literal}
"#
        );
        // ProviderSettings::from_table runs as part of from_toml_str, so
        // the CR/LF rejection surfaces here — it would otherwise be a
        // deferred reqwest builder error at request time with no field
        // name. Pin both the field path and the CR/LF hint so a future
        // refactor that moved this check elsewhere or stripped the hint
        // would fail the assertion.
        let err = SettingsFile::from_toml_str(&toml, "test").unwrap_err();
        assert!(
            matches!(err, SqueezyError::Config(_)),
            "{label}: CR/LF rejection must surface as SqueezyError::Config, got: {err:?}",
        );
        let message = err.to_string();
        let expected_path = format!("providers.openai_compatible.headers.{label}");
        assert!(
            message.contains(&expected_path),
            "{label}: error must name the offending TOML path {expected_path:?}; got: {message}",
        );
        assert!(
            message.contains("CR/LF") || message.contains("control characters"),
            "{label}: error must hint at the CR/LF restriction; got: {message}",
        );
    }
}

#[test]
fn extra_headers_accept_visible_ascii_at_config_load() {
    // Counterpart to `extra_headers_reject_crlf_at_config_load`: the
    // common-case `HTTP-Referer` / `X-Title` / `cf-aig-authorization`
    // shapes used in actual production deployments must still parse
    // cleanly. Without this assertion a regression that tightened the
    // filter too aggressively (e.g. rejecting whitespace, parens, or
    // forward slashes) would shut out the OpenRouter attribution and
    // Cloudflare AI Gateway dual-auth setups documented in the README.
    let toml = r#"
[model]
provider = "openai_compatible"

[providers.openai_compatible]
base_url = "https://api.example.com/v1"
api_key_env = "FAKE_KEY"

[providers.openai_compatible.headers]
HTTP-Referer = "https://github.com/esqueezy/squeezy"
X-Title = "Squeezy (1.2.3)"
cf-aig-authorization = "Bearer sk-test-1234"
"#;
    let settings = SettingsFile::from_toml_str(toml, "test").expect("settings parse");
    AppConfig::try_from_settings_and_env_vars(settings, None, |name| {
        (name == "FAKE_KEY").then(|| "k".to_string())
    })
    .expect("standard ASCII header values must round-trip cleanly");
}

#[test]
fn non_custom_preset_does_not_emit_allow_list_warning() {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    // Counterpart to `custom_preset_emits_allow_list_warning_at_config_load`:
    // a curated preset (OpenRouter here — it has a published default
    // base_url) must NOT emit the Custom-preset bypass warning. Without
    // this control a future regression that fired the warning for
    // every OpenAI-compatible preset would slip past the positive
    // assertion above.
    #[derive(Default, Clone)]
    struct Capturing {
        events: Arc<Mutex<Vec<String>>>,
    }
    struct MsgVisitor<'a>(&'a mut String);
    impl Visit for MsgVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.0.push_str(value);
            }
        }
    }
    impl Subscriber for Capturing {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= &Level::WARN && metadata.target() == "squeezy_core::config"
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut message = String::new();
            event.record(&mut MsgVisitor(&mut message));
            self.events
                .lock()
                .expect("events lock poisoned")
                .push(message);
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    let subscriber = Capturing::default();
    let toml = r#"
[model]
provider = "openrouter"

[providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"
"#;
    let settings = SettingsFile::from_toml_str(toml, "test").expect("settings parse");
    let _config = tracing::subscriber::with_default(subscriber.clone(), || {
        AppConfig::try_from_settings_and_env_vars(settings, None, |name| {
            (name == "OPENROUTER_API_KEY").then(|| "k".to_string())
        })
    })
    .expect("OpenRouter preset must build");

    let captured: Vec<String> =
        std::mem::take(&mut *subscriber.events.lock().expect("events lock poisoned"));
    for message in &captured {
        assert!(
            !message.contains("Custom preset bypasses"),
            "non-Custom preset must not emit the allow-list warning; got: {message}",
        );
    }
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
        body.contains("A separate `memory_append` tool name"),
        "scope doc must record memory_append as out of scope"
    );
    assert!(
        body.contains("notes_remember"),
        "scope doc must describe the implemented store-backed observation tools"
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
    assert_ne!(DEFAULT_BEDROCK_MODEL, BEDROCK_SMALL_FAST_MODEL);
    assert_eq!(
        small_fast_model_for_provider("azure_openai"),
        Some(AZURE_OPENAI_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("openrouter"),
        Some(OPENROUTER_SMALL_FAST_MODEL)
    );
    assert_eq!(
        small_fast_model_for_provider("vertex"),
        Some(VERTEX_SMALL_FAST_MODEL)
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
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |_| None);
    assert_eq!(
        config.small_fast_model.as_deref(),
        Some("claude-haiku-from-toml")
    );
}

#[test]
fn routing_judge_model_reads_toml_and_env_override() {
    let settings = SettingsFile::from_toml_str(
        r#"
[routing]
judge_model = "sonnet"
"#,
        "test",
    )
    .expect("settings parse");
    let config = AppConfig::from_settings_and_env_vars(settings, |name| match name {
        "SQUEEZY_ROUTING_JUDGE_MODEL" => Some("haiku".to_string()),
        _ => None,
    });

    assert_eq!(config.routing.judge_model.as_deref(), Some("haiku"));
}

#[test]
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

/// T-61: extending the SSRF block-list past the original http-only filter
/// to cover IMDS / metadata / link-local hosts regardless of scheme.
/// `https://169.254.169.254/...` previously sailed straight through into
/// the Bearer-token request path because the helper only inspected the
/// http scheme; verify each sentinel host is now refused at config-load
/// for both http and https.
#[test]
fn config_rejects_imds_and_metadata_hosts_regardless_of_scheme() {
    let cases: &[(&str, &str)] = &[
        ("https://169.254.169.254/latest/api/token", "169.254"),
        ("http://169.254.169.254/latest/", "169.254"),
        // AWS ECS task-IAM endpoint.
        ("https://169.254.170.2/v2/credentials/", "169.254"),
        // GCP metadata sentinel hostname.
        (
            "https://metadata.google.internal/computeMetadata/v1/",
            "metadata.google.internal",
        ),
        // IPv4 link-local outside the 169.254.169.254 sentinel.
        ("https://169.254.1.1/v1", "169.254"),
        // IPv6 link-local (fe80::/10).
        ("https://[fe80::1]/v1", "fe80::1"),
        // AWS IPv6 IMDS ULA address.
        ("https://[fd00:ec2::254]/latest/", "fd00:ec2::254"),
        // F1: any `fc00::/7` IPv6 unique-local address (ULA), not just the
        // AWS IMDS literal. `fd12:3456:789a:1::1` and the `fc00::` low half
        // of the range both name internal-only endpoints and must be refused.
        ("https://[fd12:3456:789a:1::1]/v1", "fd12"),
        ("https://[fc00::1]/v1", "fc00"),
        // IPv4-mapped IPv6 form of the IMDS sentinel — a standard SSRF
        // evasion. Must be canonicalized to 169.254.169.254 and rejected.
        ("https://[::ffff:169.254.169.254]/latest/", "169.254"),
    ];
    for (url, host_fragment) in cases {
        let toml = format!(
            r#"
[model]
provider = "openai_compatible"

[providers.openai_compatible]
base_url = "{url}"
api_key_env = "FAKE_KEY"
"#
        );
        let settings = SettingsFile::from_toml_str(&toml, "test").expect("settings parse");
        let error = AppConfig::try_from_settings_and_env_vars(settings, None, |name| {
            (name == "FAKE_KEY").then(|| "k".to_string())
        })
        .expect_err(&format!("must reject {url}"));
        let msg = error.to_string();
        assert!(
            msg.contains("cloud-metadata or") || msg.contains("link-local"),
            "error for {url} must mention metadata/link-local: {msg}"
        );
        assert!(
            msg.contains(host_fragment),
            "error for {url} must contain host fragment {host_fragment:?}: {msg}"
        );
    }
}

/// Companion to the metadata-host filter: bare loopback configurations
/// must still be accepted on both `http://` and `https://` so local
/// `lmstudio`, `ollama`, and `llamacpp` deployments keep working.
#[test]
fn config_accepts_loopback_hosts_on_https_too() {
    for url in [
        "https://127.0.0.1:11434/api",
        "https://localhost:8000/v1",
        "https://[::1]:8443/v1",
    ] {
        let toml = format!(
            r#"
[model]
provider = "openai_compatible"

[providers.openai_compatible]
base_url = "{url}"
api_key_env = "FAKE_KEY"
"#
        );
        let settings = SettingsFile::from_toml_str(&toml, "test").expect("settings parse");
        AppConfig::try_from_settings_and_env_vars(settings, None, |name| {
            (name == "FAKE_KEY").then(|| "k".to_string())
        })
        .unwrap_or_else(|err| panic!("loopback https {url:?} must be accepted: {err}"));
    }
}

#[cfg(unix)]
#[test]
fn shell_escape_resolves_string_value_to_stdout() {
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "!echo hello"
"#,
        "test",
    )
    .expect("settings parse");
    let providers = settings.providers.expect("providers map");
    let openai = providers.get("openai").expect("openai provider");
    assert_eq!(openai.api_key.as_deref(), Some("hello"));
}

#[cfg(unix)]
#[test]
fn shell_escape_trims_only_trailing_whitespace() {
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "!printf '  spaced-secret  \n\n'"
"#,
        "test",
    )
    .expect("settings parse");
    let providers = settings.providers.expect("providers map");
    let openai = providers.get("openai").expect("openai provider");
    assert_eq!(openai.api_key.as_deref(), Some("  spaced-secret"));
}

#[cfg(unix)]
#[test]
fn shell_escape_failure_aborts_config_load_with_clear_error() {
    let err = SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "!squeezy_definitely_not_a_real_command_f07 2>/dev/null"
"#,
        "test",
    )
    .expect_err("non-zero exit must fail config load");
    let message = err.to_string();
    assert!(
        message.contains("providers.openai.api_key"),
        "error mentions key path: {message}"
    );
    assert!(
        message.contains("shell escape"),
        "error labels the failure: {message}"
    );
    assert!(
        message.contains("squeezy_definitely_not_a_real_command_f07"),
        "error includes the failing command: {message}"
    );
}

#[test]
fn shell_escape_does_not_trigger_when_bang_is_not_leading() {
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "value with ! in it"
"#,
        "test",
    )
    .expect("settings parse");
    let providers = settings.providers.expect("providers map");
    let openai = providers.get("openai").expect("openai provider");
    assert_eq!(openai.api_key.as_deref(), Some("value with ! in it"));
}

#[test]
fn shell_escape_rejects_empty_command() {
    let err = SettingsFile::from_toml_str(
        r#"
[providers.openai]
api_key = "!"
"#,
        "test",
    )
    .expect_err("empty `!` must fail config load");
    let message = err.to_string();
    assert!(
        message.contains("providers.openai.api_key"),
        "error mentions key path: {message}"
    );
    assert!(
        message.contains("empty"),
        "error labels the empty command: {message}"
    );
}

#[cfg(unix)]
#[test]
fn shell_escape_applies_inside_string_array_values() {
    // `graph.languages` is a plain `Option<Vec<String>>` read via
    // `string_array_value`, so it exercises the per-element resolver path.
    let settings = SettingsFile::from_toml_str(
        r#"
[graph]
languages = ["!echo rust", "python"]
"#,
        "test",
    )
    .expect("settings parse");
    let graph = settings.graph.expect("graph table");
    let langs = graph.languages.expect("languages array");
    assert_eq!(langs, vec!["rust".to_string(), "python".to_string()]);
}

#[cfg(unix)]
#[test]
fn shell_escape_applies_inside_provider_headers_map() {
    let settings = SettingsFile::from_toml_str(
        r#"
[providers.openai]
[providers.openai.headers]
"x-static" = "literal"
"x-dynamic" = "!echo dynamic-value"
"#,
        "test",
    )
    .expect("settings parse");
    let providers = settings.providers.expect("providers map");
    let openai = providers.get("openai").expect("openai provider");
    let headers = openai.headers.as_ref().expect("headers map");
    assert_eq!(headers.get("x-static").map(String::as_str), Some("literal"));
    assert_eq!(
        headers.get("x-dynamic").map(String::as_str),
        Some("dynamic-value")
    );
}

#[test]
fn merge_cost_snapshot_aggregates_reasoning_output_tokens() {
    let mut total = CostSnapshot {
        reasoning_output_tokens: Some(7),
        ..CostSnapshot::default()
    };
    let next = CostSnapshot {
        reasoning_output_tokens: Some(11),
        ..CostSnapshot::default()
    };
    merge_cost_snapshot(&mut total, &next);
    assert_eq!(total.reasoning_output_tokens, Some(18));
}
