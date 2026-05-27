//! Policy guard: the graph-first navigation policy must remain documented.
//!
//! `docs/internal/SEMANTIC_GRAPH.md` carries the affirmative design statement
//! that Squeezy keeps a graph-first navigation surface and does not switch to a
//! bash/grep shell-loop model. If that file disappears or the affirmation is
//! removed, this test fires so the deletion is intentional and reviewed.

use std::{fs, path::PathBuf};

fn semantic_graph_doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .join("docs")
        .join("internal")
        .join("SEMANTIC_GRAPH.md")
}

#[test]
fn semantic_graph_policy_doc_exists_and_affirms_graph_first_nav() {
    let path = semantic_graph_doc_path();
    let body =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("missing {}: {err}", path.display()));

    assert!(
        body.contains("Design Policy: Graph-First Navigation"),
        "{} must keep the graph-first navigation policy section",
        path.display(),
    );
    assert!(
        body.contains("will not retreat"),
        "{} must explicitly state that graph-first navigation is preserved",
        path.display(),
    );
    assert!(
        body.contains("bash"),
        "{} must contrast against the bash/grep shell-loop navigation model",
        path.display(),
    );
}
