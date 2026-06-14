use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{SkillConfigEntry, SkillsBudgetMode, SkillsConfig};
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::render;

#[test]
fn compat_project_overrides_user_and_compat_user() {
    let root = temp_workspace("skills_precedence_compat_project");
    let user = root.join("user");
    let compat = root.join("compat");
    write_skill(
        &compat.join("same"),
        "same",
        "compat user description",
        &["compat user trigger"],
    );
    write_skill(
        &user.join("same"),
        "same",
        "user description",
        &["user trigger"],
    );
    write_skill(
        &root.join(".agents/skills/same"),
        "same",
        "compat project description",
        &["compat project trigger"],
    );
    let config = SkillsConfig {
        user_dir: user,
        compat_user_dir: compat,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.description, "compat project description");
    assert_eq!(summary.source, SkillSource::CompatProject);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn native_project_overrides_compat_project() {
    let root = temp_workspace("skills_precedence_native_project");
    write_skill(
        &root.join(".agents/skills/same"),
        "same",
        "compat project description",
        &["compat project trigger"],
    );
    write_skill(
        &root.join(".squeezy/skills/same"),
        "same",
        "native project description",
        &["native project trigger"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.description, "native project description");
    assert_eq!(summary.source, SkillSource::Project);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_in_extra_root_is_discovered_as_extra_root_source() {
    // A skill that only exists under `SkillsConfig::extra_roots` must
    // surface in the catalog with the `ExtraRoot` source so operators
    // can see at-a-glance which shared root contributed which skill.
    let root = temp_workspace("skills_extra_root_loads");
    let extra = root.join("team-skills");
    write_skill(
        &extra.join("rust-nav"),
        "rust-nav",
        "Team Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        extra_roots: vec![extra],
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog
        .summaries()
        .into_iter()
        .find(|summary| summary.name == "rust-nav")
        .expect("extra-root skill must surface in the catalog");
    assert_eq!(summary.source, SkillSource::ExtraRoot);
    assert_eq!(summary.description, "Team Rust nav");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn project_root_overrides_extra_root_on_same_name_collision() {
    // The whole point of project-local skills is they win over shared
    // catalogs. When the team-shared `extra_roots` ships a skill that
    // collides with a workspace's `.squeezy/skills/` entry, the project
    // copy must take precedence and the shared copy must be shadowed.
    let root = temp_workspace("skills_extra_root_project_override");
    let extra = root.join("team-skills");
    write_skill(
        &extra.join("rust-nav"),
        "rust-nav",
        "Team Rust nav",
        &["team trigger"],
    );
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Project Rust nav",
        &["project trigger"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        extra_roots: vec![extra],
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    let summary = catalog
        .summaries()
        .into_iter()
        .find(|summary| summary.name == "rust-nav")
        .expect("collision must still surface a single rust-nav entry");
    assert_eq!(summary.source, SkillSource::Project);
    assert_eq!(summary.description, "Project Rust nav");
    assert!(
        logs.contains("skill name reused at higher precedence"),
        "expected shadow warning for the extra-root copy: {logs}"
    );
    assert!(
        logs.contains("overriding_source=\"project\"")
            || logs.contains("overriding_source=project"),
        "expected project as overriding source: {logs}"
    );
    assert!(
        logs.contains("overridden_source=\"extra_root\"")
            || logs.contains("overridden_source=extra_root"),
        "expected extra_root as overridden source: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_extra_root_warns_without_failing_discovery() {
    // Operators commonly point `extra_roots` at a network mount or a
    // git submodule that may not be present in every checkout. A
    // missing entry must surface a load-time warning (so the mistake is
    // visible) but must not prevent the remaining roots from loading.
    let root = temp_workspace("skills_extra_root_missing_warns");
    let present = root.join("present-team-skills");
    write_skill(&present.join("good"), "good", "Loadable skill", &[]);
    let missing = root.join("absent-team-skills");

    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        extra_roots: vec![missing.clone(), present],
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    let names: Vec<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert_eq!(
        names,
        vec!["good"],
        "the present extra root must still load alongside a missing one"
    );
    assert!(
        logs.contains("skills.extra_roots") && logs.contains("does not exist"),
        "expected a not-found warning for {}: {logs}",
        missing.display()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn monorepo_root_and_package_skills_both_load_from_subdir_cwd() {
    // Monorepo layout: skills live at the repo root *and* inside an
    // individual package. When the agent is launched from the package
    // (cwd = packages/foo), the catalog should surface both the
    // package-local skill and the root-level sibling so the package can
    // rely on shared monorepo-wide skills without copying them.
    let monorepo = temp_workspace("skills_monorepo_root_and_package");
    fs::create_dir_all(monorepo.join(".git")).expect("mkdir .git");
    write_skill(
        &monorepo.join(".squeezy/skills/root-skill"),
        "root-skill",
        "Shared monorepo skill",
        &[],
    );
    let package_root = monorepo.join("packages/foo");
    write_skill(
        &package_root.join(".squeezy/skills/package-skill"),
        "package-skill",
        "Package-local skill",
        &[],
    );
    let config = SkillsConfig {
        user_dir: monorepo.join("user-noop"),
        compat_user_dir: monorepo.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&package_root, &config);
    let names: BTreeSet<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert!(
        names.contains("root-skill"),
        "ancestor walk must surface the monorepo-root skill from cwd={}, got {names:?}",
        package_root.display()
    );
    assert!(
        names.contains("package-skill"),
        "cwd-local skill must still load alongside ancestor skills, got {names:?}"
    );

    let _ = fs::remove_dir_all(monorepo);
}

#[test]
fn cwd_local_skill_wins_over_same_name_skill_in_monorepo_root() {
    // When the same skill name exists at both the package cwd and the
    // monorepo root, the cwd-local copy must win. This is the rule that
    // lets a package override a shared monorepo skill without renaming
    // it: drop a same-name skill in the package's `.squeezy/skills/`
    // and it shadows the root version for that package's cwd.
    let monorepo = temp_workspace("skills_monorepo_cwd_wins");
    fs::create_dir_all(monorepo.join(".git")).expect("mkdir .git");
    write_skill(
        &monorepo.join(".squeezy/skills/shared"),
        "shared",
        "Monorepo-root version",
        &[],
    );
    let package_root = monorepo.join("packages/foo");
    write_skill(
        &package_root.join(".squeezy/skills/shared"),
        "shared",
        "Package-local version",
        &[],
    );
    let config = SkillsConfig {
        user_dir: monorepo.join("user-noop"),
        compat_user_dir: monorepo.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&package_root, &config);
    let summaries: Vec<SkillSummary> = catalog
        .summaries()
        .into_iter()
        .filter(|summary| summary.name == "shared")
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "same-name skill must collapse to a single entry, got {summaries:?}"
    );
    let summary = &summaries[0];
    assert_eq!(summary.description, "Package-local version");
    assert!(
        summary
            .location
            .starts_with(package_root.join(".squeezy/skills/shared")),
        "package-local skill location should win, got {}",
        summary.location.display()
    );

    let _ = fs::remove_dir_all(monorepo);
}

#[test]
fn ancestor_walk_picks_up_compat_agents_skills_dir() {
    // The compat `.agents/skills/` form must also be discovered along
    // the ancestor walk so monorepos that haven't migrated off the
    // legacy directory still get sibling skill visibility from a
    // package cwd.
    let monorepo = temp_workspace("skills_monorepo_compat_ancestor");
    fs::create_dir_all(monorepo.join(".git")).expect("mkdir .git");
    write_skill(
        &monorepo.join(".agents/skills/legacy"),
        "legacy",
        "Legacy compat ancestor skill",
        &[],
    );
    let package_root = monorepo.join("packages/foo");
    fs::create_dir_all(&package_root).expect("mkdir package");
    let config = SkillsConfig {
        user_dir: monorepo.join("user-noop"),
        compat_user_dir: monorepo.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&package_root, &config);
    let summary = catalog
        .summaries()
        .into_iter()
        .find(|summary| summary.name == "legacy")
        .expect("legacy ancestor skill must surface");
    assert_eq!(summary.source, SkillSource::CompatProject);

    let _ = fs::remove_dir_all(monorepo);
}

#[test]
fn ancestor_walk_stops_at_first_git_root() {
    // A nested git repository (e.g. a submodule) must terminate the
    // ancestor walk so a package never reaches into a parent
    // repository's skill set. The closer `.git` marker is the
    // authoritative boundary even when an outer ancestor would also
    // qualify as a repository root.
    let outer = temp_workspace("skills_monorepo_nested_git");
    fs::create_dir_all(outer.join(".git")).expect("mkdir outer .git");
    write_skill(
        &outer.join(".squeezy/skills/outer-skill"),
        "outer-skill",
        "Outer repo skill that must be invisible",
        &[],
    );
    let inner = outer.join("inner-repo");
    fs::create_dir_all(inner.join(".git")).expect("mkdir inner .git");
    write_skill(
        &inner.join(".squeezy/skills/inner-skill"),
        "inner-skill",
        "Inner repo skill",
        &[],
    );
    let package_root = inner.join("packages/foo");
    fs::create_dir_all(&package_root).expect("mkdir package");
    let config = SkillsConfig {
        user_dir: outer.join("user-noop"),
        compat_user_dir: outer.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&package_root, &config);
    let names: BTreeSet<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert!(
        names.contains("inner-skill"),
        "inner repo skill should still load from its own root, got {names:?}"
    );
    assert!(
        !names.contains("outer-skill"),
        "ancestor walk must stop at the inner repo's .git boundary, got {names:?}"
    );

    let _ = fs::remove_dir_all(outer);
}

#[test]
fn ancestor_walk_respects_native_over_compat_inside_same_ancestor() {
    // Inside a single ancestor, `.squeezy/skills/` must still win over
    // `.agents/skills/` on same-name collision, mirroring the cwd-level
    // precedence rule. The ancestor walk's "inner shadows outer" policy
    // applies across ancestors only — within one ancestor the existing
    // source precedence stays authoritative.
    let monorepo = temp_workspace("skills_monorepo_native_over_compat");
    fs::create_dir_all(monorepo.join(".git")).expect("mkdir .git");
    write_skill(
        &monorepo.join(".agents/skills/dual"),
        "dual",
        "Compat version",
        &[],
    );
    write_skill(
        &monorepo.join(".squeezy/skills/dual"),
        "dual",
        "Native version",
        &[],
    );
    let package_root = monorepo.join("packages/foo");
    fs::create_dir_all(&package_root).expect("mkdir package");
    let config = SkillsConfig {
        user_dir: monorepo.join("user-noop"),
        compat_user_dir: monorepo.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&package_root, &config);
    let summary = catalog
        .summaries()
        .into_iter()
        .find(|summary| summary.name == "dual")
        .expect("ancestor skill must surface");
    assert_eq!(summary.source, SkillSource::Project);
    assert_eq!(summary.description, "Native version");

    let _ = fs::remove_dir_all(monorepo);
}

#[test]
fn ancestor_walk_stops_at_workspace_root_when_it_is_itself_git_root() {
    // When the cwd is already a git root, the strict-ancestor walk
    // must be a no-op — a checkout at `~/code/myrepo` should never
    // reach into `~` or `/` looking for unrelated skill caches. The
    // `ancestor_project_roots` helper enforces this by short-circuiting
    // on `is_git_root(workspace_root)`.
    let root = temp_workspace("skills_monorepo_no_parent_walk");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    write_skill(
        &root.join(".squeezy/skills/local"),
        "local",
        "Local skill",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let names: Vec<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert_eq!(
        names,
        vec!["local"],
        "no ancestor walk should run when cwd is itself a git root, got {names:?}"
    );
    assert!(
        ancestor_project_roots(&root).is_empty(),
        "ancestor list must be empty when cwd is a git root"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_and_trigger_activation_loads_lazily() {
    let root = temp_workspace("skills_activation");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let explicit = catalog
        .activate_for_input("/skill rust-nav find main")
        .expect("activate");
    assert_eq!(explicit.task_input, "find main");
    assert_eq!(explicit.skills.len(), 1);
    assert_eq!(explicit.kinds, vec![SkillActivationKind::Explicit]);

    let trigger = catalog
        .activate_for_input("please inspect this Rust symbol")
        .expect("activate");
    assert_eq!(trigger.skills.len(), 1);
    assert_eq!(trigger.kinds, vec![SkillActivationKind::Trigger]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn unknown_explicit_skill_warns_and_preserves_task() {
    let root = temp_workspace("skills_unknown_explicit");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);

    let activation = catalog
        .activate_for_input("/skill rust-nva inspect main")
        .expect("unknown explicit skill must not abort activation");
    assert_eq!(activation.task_input, "inspect main");
    assert!(activation.skills.is_empty());
    assert!(activation.kinds.is_empty());
    assert_eq!(activation.warnings.len(), 1);
    assert_eq!(activation.warnings[0].name, "rust-nva");
    assert!(
        activation.warnings[0].message.contains("skill not found"),
        "{:?}",
        activation.warnings
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn disabled_explicit_skill_warns_and_preserves_task() {
    let root = temp_workspace("skills_disabled_explicit");
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        config: vec![SkillConfigEntry {
            name: Some("rust-nav".to_string()),
            enabled: false,
            ..Default::default()
        }],
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);

    let activation = catalog
        .activate_for_input("/skill rust-nav inspect main")
        .expect("disabled explicit skill must not abort activation");
    assert_eq!(activation.task_input, "inspect main");
    assert!(activation.skills.is_empty());
    assert!(activation.kinds.is_empty());
    assert_eq!(activation.warnings.len(), 1);
    assert_eq!(activation.warnings[0].name, "rust-nav");
    assert!(
        activation.warnings[0].message.contains("skill disabled"),
        "{:?}",
        activation.warnings
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trigger_skill_load_failure_warns_and_preserves_task() {
    let root = temp_workspace("skills_trigger_stale_file");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &["rust symbol"]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    fs::remove_file(skill_dir.join("SKILL.md")).expect("remove skill file after discovery");

    let activation = catalog
        .activate_for_input("please inspect this rust symbol")
        .expect("stale trigger skill must not abort activation");
    assert_eq!(activation.task_input, "please inspect this rust symbol");
    assert!(activation.skills.is_empty());
    assert!(activation.kinds.is_empty());
    assert_eq!(activation.warnings.len(), 1);
    assert_eq!(activation.warnings[0].name, "rust-nav");
    assert!(
        activation.warnings[0].message.contains("No such file")
            || activation.warnings[0].message.contains("os error"),
        "{:?}",
        activation.warnings
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn activation_kind_serializes_as_snake_case() {
    let cases = [
        (SkillActivationKind::Explicit, "\"explicit\""),
        (SkillActivationKind::Trigger, "\"trigger\""),
        (SkillActivationKind::ImplicitShell, "\"implicit_shell\""),
    ];
    for (kind, expected) in cases {
        let json = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(json, expected);
    }
}

#[test]
fn explicit_then_trigger_dedup_keeps_explicit_kind() {
    let root = temp_workspace("skills_activation_kind_dedup");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    // Input names the skill explicitly *and* the trigger phrase matches.
    // Dedup must keep the first occurrence (Explicit) so telemetry reports
    // the strongest signal, not the incidental trigger hit.
    let activation = catalog
        .activate_for_input("/skill rust-nav inspect this rust symbol")
        .expect("activate");
    assert_eq!(activation.skills.len(), 1);
    assert_eq!(activation.kinds, vec![SkillActivationKind::Explicit]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_activation_accepts_tab_after_slash_skill() {
    let root = temp_workspace("skills_slash_tab");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &[],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let activated = catalog
        .activate_for_input("/skill\trust-nav inspect main")
        .expect("activate");
    assert_eq!(activated.task_input, "inspect main");
    assert_eq!(activated.skills.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trigger_match_uses_word_boundaries() {
    let root = temp_workspace("skills_word_boundary");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust"],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let bare = catalog
        .activate_for_input("please use Rust here")
        .expect("activate");
    assert_eq!(bare.skills.len(), 1);

    let inside_word = catalog
        .activate_for_input("i trust this code")
        .expect("activate");
    assert!(inside_word.skills.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_skill_is_skipped_without_error() {
    let root = temp_workspace("skills_malformed");
    let dir = root.join(".squeezy/skills/broken");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(dir.join("SKILL.md"), "no frontmatter here\n").expect("write skill");
    write_skill(
        &root.join(".squeezy/skills/good"),
        "good",
        "Good skill",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let names: Vec<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert_eq!(names, vec!["good"]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn loaded_skill_body_is_cached_across_loads() {
    let root = temp_workspace("skills_cache");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let first = catalog.load("rust-nav").expect("load first");

    fs::remove_file(skill_dir.join("SKILL.md")).expect("remove skill file");

    let second = catalog.load("rust-nav").expect("load second from cache");
    assert_eq!(first.body, second.body);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_skill_render_respects_budget_and_uses_stub() {
    let root = temp_workspace("skills_active_budget");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill_with_body(
        &skill_dir,
        "rust-nav",
        "Rust navigation",
        &[],
        &"Use the graph carefully. ".repeat(200),
    );
    // Inline mode is the only render path that can produce a budget
    // stub; the metadata-only default never emits the skill body so
    // there's nothing to truncate.
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_chars: 700,
        active_body_cap_chars: 100,
        inline: true,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav inspect main")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");

    assert!(rendered.chars().count() <= config.active_budget_chars);
    assert!(rendered.contains("truncated=\"true\""));
    assert!(rendered.contains("load_skill"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_skills_default_to_metadata_only_render() {
    // Snapshot the active-skills block in the default (metadata-only)
    // render mode and assert: (a) every skill appears as a metadata
    // block, (b) no skill body leaks into the system prompt, (c) the
    // model is pointed at `load_skill` for each name. This is the
    // F03-skill-metadata-only-default contract: bodies are paid for
    // only when the model explicitly fetches them.
    let root = temp_workspace("skills_metadata_only_render");
    let skills = [
        (
            "alpha-nav",
            "Alpha skill description",
            "ALPHA_BODY_MARKER must never appear in the system prompt by default.",
        ),
        (
            "beta-nav",
            "Beta skill description",
            "BETA_BODY_MARKER must never appear in the system prompt by default.",
        ),
        (
            "gamma-nav",
            "Gamma skill description",
            "GAMMA_BODY_MARKER must never appear in the system prompt by default.",
        ),
    ];
    for (name, description, body) in &skills {
        write_skill_with_body(
            &root.join(".squeezy/skills").join(name),
            name,
            description,
            &[],
            body,
        );
    }

    // Default mode: `inline` is not set, so the catalog must emit
    // metadata-only blocks.
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_chars: 16_000,
        active_body_cap_chars: 16_000,
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    let loaded = skills
        .iter()
        .map(|(name, _, _)| catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let rendered = catalog
        .render_active_skills(&loaded)
        .expect("metadata-only render");

    // Outer wrapper and per-skill name attributes are present.
    assert!(
        rendered.starts_with("<active_skills>"),
        "missing <active_skills> wrapper: {rendered}"
    );
    assert!(
        rendered.ends_with("</active_skills>"),
        "missing </active_skills> wrapper: {rendered}"
    );
    for (name, description, body) in &skills {
        assert!(
            rendered.contains(&format!("name=\"{name}\"")),
            "missing skill metadata for {name}: {rendered}"
        );
        assert!(
            rendered.contains(&format!("<description>{description}</description>")),
            "missing description for {name}: {rendered}"
        );
        // The body must NOT appear; the model is expected to fetch it
        // via the `load_skill` tool when needed.
        assert!(
            !rendered.contains(body),
            "body marker for {name} leaked into metadata-only render: {rendered}"
        );
        // Instruction text references the same skill name (escaped by
        // `xml_escape`, so quotes become `&quot;`).
        assert!(
            rendered.contains(&format!("name &quot;{name}&quot;")),
            "missing load_skill instruction for {name}: {rendered}"
        );
    }
    // The metadata mode marker keeps the body explicitly absent rather
    // than relying on a truncation reason borrowed from the inline path.
    assert!(
        rendered.contains("body=\"omitted\""),
        "metadata-only mode must flag bodies as omitted: {rendered}"
    );
    assert!(
        !rendered.contains("<content>"),
        "metadata-only mode must not emit any <content> body slot: {rendered}"
    );
    assert!(
        rendered.contains("load_skill"),
        "metadata-only mode must instruct the model to call load_skill: {rendered}"
    );

    // Flip the knob: with `[skills] inline = true` the legacy render
    // must re-inline each body verbatim.
    let inline_config = SkillsConfig {
        inline: true,
        ..config.clone()
    };
    let inline_catalog = SkillCatalog::discover(&root, &inline_config);
    let inline_loaded = skills
        .iter()
        .map(|(name, _, _)| inline_catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let inline_rendered = inline_catalog
        .render_active_skills(&inline_loaded)
        .expect("inline render");
    for (_, _, body) in &skills {
        assert!(
            inline_rendered.contains(body),
            "inline mode must keep injecting bodies: {inline_rendered}"
        );
    }
    assert!(
        inline_rendered.contains("<content>"),
        "inline mode must keep emitting <content>: {inline_rendered}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_skill_render_redistributes_descriptions_to_preserve_roster() {
    // Three skills, each with body well above the body cap so every skill
    // starts as a stub. Budget is sized so that the full-description stubs
    // overflow the active block but the minimum-line stubs all fit — the
    // redistribute step must keep every skill present and give each a
    // non-empty description.
    let root = temp_workspace("skills_active_redistribute");
    let names = ["alpha-skill", "beta-skill", "gamma-skill"];
    let descriptions = [
        "Alpha description that is long enough to occupy several stub characters when budget allows.",
        "Beta description, also reasonably long so we can verify the redistribute loop allocates across skills.",
        "Gamma description with comparable length, exercising the third allocation slot of the loop.",
    ];
    for (name, description) in names.iter().zip(descriptions.iter()) {
        write_skill_with_body(
            &root.join(".squeezy/skills").join(name),
            name,
            description,
            &[],
            &"body line that exceeds the cap. ".repeat(60),
        );
    }
    // Compute the minimum-stub floor at runtime so the test budget is robust
    // against temp-path length variation across hosts. The full-description
    // aggregate is the floor plus the description chars themselves — sit
    // between the two so the redistribute step is required. The
    // redistribute path is inline-mode only; the metadata-only default
    // never falls back to per-skill description budgeting.
    let catalog = SkillCatalog::discover(
        &root,
        &SkillsConfig {
            user_dir: root.join("user"),
            compat_user_dir: root.join("compat"),
            active_budget_chars: usize::MAX,
            active_body_cap_chars: 100,
            inline: true,
            ..Default::default()
        },
    );
    let loaded = names
        .iter()
        .map(|name| catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let full_block = render::render_active_skills(&loaded, usize::MAX, 100)
        .expect("baseline render with unbounded budget");
    // Each description is ASCII without XML-special chars so xml_escape is
    // identity; subtracting the description-char sum from the full render
    // yields the minimum-stub floor (description text removed, structure
    // kept).
    let desc_payload_chars: usize = descriptions
        .iter()
        .map(|description| description.chars().count())
        .sum();
    let floor = full_block.chars().count() - desc_payload_chars;
    // Pick a budget strictly between the floor and the full aggregate so the
    // redistribute branch — not the all-fits-as-is branch and not the
    // drop-skills branch — is the one exercised.
    let active_budget_chars = floor + desc_payload_chars / 2;

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_chars,
        active_body_cap_chars: 100,
        inline: true,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let loaded = names
        .iter()
        .map(|name| catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let rendered = catalog
        .render_active_skills(&loaded)
        .expect("render active skills");

    assert!(rendered.chars().count() <= config.active_budget_chars);
    for name in &names {
        let needle = format!("name=\"{name}\"");
        assert!(
            rendered.contains(&needle),
            "redistribute must keep every skill present; missing {name}\n---\n{rendered}\n---"
        );
    }
    // No skill should have a completely empty description tag — the loop
    // must allocate at least one char to each skill when the budget allows
    // it. Empty `<description></description>` indicates the skill was kept
    // at the minimum-stub floor.
    assert!(
        !rendered.contains("<description></description>"),
        "redistribute must allocate at least one description char per skill\n---\n{rendered}\n---"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn available_skills_preamble_respects_budget() {
    let root = temp_workspace("skills_preamble_budget");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        preamble_budget_chars: 1_200,
        ..Default::default()
    };
    for name in ["alpha-nav", "beta-nav", "gamma-nav"] {
        write_skill(
            &root.join(".squeezy/skills").join(name),
            name,
            "A deliberately long description that should force the available skills preamble to omit at least one skill when the budget is tight",
            &[],
        );
    }

    let catalog = SkillCatalog::discover(&root, &config);
    let preamble = catalog.render_preamble().expect("preamble");

    assert!(preamble.body.chars().count() <= config.preamble_budget_chars);
    assert!(preamble.omitted_count > 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn disabled_skill_config_filters_activation_and_load() {
    let root = temp_workspace("skills_disabled_name");
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        config: vec![SkillConfigEntry {
            name: Some("rust-nav".to_string()),
            enabled: false,
            ..Default::default()
        }],
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.disabled);
    assert!(
        catalog
            .activate_for_input("please inspect this Rust symbol")
            .expect("activate")
            .skills
            .is_empty()
    );
    let error = catalog.load("rust-nav").expect_err("disabled load");
    assert!(error.to_string().contains("skill disabled"));
    assert_eq!(catalog.summaries_json()["skills"][0]["disabled"], true);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn path_config_can_reenable_after_name_disable() {
    let root = temp_workspace("skills_path_reenable");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        config: vec![
            SkillConfigEntry {
                name: Some("rust-nav".to_string()),
                enabled: false,
                ..Default::default()
            },
            SkillConfigEntry {
                path: Some(PathBuf::from(".squeezy/skills/rust-nav")),
                enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    assert!(!catalog.summaries().pop().expect("summary").disabled);
    catalog.load("rust-nav").expect("enabled by path");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_precedence_collision_skips_trigger_activation() {
    let root = temp_workspace("skills_collision");
    write_skill(
        &root.join(".squeezy/skills/first"),
        "rust-nav",
        "First Rust nav",
        &["rust symbol"],
    );
    write_skill(
        &root.join(".squeezy/skills/second"),
        "rust-nav",
        "Second Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    assert!(catalog.ambiguous_names().contains("rust-nav"));
    assert!(
        catalog
            .activate_for_input("please inspect this Rust symbol")
            .expect("activate")
            .skills
            .is_empty()
    );
    assert_eq!(
        catalog
            .activate_for_input("/skill rust-nav inspect")
            .expect("activate")
            .skills
            .len(),
        1
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn detects_implicit_skill_from_script_and_doc_read() {
    let root = temp_workspace("skills_implicit");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let scripts = skill_dir.join("scripts");
    fs::create_dir_all(&scripts).expect("mkdir scripts");
    fs::write(scripts.join("init.py"), "print('ok')\n").expect("write script");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let script = catalog
        .detect_for_command("python .squeezy/skills/rust-nav/scripts/init.py", &root)
        .expect("script detection");
    let doc = catalog
        .detect_for_command("cat .squeezy/skills/rust-nav/SKILL.md", &root)
        .expect("doc detection");

    assert_eq!(script.name, "rust-nav");
    assert_eq!(doc.name, "rust-nav");

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn detects_implicit_skill_doc_read_through_symlinked_skill_file() {
    use std::os::unix::fs::symlink;

    let root = temp_workspace("skills_implicit_symlink");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    let target_dir = root.join("canonical");
    fs::create_dir_all(&target_dir).expect("mkdir canonical");
    let target = target_dir.join("rust-nav.md");
    fs::write(
        &target,
        "---\nname: rust-nav\ndescription: Rust nav\ntriggers:\n\n---\n# rust-nav\n",
    )
    .expect("write target skill");
    symlink(&target, skill_dir.join("SKILL.md")).expect("symlink skill");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let doc = catalog
        .detect_for_command("cat .squeezy/skills/rust-nav/SKILL.md", &root)
        .expect("doc detection through symlink");

    assert_eq!(doc.name, "rust-nav");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_source_serializes_as_snake_case() {
    let json = serde_json::to_string(&SkillSource::CompatProject).expect("serialize");
    assert_eq!(json, "\"compat_project\"");
    let json = serde_json::to_string(&SkillSource::Project).expect("serialize");
    assert_eq!(json, "\"project\"");
    let json = serde_json::to_string(&SkillSource::User).expect("serialize");
    assert_eq!(json, "\"user\"");
    let json = serde_json::to_string(&SkillSource::CompatUser).expect("serialize");
    assert_eq!(json, "\"compat_user\"");
}

#[test]
fn prompt_block_escapes_metadata_and_breakouts() {
    let skill = LoadedSkill {
        summary: SkillSummary {
            name: "rust-nav".to_string(),
            description: "uses </skill> tag & <code>".to_string(),
            when_to_use: Some("look for <foo>".to_string()),
            source: SkillSource::Project,
            location: PathBuf::from("/tmp/SKILL.md"),
            disabled: false,
            manifest: None,
            context_mode: SkillContextMode::Inline,
        },
        base_dir: PathBuf::from("/tmp"),
        body: "Body with </content> and </skill> markers.".to_string(),
        hooks: BTreeMap::new(),
    };

    let block = skill.prompt_block();
    assert!(block.contains("&lt;/skill&gt;"));
    assert!(block.contains("&amp;"));
    assert!(block.contains("&lt;foo&gt;"));
    assert!(!block.contains("uses </skill>"));
    assert!(block.contains("<\\/content>"));
    assert!(block.contains("<\\/skill>"));
}

#[test]
fn discover_applies_context_percent_budget_to_catalog() {
    let root = temp_workspace("skills_budget_mode_discover");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill_with_body(
        &skill_dir,
        "rust-nav",
        "Rust nav",
        &[],
        // Pad the body so the active bundle would exceed a small budget
        // and force the catalog to fall back to a stub. That confirms the
        // discover-time budget actually drives render-time decisions.
        &"x".repeat(20_000),
    );
    // 32K-token model gets a 2_560-char active budget; the 20K-char body
    // can't fit so the catalog should emit a stub. The inline-mode opt
    // in keeps this test exercising the legacy body+stub path; the
    // metadata-only default never emits the body in the first place.
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_body_cap_chars: 64_000,
        active_budget_mode: SkillsBudgetMode::ContextPercent { percent: 2.0 },
        model_context_window: Some(32_000),
        inline: true,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav do something")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");
    assert!(rendered.chars().count() <= 2_560);
    assert!(rendered.contains("truncated=\"true\""));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discover_chars_mode_pins_catalog_budget_regardless_of_window() {
    let root = temp_workspace("skills_budget_mode_chars");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_mode: SkillsBudgetMode::Chars { chars: 8_000 },
        preamble_budget_mode: SkillsBudgetMode::Chars { chars: 8_000 },
        model_context_window: Some(200_000),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav do something")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");
    // The bundle is small but the budget must still be the explicit 8_000
    // cap, not 2% of the 200K-token window (which would be 16_000).
    assert!(rendered.chars().count() <= 8_000);

    let _ = fs::remove_dir_all(root);
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

thread_local! {
    /// Per-thread capture buffer the global subscriber writes into while a
    /// `capture_discover_logs` call is active on this thread.
    static CAPTURE_BUFFER: std::cell::RefCell<Option<Arc<Mutex<Vec<u8>>>>> =
        const { std::cell::RefCell::new(None) };
}

static LOG_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// Routes each event to the calling thread's [`CAPTURE_BUFFER`], or drops it
/// when none is installed. Cloned per `make_writer`, so it is cheap.
#[derive(Clone, Default)]
struct ThreadLocalLogWriter;

impl<'writer> MakeWriter<'writer> for ThreadLocalLogWriter {
    type Writer = ThreadLocalLogWrite;

    fn make_writer(&'writer self) -> Self::Writer {
        ThreadLocalLogWrite
    }
}

struct ThreadLocalLogWrite;

impl io::Write for ThreadLocalLogWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        CAPTURE_BUFFER.with(|cell| {
            if let Some(buffer) = cell.borrow().as_ref() {
                buffer.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A single process-global subscriber backs every capture. Installing it once
/// (rather than per-call `with_default`) keeps the warning callsites' tracing
/// interest live: a parallel test calling `discover` without a subscriber
/// would otherwise be the first to hit a callsite, cache its interest as
/// `never`, and silently suppress it for the capturing thread.
fn install_capture_subscriber() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(ThreadLocalLogWriter)
            .with_max_level(tracing::Level::WARN)
            .finish();
        // Ignore an error: another crate's test harness may already own the
        // global default. The thread-local routing only needs *a* live
        // subscriber so callsite interest stays re-evaluated.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn capture_discover_logs(workspace: &Path, config: &SkillsConfig) -> (SkillCatalog, String) {
    install_capture_subscriber();
    let _guard = LOG_CAPTURE_LOCK.lock().expect("log capture lock");
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    CAPTURE_BUFFER.with(|cell| *cell.borrow_mut() = Some(buffer.clone()));
    let catalog = SkillCatalog::discover(workspace, config);
    CAPTURE_BUFFER.with(|cell| *cell.borrow_mut() = None);
    let bytes = buffer.lock().expect("log buffer").clone();
    (catalog, String::from_utf8(bytes).expect("logs are UTF-8"))
}

#[test]
fn same_precedence_name_collision_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_same_precedence_name");
    write_skill(
        &root.join(".squeezy/skills/first"),
        "rust-nav",
        "First Rust nav",
        &[],
    );
    write_skill(
        &root.join(".squeezy/skills/second"),
        "rust-nav",
        "Second Rust nav",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    assert!(catalog.ambiguous_names().contains("rust-nav"));
    assert!(
        logs.contains("same-precedence skill name collision"),
        "missing same-precedence warning: {logs}"
    );
    assert!(
        logs.contains("name=rust-nav"),
        "missing skill name field: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cross_precedence_name_collision_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_cross_precedence_name");
    write_skill(
        &root.join(".agents/skills/rust-nav"),
        "rust-nav",
        "Compat project Rust nav",
        &[],
    );
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Native project Rust nav",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    // The native-project copy wins; the compat copy is shadowed.
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.source, SkillSource::Project);
    assert!(
        logs.contains("skill name reused at higher precedence"),
        "missing cross-precedence warning: {logs}"
    );
    assert!(
        logs.contains("overriding_source=\"project\"")
            || logs.contains("overriding_source=project"),
        "missing overriding source field: {logs}"
    );
    assert!(
        logs.contains("overridden_source=\"compat_project\"")
            || logs.contains("overridden_source=compat_project"),
        "missing overridden source field: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn duplicate_trigger_across_skills_skips_auto_activation() {
    let root = temp_workspace("skills_trigger_collision_skip");
    write_skill(
        &root.join(".squeezy/skills/alpha"),
        "alpha",
        "Alpha skill",
        &["graph"],
    );
    write_skill(
        &root.join(".squeezy/skills/beta"),
        "beta",
        "Beta skill",
        &["graph"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);

    assert!(
        catalog.ambiguous_triggers().contains("graph"),
        "discovery must mark a cross-skill duplicate trigger as ambiguous"
    );

    let activation = catalog
        .activate_for_input("please inspect the graph")
        .expect("activate");
    assert!(
        activation.skills.is_empty(),
        "ambiguous trigger must not auto-activate either skill, got {:?}",
        activation
            .skills
            .iter()
            .map(|s| &s.summary.name)
            .collect::<Vec<_>>()
    );

    // Explicit `/skill <name>` still selects the requested skill.
    let explicit = catalog
        .activate_for_input("/skill alpha use the graph")
        .expect("activate explicit");
    assert_eq!(explicit.skills.len(), 1);
    assert_eq!(explicit.skills[0].summary.name, "alpha");
    assert_eq!(explicit.task_input, "use the graph");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn duplicate_trigger_across_skills_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_trigger_collision");
    write_skill(
        &root.join(".squeezy/skills/alpha"),
        "alpha",
        "Alpha skill",
        &["graph"],
    );
    write_skill(
        &root.join(".squeezy/skills/beta"),
        "beta",
        "Beta skill",
        &["GRAPH"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (_catalog, logs) = capture_discover_logs(&root, &config);
    assert!(
        logs.contains("duplicate skill trigger"),
        "missing trigger collision warning: {logs}"
    );
    assert!(
        logs.contains("trigger=graph"),
        "missing trigger field (case-folded): {logs}"
    );
    assert!(
        logs.contains("\"alpha\"") && logs.contains("\"beta\""),
        "missing colliding skill names: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trigger_warning_skips_single_skill_with_repeated_trigger() {
    let root = temp_workspace("skills_warn_no_self_trigger");
    write_skill(
        &root.join(".squeezy/skills/alpha"),
        "alpha",
        "Alpha skill",
        &["graph", "GRAPH"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (_catalog, logs) = capture_discover_logs(&root, &config);
    assert!(
        !logs.contains("duplicate skill trigger"),
        "trigger collision must require two distinct skills: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn xdg_user_dir_skills_are_discovered() {
    let root = lib_tests_tempdir("xdg_skill_discover");
    let xdg_skills = root.join("xdg_data").join("squeezy").join("skills");
    let xdg_skill_dir = xdg_skills.join("xdg-nav");
    fs::create_dir_all(&xdg_skill_dir).expect("create xdg skill dir");
    fs::write(
        xdg_skill_dir.join("SKILL.md"),
        "---\nname: xdg-nav\ndescription: \"XDG skill\"\ntriggers: [\"xdg nav\"]\n---\n# xdg-nav\n",
    )
    .expect("write xdg skill");

    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        xdg_user_dir: Some(xdg_skills),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    assert!(
        catalog.skills.contains_key("xdg-nav"),
        "xdg-nav skill should be discovered via xdg_user_dir"
    );
    let _ = fs::remove_dir_all(root);
}

/// When `xdg_user_dir` is `None`, discovery does not panic and works normally.
#[test]
fn discover_works_without_xdg_user_dir() {
    let root = lib_tests_tempdir("xdg_none_discover");
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        xdg_user_dir: None,
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    assert!(catalog.summaries().is_empty());
    let _ = fs::remove_dir_all(root);
}

/// Legacy user_dir skills take precedence over same-name XDG skills.
#[test]
fn legacy_user_dir_shadows_xdg_same_name_skill() {
    let root = lib_tests_tempdir("xdg_shadow_test");

    let user_skill = root.join("user").join("my-skill");
    fs::create_dir_all(&user_skill).expect("create user skill");
    fs::write(
        user_skill.join("SKILL.md"),
        "---\nname: my-skill\ndescription: \"from user\"\ntriggers: [\"my skill\"]\n---\n# user\n",
    )
    .expect("write user skill");

    let xdg_skills = root.join("xdg").join("squeezy").join("skills");
    let xdg_skill = xdg_skills.join("my-skill");
    fs::create_dir_all(&xdg_skill).expect("create xdg skill");
    fs::write(
        xdg_skill.join("SKILL.md"),
        "---\nname: my-skill\ndescription: \"from xdg\"\ntriggers: [\"my skill\"]\n---\n# xdg\n",
    )
    .expect("write xdg skill");

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat-noop"),
        xdg_user_dir: Some(xdg_skills),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    let skill = catalog.skills.get("my-skill").expect("skill present");
    assert_eq!(
        skill.summary.description, "from user",
        "legacy user_dir should shadow xdg for same-name skill"
    );
    let _ = fs::remove_dir_all(root);
}

fn lib_tests_tempdir(name: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("squeezy-{name}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
