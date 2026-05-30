use serde_json::json;

use super::{ApplyPatchArgs, render_apply_patch_diff};

fn args_from(value: serde_json::Value) -> ApplyPatchArgs {
    serde_json::from_value(value).expect("valid ApplyPatchArgs JSON")
}

#[test]
fn render_create_file_uses_dev_null_old_header_and_create_marker() {
    let args = args_from(json!({
        "operations": [{
            "kind": "create_file",
            "path": "crates/squeezy-eval/README-PROBE.md",
            "contents": "# Probe one\n"
        }]
    }));
    let diff = render_apply_patch_diff(&args).expect("create_file produces a preview blob");
    assert_eq!(
        diff,
        concat!(
            "--- /dev/null\n",
            "+++ b/crates/squeezy-eval/README-PROBE.md\n",
            "@@ -0,0 +1,1 @@\n",
            "@@ create b/crates/squeezy-eval/README-PROBE.md @@\n",
            "+# Probe one\n",
        ),
        "create_file should swap the old side to /dev/null, carry a +1,N hunk \
         header, and emit a per-file delimiter marker that survives the \
         renderer's metadata-line filter; got:\n{diff}",
    );
}

#[test]
fn render_delete_file_uses_dev_null_new_header_and_delete_marker() {
    let args = args_from(json!({
        "operations": [{
            "kind": "delete_file",
            "path": "src/legacy.rs"
        }]
    }));
    let diff = render_apply_patch_diff(&args).expect("delete_file produces a preview blob");
    assert_eq!(
        diff,
        concat!(
            "--- a/src/legacy.rs\n",
            "+++ /dev/null\n",
            "@@ -1,0 +0,0 @@\n",
            "@@ delete a/src/legacy.rs @@\n",
        ),
        "delete_file should swap the new side to /dev/null and still surface a \
         visible per-op marker even though there is no body at preview time; \
         got:\n{diff}",
    );
}

#[test]
fn render_move_file_emits_rename_marker_and_path_endpoints() {
    let args = args_from(json!({
        "operations": [{
            "kind": "move_file",
            "from": "src/old.rs",
            "to": "src/new.rs"
        }]
    }));
    let diff = render_apply_patch_diff(&args).expect("move_file produces a preview blob");
    assert_eq!(
        diff,
        concat!(
            "--- a/src/old.rs\n",
            "+++ b/src/new.rs\n",
            "@@ -1,0 +1,0 @@\n",
            "@@ rename a/src/old.rs -> b/src/new.rs @@\n",
        ),
        "move_file should emit both endpoints in the diff headers plus a \
         non-empty rename marker, otherwise the approval preview shows \
         literally nothing for a rename; got:\n{diff}",
    );
}

#[test]
fn render_multi_op_preserves_per_file_delimiters() {
    let args = args_from(json!({
        "operations": [
            {"kind": "create_file", "path": "a.md", "contents": "alpha\n"},
            {"kind": "create_file", "path": "b.md", "contents": "beta\n"}
        ]
    }));
    let diff =
        render_apply_patch_diff(&args).expect("multi-op create_file produces a preview blob");
    // Each op contributes its own `--- ... +++ ...` section AND a
    // textual `@@ create b/<path> @@` marker. The marker is what survives
    // the renderer's metadata-line filter (which strips `--- ` / `+++ `),
    // so without it the two files collapse into one undelimited block.
    assert!(
        diff.contains("@@ create b/a.md @@\n"),
        "missing first-file marker in multi-op preview:\n{diff}",
    );
    assert!(
        diff.contains("@@ create b/b.md @@\n"),
        "missing second-file marker in multi-op preview:\n{diff}",
    );
    assert!(diff.contains("+alpha\n"));
    assert!(diff.contains("+beta\n"));
}

#[test]
fn render_search_replace_keeps_in_place_headers() {
    let args = args_from(json!({
        "patches": [{
            "path": "lib.rs",
            "search": "old contents",
            "replace": "new contents"
        }]
    }));
    let diff = render_apply_patch_diff(&args).expect("search/replace produces a preview blob");
    // In-place edits keep `a/` and `b/` on both sides — only create / delete
    // / move should reach for `/dev/null`.
    assert!(diff.starts_with("--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n"));
    assert!(diff.contains("-old contents\n"));
    assert!(diff.contains("+new contents\n"));
    assert!(
        !diff.contains("/dev/null"),
        "search/replace must not synthesise /dev/null headers:\n{diff}",
    );
}

#[test]
fn render_returns_none_when_no_ops_or_patches() {
    let args = args_from(json!({}));
    assert!(render_apply_patch_diff(&args).is_none());
}
