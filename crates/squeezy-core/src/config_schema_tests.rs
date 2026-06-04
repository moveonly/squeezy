use super::*;

#[test]
fn every_section_has_at_least_one_field() {
    for section in CONFIG_SECTIONS {
        // `Reset` is a synthetic action-only section — the TUI renders
        // tier-delete rows instead of `FieldMeta` entries.
        if matches!(section.id, SectionId::Reset | SectionId::Themes) {
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
