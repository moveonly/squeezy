use std::path::PathBuf;

use super::*;

fn temp_ws(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "squeezy-extract-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir temp ws");
    dir
}

#[test]
fn parse_extracts_save_and_delete_ops() {
    let json = r#"[
      {"op":"save","name":"prefers-bun","type":"feedback","description":"use bun","body":"Use bun."},
      {"op":"delete","name":"stale-fact"}
    ]"#;
    let ops = parse_extraction_ops(json);
    assert_eq!(ops.len(), 2);
    assert!(matches!(
        &ops[0],
        MemoryOp::Save { name, ty, .. } if name == "prefers-bun" && *ty == MemoryType::Feedback
    ));
    assert!(matches!(&ops[1], MemoryOp::Delete { name } if name == "stale-fact"));
}

#[test]
fn parse_tolerates_fences_and_prose() {
    let resp = "Sure:\n```json\n[{\"op\":\"save\",\"name\":\"x\",\"type\":\"project\",\
                \"description\":\"d\",\"body\":\"b\"}]\n```\nDone.";
    assert_eq!(parse_extraction_ops(resp).len(), 1);
}

#[test]
fn parse_skips_invalid_type_and_missing_fields() {
    let json = r#"[
      {"op":"save","name":"bad","type":"nonsense","description":"d","body":"b"},
      {"op":"save","name":"missing-body","type":"user","description":"d"},
      {"op":"save","name":"ok","type":"user","description":"d","body":"b"}
    ]"#;
    let ops = parse_extraction_ops(json);
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], MemoryOp::Save { name, .. } if name == "ok"));
}

#[test]
fn parse_handles_empty_and_garbage() {
    assert!(parse_extraction_ops("[]").is_empty());
    assert!(parse_extraction_ops("no json here").is_empty());
    assert!(parse_extraction_ops("").is_empty());
}

#[test]
fn parse_caps_op_count() {
    let one =
        "{\"op\":\"save\",\"name\":\"n\",\"type\":\"user\",\"description\":\"d\",\"body\":\"b\"}";
    let many = vec![one; 20].join(",");
    assert_eq!(
        parse_extraction_ops(&format!("[{many}]")).len(),
        EXTRACTION_MAX_OPS
    );
}

#[test]
fn apply_ops_persists_and_deletes_project_scoped() {
    // Project/reference ops route to the workspace base, so this test never
    // touches the real `~/.squeezy` (no HOME mutation needed).
    let ws = temp_ws("apply");
    let memory = Memory::new(Some(&ws));
    let result = apply_ops(
        &memory,
        vec![
            MemoryOp::Save {
                name: "auth-rewrite".into(),
                ty: MemoryType::Project,
                description: "compliance-driven".into(),
                body: "The rewrite is compliance-driven.".into(),
                title: None,
                hook: None,
            },
            MemoryOp::Save {
                name: "BAD SLUG".into(),
                ty: MemoryType::Project,
                description: "d".into(),
                body: "b".into(),
                title: None,
                hook: None,
            },
        ],
    );
    assert_eq!(result.saved.len(), 1, "one valid save");
    assert_eq!(result.skipped, 1, "bad slug skipped, not fatal");
    assert!(ws.join(".squeezy/memory/auth-rewrite.md").exists());

    let del = apply_ops(
        &memory,
        vec![MemoryOp::Delete {
            name: "auth-rewrite".into(),
        }],
    );
    assert_eq!(del.deleted, vec!["auth-rewrite".to_string()]);
    assert!(!ws.join(".squeezy/memory/auth-rewrite.md").exists());

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn build_input_includes_slice_and_indexes() {
    let input = build_extraction_input("User: hi\nAssistant: hello", "- [G](memory/g.md) — x", "");
    assert!(input.contains("Current global memory index"));
    assert!(input.contains("- [G](memory/g.md) — x"));
    assert!(input.contains("(empty)"), "empty project index labeled");
    assert!(input.contains("User: hi"));
    assert!(input.contains("Emit the JSON array"));
}

#[test]
fn extraction_result_summary() {
    let mut result = ExtractionResult::default();
    assert!(result.summary().is_none(), "no changes -> no summary");
    result.saved.push(("who-i-am".into(), MemoryType::User));
    result.deleted.push("stale".into());
    let summary = result.summary().expect("summary");
    assert!(summary.contains("saved who-i-am (user)"), "{summary}");
    assert!(summary.contains("removed stale"), "{summary}");
}
