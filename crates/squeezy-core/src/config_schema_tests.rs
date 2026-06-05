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
