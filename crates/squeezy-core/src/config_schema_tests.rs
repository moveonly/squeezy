use super::*;

#[test]
fn every_section_has_at_least_one_field() {
    for section in CONFIG_SECTIONS {
        // `Reset` is a synthetic action-only section — the TUI renders
        // tier-delete rows instead of `FieldMeta` entries. `Themes`
        // and `McpServers` likewise render their own row layout (theme
        // palette / per-server status grid) and route writes through
        // dedicated TUI handlers, so they intentionally carry no
        // `FieldMeta` entries here.
        if matches!(
            section.id,
            SectionId::Reset | SectionId::Themes | SectionId::McpServers
        ) {
            continue;
        }
        assert!(
            !section.fields.is_empty(),
            "section {} has no fields",
            section.label
        );
    }
}

#[test]
fn section_lookup_is_consistent() {
    for s in CONFIG_SECTIONS {
        assert_eq!(section(s.id).map(|m| m.id), Some(s.id));
        assert_eq!(section_from_slug(s.id.slug()), Some(s.id));
    }
}

#[test]
fn absent_field_values_render_as_dash() {
    assert_eq!(FieldValue::OptionalInteger(None).as_display(), "—");
    assert_eq!(FieldValue::OptionalFloat(None).as_display(), "—");
    assert_eq!(FieldValue::OptionalEnum(None).as_display(), "—");
    assert_eq!(FieldValue::String(String::new()).as_display(), "—");
    assert_eq!(FieldValue::StringList(Vec::new()).as_display(), "—");
    assert_eq!(FieldValue::Unset.as_display(), "—");
}

#[test]
fn context_trigger_info_reflects_thresholds_and_toggles() {
    let ctx = section(SectionId::Context).expect("Context section must be registered");
    let triggers = ctx
        .fields
        .iter()
        .find(|field| field.label == "triggers")
        .expect("Context section exposes trigger summary");

    // Default config has no model window, so the summary marks the window as a
    // fallback and lists each tier's resolved firing point.
    let default_summary = (triggers.get)(&AppConfig::default()).as_display();
    assert!(default_summary.contains("(fallback)"), "{default_summary}");
    assert!(default_summary.contains("trim @"), "{default_summary}");
    assert!(default_summary.contains("warn @"), "{default_summary}");
    assert!(default_summary.contains("summarize @"), "{default_summary}");
    assert!(!default_summary.contains("trim off"), "{default_summary}");
    assert!(
        !default_summary.contains("summarize off"),
        "{default_summary}"
    );

    // Toggling the tiers off is reflected distinctly.
    let mut disabled = AppConfig::default();
    disabled.context_compaction.micro_compaction_enabled = false;
    disabled.context_compaction.enabled = false;
    let disabled_summary = (triggers.get)(&disabled).as_display();
    assert!(disabled_summary.contains("trim off"), "{disabled_summary}");
    assert!(
        disabled_summary.contains("summarize off"),
        "{disabled_summary}"
    );
}

#[test]
fn provider_setter_swaps_default_model() {
    let mut cfg = AppConfig::from_env();
    let original = cfg.model.clone();
    (CONFIG_SECTIONS[0].fields[0].set)(&mut cfg, FieldValue::Enum("anthropic")).unwrap();
    assert_eq!(cfg.model, DEFAULT_ANTHROPIC_MODEL);
    // and back
    (CONFIG_SECTIONS[0].fields[0].set)(&mut cfg, FieldValue::Enum("openai")).unwrap();
    assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
    let _ = original;
}

