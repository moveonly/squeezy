use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::SkillsConfig;

use crate::SkillCatalog;

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

#[test]
fn unmet_tool_deps_flags_missing_builtin_and_mcp() {
    let deps = vec![
        "shell".to_string(),
        "websearch".to_string(),
        "mcp:exa".to_string(),
        "mcp:parallel".to_string(),
        "  ".to_string(),
    ];
    let available_tools: BTreeSet<String> = ["shell".to_string()].into_iter().collect();
    let available_mcp: BTreeSet<String> = ["exa".to_string()].into_iter().collect();
    let missing = super::unmet_tool_deps(&deps, &available_tools, &available_mcp);
    assert_eq!(
        missing,
        vec!["websearch".to_string(), "mcp:parallel".to_string()]
    );
}

#[test]
fn unmet_tool_deps_empty_when_all_satisfied() {
    let deps = vec!["shell".to_string(), "mcp:exa".to_string()];
    let available_tools: BTreeSet<String> = ["shell".to_string()].into_iter().collect();
    let available_mcp: BTreeSet<String> = ["exa".to_string()].into_iter().collect();
    let missing = super::unmet_tool_deps(&deps, &available_tools, &available_mcp);
    assert!(missing.is_empty());
}

#[test]
fn skill_scan_dirs_includes_ancestor_project_roots() {
    // Layout:
    //   /root/.git            ← git root, stops the ancestor walk
    //   /root/.squeezy/skills/ ← ancestor project root
    //   /root/packages/foo/   ← workspace_root (launch dir)
    let root = temp_workspace("skills_scan_ancestor");
    let git_dir = root.join(".git");
    fs::create_dir_all(&git_dir).expect("create .git");
    let ancestor_skills = root.join(".squeezy/skills");
    fs::create_dir_all(&ancestor_skills).expect("create ancestor skills");
    let ws_root = root.join("packages/foo");
    fs::create_dir_all(&ws_root).expect("create workspace dir");

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let dirs = super::skill_scan_dirs(&ws_root, &config);

    // The ancestor's `.squeezy/skills` dir should be in the scan list.
    let has_ancestor = dirs.iter().any(|d| d == &ancestor_skills);
    assert!(
        has_ancestor,
        "skill_scan_dirs must include ancestor project skill roots; got: {dirs:?}"
    );
}

#[test]
fn validate_skill_dirs_includes_xdg_user_dir() {
    let root = temp_workspace("skills_validate_xdg_malformed");
    let xdg_root = root.join("xdg").join("squeezy").join("skills");
    let bad_dir = xdg_root.join("bad-xdg");
    fs::create_dir_all(&bad_dir).expect("mkdir bad-xdg");
    fs::write(bad_dir.join("SKILL.md"), "not frontmatter at all").expect("write bad skill");

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        xdg_user_dir: Some(xdg_root),
        ..Default::default()
    };

    let results = super::validate_skill_dirs(&root, &config);
    let bad = results
        .iter()
        .find(|r| {
            r.path
                .to_str()
                .map(|s| s.contains("bad-xdg"))
                .unwrap_or(false)
        })
        .expect("bad-xdg result must be present");
    assert!(
        bad.outcome.is_err(),
        "malformed XDG skill must produce an error: {:?}",
        bad.outcome
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn validate_skill_dirs_includes_ancestor_malformed_skill() {
    // Same monorepo layout, but with a malformed SKILL.md in the ancestor root.
    let root = temp_workspace("skills_validate_ancestor_malformed");
    let git_dir = root.join(".git");
    fs::create_dir_all(&git_dir).expect("create .git");
    let bad_dir = root.join(".squeezy/skills/bad-ancestor");
    fs::create_dir_all(&bad_dir).expect("mkdir bad-ancestor");
    fs::write(bad_dir.join("SKILL.md"), "this is not valid frontmatter").expect("write bad skill");
    let ws_root = root.join("packages/foo");
    fs::create_dir_all(&ws_root).expect("create workspace");

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let results = super::validate_skill_dirs(&ws_root, &config);
    assert!(
        !results.is_empty(),
        "validate must find the ancestor skill even though it is malformed"
    );
    let bad = results
        .iter()
        .find(|r| {
            r.path
                .to_str()
                .map(|s| s.contains("bad-ancestor"))
                .unwrap_or(false)
        })
        .expect("bad-ancestor result must be present");
    assert!(
        bad.outcome.is_err(),
        "malformed ancestor skill must produce an error: {:?}",
        bad.outcome
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn validate_skill_dirs_catches_malformed_files_that_discovery_drops() {
    let root = temp_workspace("skills_validate_dirs_malformed");
    // Good skill — discovery and validate both see it.
    let good_dir = root.join(".squeezy/skills/good-skill");
    fs::create_dir_all(&good_dir).expect("mkdir good");
    fs::write(
        good_dir.join("SKILL.md"),
        "---\nname: good-skill\ndescription: \"works\"\n---\n# Good\n",
    )
    .expect("write good");
    // Malformed skill — discovery silently drops it; validate must report it.
    let bad_dir = root.join(".squeezy/skills/bad-skill");
    fs::create_dir_all(&bad_dir).expect("mkdir bad");
    fs::write(bad_dir.join("SKILL.md"), "not frontmatter at all").expect("write bad");

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    // Discovery should only produce the good skill.
    let catalog = SkillCatalog::discover(&root, &config);
    assert_eq!(
        catalog.summaries().len(),
        1,
        "discovery must drop the bad skill"
    );

    // validate_skill_dirs must report both.
    let results = super::validate_skill_dirs(&root, &config);
    assert_eq!(
        results.len(),
        2,
        "validator must include both SKILL.md files"
    );
    let bad_result = results
        .iter()
        .find(|r| {
            r.path
                .to_str()
                .map(|s| s.contains("bad-skill"))
                .unwrap_or(false)
        })
        .expect("bad-skill result must be present");
    assert!(
        bad_result.outcome.is_err(),
        "malformed SKILL.md must produce an error result: {:?}",
        bad_result.outcome
    );
    let good_result = results
        .iter()
        .find(|r| {
            r.path
                .to_str()
                .map(|s| s.contains("good-skill"))
                .unwrap_or(false)
        })
        .expect("good-skill result must be present");
    assert!(
        good_result.outcome.is_ok(),
        "well-formed SKILL.md must produce an ok result"
    );

    let _ = fs::remove_dir_all(root);
}
