use std::collections::HashSet;

use super::{deny_write_ace_should_inherit, path_key_is_equal_or_beneath};

fn roots(keys: &[&str]) -> HashSet<String> {
    keys.iter().map(|key| key.to_string()).collect()
}

#[test]
fn path_key_relationship_requires_path_boundary() {
    assert!(path_key_is_equal_or_beneath("c:/tmp/work", "c:/tmp"));
    assert!(path_key_is_equal_or_beneath("c:/tmp", "c:/tmp"));
    assert!(!path_key_is_equal_or_beneath("c:/tmp2/work", "c:/tmp"));
}

#[test]
fn ancestor_of_writable_root_uses_non_inheritable_deny() {
    let writable_roots = roots(&["c:/temp/squeezy-wsbx-test"]);

    assert!(
        !deny_write_ace_should_inherit("c:/temp", &writable_roots),
        "denying an ancestor like %TEMP% must not poison future workspace children",
    );
    assert!(
        deny_write_ace_should_inherit("c:/temp/other-world-writable", &writable_roots),
        "ordinary sibling escape directories still need inheritable denies",
    );
}
