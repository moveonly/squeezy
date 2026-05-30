use super::*;
use squeezy_core::config_schema::{CONFIG_SECTIONS, FieldMeta, SectionId};

fn permission_field(label: &str) -> &'static FieldMeta {
    CONFIG_SECTIONS
        .iter()
        .find(|section| section.id == SectionId::Permissions)
        .and_then(|section| section.fields.iter().find(|field| field.label == label))
        .expect("permission field exists")
}

#[test]
fn permission_detail_fields_write_to_custom_table() {
    assert_eq!(
        field_write_path(permission_field("read")),
        &["permissions", "custom", "read"][..]
    );
    assert_eq!(
        field_write_path(permission_field("web")),
        &["permissions", "custom", "network"][..]
    );
    assert_eq!(
        field_write_path(permission_field("destructive")),
        &["permissions", "custom", "destructive"][..]
    );
    assert_eq!(
        field_write_path(permission_field("mode")),
        &["permissions", "mode"][..]
    );
}

#[test]
fn clearing_permission_detail_fields_removes_custom_and_legacy_paths() {
    let edits = clear_field_edits(permission_field("web"));
    let paths: Vec<_> = edits.iter().map(|edit| edit.path).collect();
    assert_eq!(
        paths,
        vec![
            &["permissions", "custom", "network"][..],
            &["permissions", "web"][..],
        ]
    );

    let mode_edits = clear_field_edits(permission_field("mode"));
    assert_eq!(mode_edits.len(), 1);
    assert_eq!(mode_edits[0].path, &["permissions", "mode"][..]);
}
