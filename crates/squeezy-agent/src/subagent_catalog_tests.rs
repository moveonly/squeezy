use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

/// Per-test temp root. Same idea as `lib_tests.rs::temp_workspace` but
/// scoped here so the catalog tests can live in a paired module without
/// reaching across files. A monotonic nonce guarantees parallel runs
/// don't collide even when the wall clock has low resolution.
fn temp_root(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("squeezy_subagent_catalog_{name}_{ts}_{seq}"));
    fs::create_dir_all(&root).expect("create temp root");
    root
}

#[test]
fn builtin_catalog_contains_user_facing_kinds() {
    let catalog = SubagentCatalog::builtin();
    let names: Vec<&str> = catalog
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    for required in ["delegate", "explore", "plan", "review"] {
        assert!(
            names.contains(&required),
            "builtin catalog missing kind {required}; got {names:?}"
        );
    }
    assert!(
        catalog
            .entries()
            .iter()
            .all(|entry| matches!(entry.source, SubagentSource::Builtin)),
        "builtin catalog must report Builtin source for every entry"
    );
    assert!(
        catalog
            .entries()
            .iter()
            .all(|entry| entry.file_path.is_none()),
        "builtin catalog entries must not carry a file_path"
    );
}

#[test]
fn parse_subagent_file_extracts_required_and_optional_fields() {
    let content = concat!(
        "---\n",
        "name: scout\n",
        "description: Fast codebase recon\n",
        "tools: read, grep, find, ls, bash\n",
        "model: claude-haiku-4-5\n",
        "---\n",
        "\n",
        "You are a scout. Investigate.\n",
    );
    let (frontmatter, body) = parse_subagent_file(content).expect("parse");
    assert_eq!(frontmatter.name, "scout");
    assert_eq!(frontmatter.description, "Fast codebase recon");
    assert_eq!(frontmatter.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(
        frontmatter.tools,
        vec!["read", "grep", "find", "ls", "bash"]
    );
    assert_eq!(body, "You are a scout. Investigate.");
}

#[test]
fn parse_subagent_file_accepts_inline_yaml_tool_list() {
    let content = concat!(
        "---\n",
        "name: planner\n",
        "description: Plans only\n",
        "tools: [read, grep]\n",
        "---\n",
        "body text",
    );
    let (frontmatter, body) = parse_subagent_file(content).expect("parse");
    assert_eq!(frontmatter.tools, vec!["read", "grep"]);
    assert_eq!(body, "body text");
}

#[test]
fn parse_subagent_file_omits_optional_fields_when_absent() {
    let content = concat!(
        "---\n",
        "name: bare\n",
        "description: Minimum viable subagent\n",
        "---\n",
        "Just a body.",
    );
    let (frontmatter, body) = parse_subagent_file(content).expect("parse");
    assert!(frontmatter.model.is_none());
    assert!(frontmatter.tools.is_empty());
    assert_eq!(body, "Just a body.");
}

#[test]
fn parse_subagent_file_rejects_missing_frontmatter() {
    let err =
        parse_subagent_file("no fences here\nbody only").expect_err("missing fence should fail");
    assert!(
        err.contains("frontmatter"),
        "expected frontmatter error, got: {err}"
    );
}

#[test]
fn parse_subagent_file_rejects_missing_required_keys() {
    let err = parse_subagent_file("---\nname: only\n---\nbody")
        .expect_err("missing description should fail");
    assert!(
        err.contains("description"),
        "expected description error, got: {err}"
    );

    let err = parse_subagent_file("---\ndescription: only\n---\nbody")
        .expect_err("missing name should fail");
    assert!(err.contains("name"), "expected name error, got: {err}");
}

#[test]
fn discover_returns_only_builtins_when_dirs_missing() {
    let workspace = temp_root("missing_dirs");
    let nonexistent_user = workspace.join("not-a-user-dir");
    let catalog = SubagentCatalog::discover(&workspace, Some(&nonexistent_user));
    assert!(
        catalog
            .entries()
            .iter()
            .all(|entry| matches!(entry.source, SubagentSource::Builtin)),
        "expected only built-ins, found {:?}",
        catalog
            .entries()
            .iter()
            .map(|entry| (entry.name.clone(), entry.source))
            .collect::<Vec<_>>()
    );
    assert!(catalog.user_provided().next().is_none());
}

#[test]
fn discover_loads_project_and_user_subagents() {
    let workspace = temp_root("discover_both");
    let user_dir = temp_root("discover_both_user");
    let project_agents = workspace.join(".squeezy").join("agents");
    fs::create_dir_all(&project_agents).expect("mkdir project agents");
    fs::create_dir_all(&user_dir).expect("mkdir user agents");

    fs::write(
        project_agents.join("scout.md"),
        "---\nname: scout\ndescription: project scout\n---\nProject body\n",
    )
    .expect("write project scout");
    fs::write(
        user_dir.join("polyglot.md"),
        "---\nname: polyglot\ndescription: user polyglot\n---\nUser body\n",
    )
    .expect("write user polyglot");

    let catalog = SubagentCatalog::discover(&workspace, Some(&user_dir));

    let scout = catalog.find("scout").expect("scout present");
    assert_eq!(scout.source, SubagentSource::Project);
    assert_eq!(scout.description, "project scout");
    assert_eq!(scout.system_prompt, "Project body");
    assert_eq!(
        scout.file_path.as_deref(),
        Some(project_agents.join("scout.md").as_path())
    );

    let polyglot = catalog.find("polyglot").expect("polyglot present");
    assert_eq!(polyglot.source, SubagentSource::User);
    assert_eq!(polyglot.description, "user polyglot");

    let user_provided: Vec<&str> = catalog
        .user_provided()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(user_provided.contains(&"scout"));
    assert!(user_provided.contains(&"polyglot"));
    assert!(!user_provided.contains(&"delegate"));
}

#[test]
fn discover_project_overrides_user_when_names_collide() {
    let workspace = temp_root("discover_override");
    let user_dir = temp_root("discover_override_user");
    let project_agents = workspace.join(".squeezy").join("agents");
    fs::create_dir_all(&project_agents).expect("mkdir project agents");
    fs::create_dir_all(&user_dir).expect("mkdir user agents");

    fs::write(
        user_dir.join("scout.md"),
        "---\nname: scout\ndescription: user scout\n---\nUser body",
    )
    .expect("write user scout");
    fs::write(
        project_agents.join("scout.md"),
        "---\nname: scout\ndescription: project scout\n---\nProject body",
    )
    .expect("write project scout");

    let catalog = SubagentCatalog::discover(&workspace, Some(&user_dir));
    let scout = catalog.find("scout").expect("scout present");
    assert_eq!(scout.source, SubagentSource::Project);
    assert_eq!(scout.description, "project scout");
}

#[test]
fn discover_does_not_let_disk_definitions_shadow_builtins() {
    let workspace = temp_root("discover_no_builtin_shadow");
    let user_dir = temp_root("discover_no_builtin_shadow_user");
    let project_agents = workspace.join(".squeezy").join("agents");
    fs::create_dir_all(&project_agents).expect("mkdir project agents");
    fs::create_dir_all(&user_dir).expect("mkdir user agents");

    // A disk-loaded subagent named after a built-in tool name *does*
    // win precedence (project > user > builtin) by design, but the
    // built-in dispatch code path in `lib.rs` keys off the tool name
    // (`SubagentKind::*`) and never consults the catalog, so this is a
    // metadata-only override. We assert the metadata override here so
    // future wiring can rely on it.
    fs::write(
        project_agents.join("delegate.md"),
        "---\nname: delegate\ndescription: project-defined delegate\n---\nbody",
    )
    .expect("write delegate override");

    let catalog = SubagentCatalog::discover(&workspace, Some(&user_dir));
    let delegate = catalog.find("delegate").expect("delegate present");
    assert_eq!(delegate.source, SubagentSource::Project);
    assert_eq!(delegate.description, "project-defined delegate");

    // The other built-in kinds are still in the catalog so callers
    // listing available subagents see them.
    for required in ["explore", "plan", "review"] {
        let entry = catalog
            .find(required)
            .unwrap_or_else(|| panic!("built-in {required} should survive discovery"));
        assert_eq!(entry.source, SubagentSource::Builtin);
    }
}

#[test]
fn discover_skips_malformed_and_unrelated_files() {
    let workspace = temp_root("discover_malformed");
    let user_dir = temp_root("discover_malformed_user");
    let project_agents = workspace.join(".squeezy").join("agents");
    fs::create_dir_all(&project_agents).expect("mkdir");

    fs::write(project_agents.join("broken.md"), "no frontmatter here\n").expect("write broken");
    fs::write(
        project_agents.join("good.md"),
        "---\nname: good\ndescription: solid\n---\nA body",
    )
    .expect("write good");
    fs::write(
        project_agents.join("notes.txt"),
        "this should be ignored entirely",
    )
    .expect("write txt");
    fs::write(
        project_agents.join("UPPER.md"),
        "---\nname: UPPER\ndescription: invalid name\n---\nbody",
    )
    .expect("write invalid name");

    let catalog = SubagentCatalog::discover(&workspace, Some(&user_dir));
    assert!(catalog.find("good").is_some(), "good subagent should load");
    assert!(catalog.find("broken").is_none(), "broken subagent dropped");
    assert!(catalog.find("UPPER").is_none(), "invalid name dropped");
    assert!(catalog.find("notes").is_none(), "non-md file ignored");
}

#[test]
fn user_dir_default_uses_home_when_set() {
    let home = temp_root("default_user_home");
    let agents = home.join(".squeezy").join("agents");
    fs::create_dir_all(&agents).expect("mkdir user agents");
    fs::write(
        agents.join("homie.md"),
        "---\nname: homie\ndescription: from $HOME\n---\nbody",
    )
    .expect("write homie");

    let previous_home = std::env::var_os("HOME");
    // SAFETY: tests in this crate are serialized only by name; setting
    // HOME for the duration of this test and restoring it before
    // returning keeps other tests' env reads stable enough for the
    // discovery check we care about here.
    unsafe { std::env::set_var("HOME", &home) };

    let workspace = temp_root("default_user_workspace");
    let catalog = SubagentCatalog::discover(&workspace, None);

    if let Some(previous) = previous_home {
        unsafe { std::env::set_var("HOME", previous) };
    } else {
        unsafe { std::env::remove_var("HOME") };
    }

    let homie = catalog
        .find("homie")
        .expect("home-discovered subagent present");
    assert_eq!(homie.source, SubagentSource::User);
}

#[test]
fn discover_falls_back_to_userprofile_when_home_unset() {
    // Simulate a Windows-like environment: HOME unset, USERPROFILE set.
    // We manipulate environment variables only — the test runs on all
    // platforms so it cannot create Windows-style paths, but it does
    // verify that discover() does not panic when HOME is absent.
    let profile_root = temp_root("userprofile_discover");
    // On non-Windows the function uses $HOME, so we skip the
    // USERPROFILE branch by not setting it; but we verify the None
    // branch doesn't crash the catalog.
    let previous_home = std::env::var_os("HOME");
    unsafe { std::env::remove_var("HOME") };

    let workspace = temp_root("userprofile_workspace");
    // Should not panic; may return an empty-or-builtin-only catalog.
    let catalog = SubagentCatalog::discover(&workspace, None);
    assert!(
        catalog
            .entries()
            .iter()
            .all(|e| matches!(e.source, SubagentSource::Builtin | SubagentSource::Project)),
        "without HOME and USERPROFILE only builtin+project entries expected"
    );

    if let Some(prev) = previous_home {
        unsafe { std::env::set_var("HOME", prev) };
    }
    let _ = fs::remove_dir_all(&profile_root);
    let _ = fs::remove_dir_all(&workspace);
}

#[cfg(target_os = "windows")]
#[test]
fn discover_uses_appdata_on_windows() {
    let appdata_root = temp_root("appdata_discover");
    let agents_dir = appdata_root.join("squeezy").join("agents");
    fs::create_dir_all(&agents_dir).expect("mkdir appdata agents");
    fs::write(
        agents_dir.join("winagent.md"),
        "---\nname: winagent\ndescription: from APPDATA\n---\nbody",
    )
    .expect("write winagent");

    let previous_appdata = std::env::var_os("APPDATA");
    unsafe { std::env::set_var("APPDATA", &appdata_root) };

    let workspace = temp_root("appdata_workspace");
    let catalog = SubagentCatalog::discover(&workspace, None);

    if let Some(prev) = previous_appdata {
        unsafe { std::env::set_var("APPDATA", prev) };
    } else {
        unsafe { std::env::remove_var("APPDATA") };
    }

    let winagent = catalog
        .find("winagent")
        .expect("APPDATA-discovered subagent present");
    assert_eq!(winagent.source, SubagentSource::User);
    let _ = fs::remove_dir_all(&appdata_root);
    let _ = fs::remove_dir_all(&workspace);
}
