use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::SkillsConfig;

use super::*;
use crate::SkillCatalog;

#[test]
fn parses_skill_manifest_with_all_fields() {
    let manifest = parse_skill_manifest(
        r#"
tool_deps = ["mcp:exa", "shell"]
icon = "assets/icon.png"
prompt_hint = "Use this when navigating the repo."
"#,
    )
    .expect("parse manifest");

    assert_eq!(manifest.tool_deps, vec!["mcp:exa", "shell"]);
    assert_eq!(manifest.icon, Some(PathBuf::from("assets/icon.png")));
    assert_eq!(
        manifest.prompt_hint.as_deref(),
        Some("Use this when navigating the repo.")
    );
}

#[test]
fn empty_skill_manifest_parses_into_default() {
    let manifest = parse_skill_manifest("").expect("empty toml parses");
    assert!(manifest.tool_deps.is_empty());
    assert!(manifest.icon.is_none());
    assert!(manifest.prompt_hint.is_none());
}

#[test]
fn malformed_skill_manifest_returns_error() {
    let error = parse_skill_manifest("tool_deps = not a list\n").expect_err("invalid toml");
    assert!(!error.is_empty());
}

#[test]
fn manifest_is_attached_to_summary_during_discovery() {
    let root = temp_workspace("skills_manifest_attached");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(
        skill_dir.join("skill.toml"),
        r#"tool_deps = ["mcp:exa"]
icon = "assets/icon.png"
prompt_hint = "Use the graph carefully."
"#,
    )
    .expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    let manifest = summary.manifest.expect("manifest attached");
    assert_eq!(manifest.tool_deps, vec!["mcp:exa"]);
    assert_eq!(manifest.icon, Some(PathBuf::from("assets/icon.png")));
    assert_eq!(
        manifest.prompt_hint.as_deref(),
        Some("Use the graph carefully.")
    );

    let json = catalog.summaries_json();
    let entry = &json["skills"][0];
    assert_eq!(entry["manifest"]["tool_deps"][0], "mcp:exa");
    assert_eq!(entry["manifest"]["icon"], "assets/icon.png");
    assert_eq!(entry["manifest"]["prompt_hint"], "Use the graph carefully.");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn manifest_tool_deps_and_hint_surface_in_prompt_block() {
    let root = temp_workspace("skills_manifest_prompt");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(
        skill_dir.join("skill.toml"),
        r#"tool_deps = ["mcp:exa", "web_fetch"]
prompt_hint = "Activate for Rust graph questions."
"#,
    )
    .expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let loaded = catalog.load("rust-nav").expect("load");
    let block = loaded.prompt_block();

    assert!(block.contains("<manifest>"), "{block}");
    assert!(block.contains("<tool>mcp:exa</tool>"), "{block}");
    assert!(block.contains("<tool>web_fetch</tool>"), "{block}");
    assert!(
        block.contains("<prompt_hint>Activate for Rust graph questions.</prompt_hint>"),
        "{block}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_skill_manifest_leaves_summary_unaffected() {
    let root = temp_workspace("skills_manifest_missing");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.manifest.is_none());

    let loaded = catalog.load("rust-nav").expect("load");
    assert!(!loaded.prompt_block().contains("<manifest>"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_skill_manifest_is_skipped_without_dropping_skill() {
    let root = temp_workspace("skills_manifest_malformed");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(skill_dir.join("skill.toml"), "tool_deps = not a list\n").expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.name, "rust-nav");
    assert!(summary.manifest.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn empty_skill_manifest_does_not_attach_to_summary() {
    let root = temp_workspace("skills_manifest_empty_file");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(skill_dir.join("skill.toml"), "").expect("write empty skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.manifest.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_manifest_unknown_keys_are_rejected() {
    // Strict mode keeps the format auditable: typos in field names must
    // surface so a misspelled `tools_dep` doesn't silently drop the
    // declared dependency.
    let error = parse_skill_manifest("bogus_field = 1\n").expect_err("unknown key");
    assert!(
        error.contains("unknown") || error.contains("bogus"),
        "{error}"
    );
}

fn write_skill(dir: &Path, name: &str, description: &str, triggers: &[&str]) {
    write_skill_with_body(dir, name, description, triggers, &format!("# {name}\n"));
}

fn write_skill_with_body(dir: &Path, name: &str, description: &str, triggers: &[&str], body: &str) {
    fs::create_dir_all(dir).expect("mkdir");
    let triggers = triggers
        .iter()
        .map(|trigger| format!("  - {trigger}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {description}\ntriggers:\n{triggers}\n---\n{body}"
        ),
    )
    .expect("write skill");
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("squeezy_{name}_{nonce}"));
    fs::create_dir_all(&path).expect("create temp workspace");
    path
}
