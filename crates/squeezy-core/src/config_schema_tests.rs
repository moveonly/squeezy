use super::*;

#[test]
fn every_section_has_at_least_one_field() {
    for section in CONFIG_SECTIONS {
        // `Reset` is a synthetic action-only section — the TUI renders
        // tier-delete rows instead of `FieldMeta` entries.
        if section.id == SectionId::Reset {
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
        for option in PERMISSION_MODE_OPTIONS {
            (f.set)(&mut cfg, FieldValue::Enum(option)).unwrap();
            match (f.get)(&cfg) {
                FieldValue::Enum(v) => assert_eq!(v, *option, "{}", f.label),
                other => panic!("unexpected: {:?}", other),
            }
        }
    }
}
