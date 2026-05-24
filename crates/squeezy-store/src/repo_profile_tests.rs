use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{GraphConfig, repo_settings_id};

use super::*;

#[test]
fn repo_profile_detects_mixed_language_repo_and_commands() {
    let root = temp_root("mixed_language_profile");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"scripts":{"build":"vite build","test":"vitest","lint":"eslint ."}}"#,
    )
    .unwrap();
    fs::write(root.join("pnpm-lock.yaml"), "\n").unwrap();
    fs::write(root.join("src/app.ts"), "export const value = 1;\n").unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
    fs::write(
        root.join("tests/test_app.py"),
        "def test_ok():\n    assert True\n",
    )
    .unwrap();

    let profile = RepoProfile::detect(&root, &GraphConfig::default()).unwrap();

    assert!(
        profile
            .languages
            .iter()
            .any(|language| language.name == "Rust" && language.files == 1)
    );
    assert!(
        profile
            .languages
            .iter()
            .any(|language| language.name == "TypeScript" && language.files == 1)
    );
    assert!(
        profile
            .package_managers
            .iter()
            .any(|manager| manager.name == "cargo")
    );
    assert!(
        profile
            .package_managers
            .iter()
            .any(|manager| manager.name == "pnpm")
    );
    assert!(
        profile
            .commands
            .iter()
            .any(|command| command.kind == "test" && command.ambiguous)
    );
}

#[test]
fn missing_build_commands_are_reported_as_low_confidence_ambiguous_fallbacks() {
    let root = temp_root("ambiguous_missing_build_commands");
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
    fs::write(root.join("main.py"), "print('hi')\n").unwrap();

    let profile = RepoProfile::detect(&root, &GraphConfig::default()).unwrap();

    assert!(profile.commands.iter().any(|command| command.kind == "test"
        && command.command == "python -m pytest"
        && command.confidence == "low"));
    assert!(
        !profile
            .commands
            .iter()
            .any(|command| command.kind == "build")
    );
}

#[test]
fn ensure_repo_profile_reuses_unchanged_light_fingerprint() {
    let root = temp_root("repo_profile_reuse");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    let registry_path = root.join("repos.toml");

    let first = ensure_repo_profile_at(&registry_path, &root, &GraphConfig::default()).unwrap();
    let second = ensure_repo_profile_at(&registry_path, &root, &GraphConfig::default()).unwrap();

    assert_eq!(first.status, RepoProfileStatus::Created);
    assert_eq!(second.status, RepoProfileStatus::Reused);
    assert!(registry_path.exists());
}

#[test]
fn registry_round_trip_preserves_optional_git_and_language_fields() {
    // Guards against a regression where the hand-written TOML writer wrote
    // `""` (or `false`) for `Option<_>` `None` values and the loader then
    // produced `Some("")`/`Some(false)`, drifting away from a freshly
    // detected profile.
    let root = temp_root("repo_profile_roundtrip");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let registry_path = root.join("repos.toml");
    let first = ensure_repo_profile_at(&registry_path, &root, &GraphConfig::default()).unwrap();
    let reloaded = RepoRegistry::load(&registry_path).unwrap();
    let loaded = reloaded
        .profile_for_root(&fs::canonicalize(&root).unwrap())
        .expect("profile present");

    assert_eq!(loaded.git, first.profile.git);
    assert!(loaded.git.vcs_type.is_none());
    assert!(loaded.git.dirty.is_none());
    for language in &loaded.languages {
        let original = first
            .profile
            .languages
            .iter()
            .find(|candidate| candidate.name == language.name)
            .expect("language present");
        assert_eq!(language.family, original.family);
    }
}

#[test]
fn generated_and_vendor_coverage_appears_in_profile() {
    let root = temp_root("repo_profile_ignored_coverage");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor/lib")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    fs::write(
        root.join("src/generated.rs"),
        "// @generated\npub fn g() {}\n",
    )
    .unwrap();
    fs::write(root.join("vendor/lib/lib.rs"), "pub fn vendored() {}\n").unwrap();

    let profile = RepoProfile::detect(&root, &GraphConfig::default()).unwrap();

    assert!(
        profile
            .ignored_paths
            .iter()
            .any(|ignored| ignored.reason == "generated")
    );
    assert!(
        profile
            .ignored_paths
            .iter()
            .any(|ignored| ignored.reason == "vendor")
    );
}

#[test]
fn registry_round_trip_preserves_repo_id_and_refreshes_legacy_profiles() {
    // Guards the per-repo settings handshake: each persisted profile carries
    // the stable repo id used to locate
    // ~/.squeezy/projects/<repo-id>/settings.toml, and profiles written
    // before the field existed must be regenerated rather than reused with
    // an empty id.
    let root = temp_root("repo_profile_repo_id_roundtrip");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let registry_path = root.join("repos.toml");
    let first = ensure_repo_profile_at(&registry_path, &root, &GraphConfig::default()).unwrap();
    let expected_id = repo_settings_id(&root);
    assert!(!expected_id.is_empty());
    assert_eq!(first.profile.repo_id, expected_id);

    let reloaded = RepoRegistry::load(&registry_path).unwrap();
    let canonical_root = fs::canonicalize(&root).unwrap();
    let loaded = reloaded
        .profile_for_root(&canonical_root)
        .expect("profile present");
    assert_eq!(loaded.repo_id, expected_id);

    let mut legacy = reloaded;
    let mut legacy_profile = legacy
        .profile_for_root(&canonical_root)
        .expect("profile present")
        .clone();
    legacy_profile.repo_id.clear();
    legacy.upsert(legacy_profile);
    legacy.save(&registry_path).unwrap();

    let refreshed = ensure_repo_profile_at(&registry_path, &root, &GraphConfig::default()).unwrap();
    assert_eq!(refreshed.status, RepoProfileStatus::Refreshed);
    assert_eq!(refreshed.profile.repo_id, expected_id);
}

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-store-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
