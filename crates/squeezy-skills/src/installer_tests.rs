use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::SkillsConfig;

use super::*;
use crate::SkillCatalog;

#[test]
fn bundled_skills_load_with_valid_metadata() {
    let bundled = super::bundled_skills();
    assert!(
        bundled.len() >= 2,
        "expected at least two bundled samples, got {}",
        bundled.len()
    );
    let mut seen = BTreeSet::new();
    for skill in &bundled {
        assert!(
            is_valid_skill_name(&skill.summary.name),
            "bundled skill name {} must satisfy SKILL.md naming rules",
            skill.summary.name
        );
        assert!(
            !skill.summary.description.trim().is_empty(),
            "bundled skill {} must have a non-empty description",
            skill.summary.name
        );
        assert!(
            !skill.body.trim().is_empty(),
            "bundled skill {} must have a non-empty body",
            skill.summary.name
        );
        assert!(
            seen.insert(skill.summary.name.clone()),
            "duplicate bundled skill name: {}",
            skill.summary.name
        );
        assert!(
            skill.base_dir.starts_with("<squeezy-builtin>"),
            "bundled skill {} base_dir must be the in-binary sentinel root, got {}",
            skill.summary.name,
            skill.base_dir.display()
        );
    }
}

#[test]
fn customize_skill_loads_when_user_edits_config_toml() {
    // Acceptance test for the bundled `customize-squeezy` skill: it must
    // be present in the in-binary sample list, and when installed into a
    // discovered skills root it must activate on the kind of input a user
    // gives when they ask to edit `settings.toml` / `squeezy.toml`.
    let bundled = super::bundled_skills();
    let customize = bundled
        .iter()
        .find(|skill| skill.summary.name == "customize-squeezy")
        .expect("customize-squeezy bundled skill is registered");
    assert!(
        customize
            .body
            .to_ascii_lowercase()
            .contains("settings.toml"),
        "bundled customize-squeezy body should document settings.toml edits"
    );

    let root = temp_workspace("skills_customize_squeezy_loads");
    let user_dir = root.join("user");
    let skill_dir = user_dir.join("customize-squeezy");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        include_str!("../builtin/customize-squeezy/SKILL.md"),
    )
    .expect("install bundled skill");

    let config = SkillsConfig {
        user_dir,
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("please edit my ~/.squeezy/settings.toml to add a provider")
        .expect("activate");
    assert!(
        activation
            .skills
            .iter()
            .any(|loaded| loaded.summary.name == "customize-squeezy"),
        "expected customize-squeezy to activate on a settings.toml edit prompt, got {:?}",
        activation
            .skills
            .iter()
            .map(|loaded| loaded.summary.name.as_str())
            .collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn bundled_skills_render_through_catalog_helpers() {
    let bundled = super::bundled_skills();
    let block = bundled
        .first()
        .expect("at least one bundled skill")
        .prompt_block();
    assert!(block.contains("<skill name=\""));
    assert!(block.contains("<content>"));
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

#[test]
fn install_bundled_skills_is_idempotent_and_skips_existing() {
    let root = temp_workspace("skills_install_bundled");
    let user_dir = root.join("user");

    let first = super::install_bundled_skills(&user_dir).expect("install first");
    assert!(
        !first.is_empty(),
        "first install must write at least one bundled skill"
    );
    let names_first: BTreeSet<String> = first.into_iter().collect();
    let expected: BTreeSet<String> = [
        "customize-squeezy",
        "release-notes",
        "skill-creator",
        "trace-symbol",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(names_first, expected);

    for name in &expected {
        let path = user_dir.join(name).join("SKILL.md");
        assert!(path.exists(), "missing installed skill: {}", path.display());
        let body = fs::read_to_string(&path).expect("read installed");
        assert!(
            body.contains(&format!("name: {name}")),
            "frontmatter must declare the skill name verbatim: {body}"
        );
    }

    // Second invocation is a no-op even if the user has hand-edited
    // a SKILL.md — the installer must never clobber existing files.
    let user_edit = user_dir.join("customize-squeezy").join("SKILL.md");
    fs::write(
        &user_edit,
        "---\nname: customize-squeezy\ndescription: \"mine\"\n---\nedited\n",
    )
    .expect("simulate user edit");
    let second = super::install_bundled_skills(&user_dir).expect("install second");
    assert!(
        second.is_empty(),
        "second install must not rewrite any skill"
    );
    let preserved = fs::read_to_string(&user_edit).expect("read after second install");
    assert!(
        preserved.contains("edited"),
        "user edits must survive a re-install: {preserved}"
    );

    let _ = fs::remove_dir_all(root);
}