#[test]
fn permission_round_trip() {
    let mut cfg = AppConfig::from_env();
    let perms = section(SectionId::Permissions).unwrap();
    for f in perms.fields {
        match f.kind {
            FieldKind::Enum { options } => {
                for option in options {
                    (f.set)(&mut cfg, FieldValue::Enum(option)).unwrap();
                    match (f.get)(&cfg) {
                        FieldValue::Enum(v) => assert_eq!(v, *option, "{}", f.label),
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            }
            // The Auto-review reviewer rows (reviewer_model, reviewer_policy,
            // reviewer_policy_extra): an explicit value round-trips, and
            // clearing the override succeeds. (reviewer_model resolves to the
            // provider's small/fast model when cleared, so the cleared value is
            // not necessarily empty — assert only that clearing is accepted.)
            FieldKind::String { .. } => {
                (f.set)(&mut cfg, FieldValue::String("custom-value".to_string())).unwrap();
                match (f.get)(&cfg) {
                    FieldValue::String(v) => assert_eq!(v, "custom-value", "{}", f.label),
                    other => panic!("unexpected: {other:?}"),
                }
                (f.set)(&mut cfg, FieldValue::String(String::new())).unwrap();
                match (f.get)(&cfg) {
                    FieldValue::String(_) => {}
                    other => panic!("unexpected: {other:?}"),
                }
            }
            // reviewer_capabilities: a list of capability names round-trips.
            FieldKind::StringList { .. } => {
                (f.set)(
                    &mut cfg,
                    FieldValue::StringList(vec!["read".to_string(), "edit".to_string()]),
                )
                .unwrap();
                match (f.get)(&cfg) {
                    FieldValue::StringList(v) => {
                        assert_eq!(
                            v,
                            vec!["read".to_string(), "edit".to_string()],
                            "{}",
                            f.label
                        )
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            }
            other => panic!(
                "unexpected permission field kind for {}: {other:?}",
                f.label
            ),
        }
    }
}

#[test]
fn permission_mode_default_matches_shipped_runtime_default() {
    // The schema's inherited-default + Ctrl+R reset target must agree with the
    // engine default (Default — reviewer off). Otherwise the screen shows a wrong
    // "(default)" badge and reset silently disables the reviewer.
    let mode = section(SectionId::Permissions)
        .unwrap()
        .fields
        .iter()
        .find(|f| f.label == "mode")
        .unwrap();
    assert_eq!((mode.default)(), FieldValue::Enum("default"));
    assert_eq!(mode.default_display, "default");
    assert_eq!(
        crate::PermissionPolicy::default().mode,
        crate::PermissionPolicyMode::Default
    );
}

#[test]
fn permission_reviewer_rows_follow_mode() {
    // The TUI config screen relies on the reviewer rows sitting immediately
    // after `mode` so the visible Permissions rows stay a contiguous prefix
    // (see `permissions_visible_rows`).
    let perms = section(SectionId::Permissions).unwrap();
    assert_eq!(perms.fields[0].label, "mode");
    assert_eq!(perms.fields[1].label, "reviewer_model");
    assert_eq!(perms.fields[2].label, "reviewer_policy");
    assert_eq!(perms.fields[3].label, "reviewer_policy_extra");
    assert_eq!(perms.fields[4].label, "reviewer_capabilities");
}

// ─── Subagent schema consistency ──────────────────────────────────────────────

#[test]
fn subagent_max_tool_calls_schema_max_covers_runtime_default() {
    // Previously the UI field had max=256, which is below the runtime default
    // of 10_000. A "reset to default" from the TUI would silently clamp the
    // value to 256, giving the user a misleading cap. This test locks the
    // invariant: the schema max must be >= the runtime default so no valid
    // default can exceed the editable range.
    let field = section(SectionId::Subagents)
        .unwrap()
        .fields
        .iter()
        .find(|f| f.label == "max_tool_calls_per_call")
        .expect("max_tool_calls_per_call field must exist in Subagents section");
    let schema_max = match field.kind {
        FieldKind::Integer { max, .. } => max,
        other => panic!("expected Integer, got {other:?}"),
    };
    let runtime_default = crate::DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL as i64;
    assert!(
        schema_max >= runtime_default,
        "schema max ({schema_max}) is below the runtime default ({runtime_default}); \
         a TUI reset would silently clamp the value"
    );
}

#[test]
fn subagent_integer_schema_defaults_match_runtime_constants() {
    // Each `default_display` string in the Subagents section is shown in the
    // TUI next to the field label and used as the Ctrl+R reset target. It must
    // match the actual runtime default so users get accurate information.
    struct Case {
        label: &'static str,
        runtime_default: i64,
    }
    let cases = [
        Case {
            label: "max_concurrent",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_CONCURRENT as i64,
        },
        Case {
            label: "max_tool_calls_per_call",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL as i64,
        },
        Case {
            label: "max_tool_bytes_read_per_call",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL as i64,
        },
        Case {
            label: "max_search_files_per_call",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL as i64,
        },
        Case {
            label: "max_model_rounds",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS as i64,
        },
        Case {
            label: "max_summary_tokens",
            runtime_default: crate::DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS as i64,
        },
    ];
    let subagents = section(SectionId::Subagents).unwrap();
    for case in &cases {
        let field = subagents
            .fields
            .iter()
            .find(|f| f.label == case.label)
            .unwrap_or_else(|| panic!("field '{}' not found in Subagents section", case.label));
        // The `default` closure must return the right value.
        let default_value = match (field.default)() {
            FieldValue::Integer(v) => v,
            other => panic!("field '{}' default is not Integer: {other:?}", case.label),
        };
        assert_eq!(
            default_value, case.runtime_default,
            "field '{}' default() value ({default_value}) != runtime constant ({})",
            case.label, case.runtime_default
        );
        // The `default_display` string must also agree.
        let display_val: i64 = field
            .default_display
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .unwrap_or_else(|_| {
                panic!(
                    "field '{}' default_display '{}' does not start with a parseable integer",
                    case.label, field.default_display
                )
            });
        assert_eq!(
            display_val, case.runtime_default,
            "field '{}' default_display '{}' does not match runtime constant ({})",
            case.label, field.default_display, case.runtime_default
        );
    }
}

#[test]
fn context_section_integer_defaults_match_runtime_constants() {
    let cases = [
        (
            "fallback_window_tokens",
            crate::DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS as i64,
        ),
        (
            "compaction_min_items",
            crate::DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS as i64,
        ),
        (
            "compaction_recent_items",
            crate::DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS as i64,
        ),
        (
            "compaction_max_summary_bytes",
            crate::DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES as i64,
        ),
        (
            "warn_at_percent",
            crate::DEFAULT_CONTEXT_WARN_AT_PERCENT as i64,
        ),
        (
            "trim_at_percent",
            crate::DEFAULT_CONTEXT_TRIM_AT_PERCENT as i64,
        ),
        (
            "micro_compaction_keep_recent",
            crate::DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT as i64,
        ),
        (
            "model_assisted_max_output_tokens",
            crate::DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS as i64,
        ),
        (
            "model_assisted_timeout_secs",
            crate::DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS as i64,
        ),
        (
            "layered_fallback_extractive_threshold_tokens",
            crate::DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS as i64,
        ),
        (
            "repo_doc_max_bytes",
            crate::DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES as i64,
        ),
        (
            "user_memory_max_bytes",
            crate::DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES as i64,
        ),
    ];
    let ctx = section(SectionId::Context).expect("Context section must be registered");
    for (label, runtime_default) in cases {
        let field = ctx
            .fields
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("field '{label}' not found in Context section"));
        let default_value = match (field.default)() {
            FieldValue::Integer(v) => v,
            other => panic!("field '{label}' default is not Integer: {other:?}"),
        };
        assert_eq!(default_value, runtime_default, "{label} default() mismatch");
        let display_val: i64 = field
            .default_display
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .unwrap_or_else(|_| panic!("field '{label}' default_display not parseable"));
        assert_eq!(
            display_val, runtime_default,
            "{label} default_display mismatch"
        );
    }
}

#[test]
fn context_section_fields_round_trip() {
    let ctx = section(SectionId::Context).expect("Context section must be registered");
    for f in ctx.fields {
        let mut cfg = AppConfig::default();
        match f.kind {
            FieldKind::Info => {
                // Read-only: get returns a non-empty string, set is a no-op.
                assert!(
                    matches!((f.get)(&cfg), FieldValue::String(_)),
                    "{}",
                    f.label
                );
                (f.set)(&mut cfg, FieldValue::String(String::new())).unwrap();
            }
            FieldKind::Bool => {
                (f.set)(&mut cfg, FieldValue::Bool(false)).unwrap();
                assert_eq!((f.get)(&cfg), FieldValue::Bool(false), "{}", f.label);
                (f.set)(&mut cfg, FieldValue::Bool(true)).unwrap();
                assert_eq!((f.get)(&cfg), FieldValue::Bool(true), "{}", f.label);
            }
            FieldKind::Integer { min, .. } => {
                let v = min.max(7);
                (f.set)(&mut cfg, FieldValue::Integer(v)).unwrap();
                assert_eq!((f.get)(&cfg), FieldValue::Integer(v), "{}", f.label);
            }
            FieldKind::OptionalInteger { min, max, .. } => {
                // Pick a value inside the field's declared range so bounded
                // fields (e.g. a 1..=100 percent) round-trip too.
                let v = 4242i64.clamp(min, max);
                (f.set)(&mut cfg, FieldValue::OptionalInteger(Some(v))).unwrap();
                assert_eq!(
                    (f.get)(&cfg),
                    FieldValue::OptionalInteger(Some(v)),
                    "{}",
                    f.label
                );
                (f.set)(&mut cfg, FieldValue::OptionalInteger(None)).unwrap();
                assert_eq!(
                    (f.get)(&cfg),
                    FieldValue::OptionalInteger(None),
                    "{}",
                    f.label
                );
            }
            FieldKind::Enum { options } => {
                for option in options {
                    (f.set)(&mut cfg, FieldValue::Enum(option)).unwrap();
                    assert_eq!((f.get)(&cfg), FieldValue::Enum(option), "{}", f.label);
                }
            }
            FieldKind::String { .. } => {
                (f.set)(&mut cfg, FieldValue::String("cheap-model".to_string())).unwrap();
                assert_eq!(
                    (f.get)(&cfg),
                    FieldValue::String("cheap-model".to_string()),
                    "{}",
                    f.label
                );
            }
            other => panic!("unexpected FieldKind in Context section: {other:?}"),
        }
    }
}

#[test]
fn context_percent_setters_reject_out_of_range() {
    let ctx = section(SectionId::Context).unwrap();
    let pct = ctx
        .fields
        .iter()
        .find(|f| f.label == "warn_at_percent")
        .unwrap();
    let mut cfg = AppConfig::default();
    assert!((pct.set)(&mut cfg, FieldValue::Integer(150)).is_err());
    assert!((pct.set)(&mut cfg, FieldValue::Integer(80)).is_ok());
}
