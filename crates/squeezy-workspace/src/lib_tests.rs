use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

#[test]
fn crawler_respects_gitignore_and_classifies_files() {
    let root = temp_root("crawler_respects_gitignore_and_classifies_files");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join("src").join("lib.rs"), "fn a(){}\n").unwrap();
    fs::write(root.join("ignored.rs"), "pub fn hidden() {}\n").unwrap();
    fs::write(root.join("README.md"), "# docs\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    let paths = snapshot
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"src/lib.rs"));
    assert!(paths.contains(&"README.md"));
    assert!(!paths.contains(&"ignored.rs"));
    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == "src/lib.rs")
            .unwrap()
            .language,
        LanguageKind::Rust
    );
    assert_eq!(snapshot.unsupported.len(), 1);
    assert_eq!(
        snapshot.unsupported[0].reason,
        UnsupportedReason::UnsupportedExtension
    );
}

#[test]
fn stable_content_hash_is_stable() {
    assert_eq!(stable_content_hash(b"same"), stable_content_hash(b"same"));
    assert_ne!(
        stable_content_hash(b"same"),
        stable_content_hash(b"different")
    );
}

#[test]
fn crawler_marks_large_and_binary_files_as_unsupported() {
    let root = temp_root("crawler_marks_large_and_binary_files_as_unsupported");
    fs::write(root.join("large.rs"), "pub fn large() {}\n").unwrap();
    fs::write(root.join("bytes.bin"), b"abc\0def").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        include_hidden: false,
        max_file_bytes: 8,
        require_indexing_signal: true,
        policy: IndexingPolicy::default(),
    })
    .crawl(&root)
    .unwrap();

    assert!(snapshot.files.is_empty());
    assert!(snapshot.excluded.iter().any(|file| {
        file.relative_path == "large.rs" && file.reason == ExclusionReason::LargeFile
    }));
    assert!(snapshot.excluded.iter().any(|file| {
        file.relative_path == "bytes.bin" && file.reason == ExclusionReason::Binary
    }));
}

#[test]
fn default_policy_excludes_generated_vendor_build_lock_binary_and_large_paths() {
    let root =
        temp_root("default_policy_excludes_generated_vendor_build_lock_binary_and_large_paths");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(root.join("vendor/lib")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::create_dir_all(root.join(".venv/lib")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "fn a(){}\n").unwrap();
    fs::write(root.join("src").join("generated.rs"), "// @generated\n").unwrap();
    fs::write(root.join("node_modules/pkg/index.ts"), "needle\n").unwrap();
    fs::write(root.join("vendor/lib/lib.rs"), "pub fn vendored() {}\n").unwrap();
    fs::write(
        root.join("target/debug/out.rs"),
        "pub fn build_output() {}\n",
    )
    .unwrap();
    fs::write(root.join(".venv/lib/site.py"), "def hidden(): pass\n").unwrap();
    fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    fs::write(root.join("image.png"), b"\x89PNG\r\n\0").unwrap();
    fs::write(
        root.join("huge.rs"),
        "pub fn huge() { let x = 1 + 2 + 3; }\n",
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        max_file_bytes: 16,
        ..CrawlOptions::default()
    })
    .crawl(&root)
    .unwrap();

    assert!(
        snapshot
            .files
            .iter()
            .any(|file| file.relative_path == "src/lib.rs")
    );
    for (path, reason) in [
        ("src/generated.rs", ExclusionReason::Generated),
        (
            "node_modules/pkg/index.ts",
            ExclusionReason::DependencyCache,
        ),
        ("vendor/lib/lib.rs", ExclusionReason::Vendor),
        ("target/debug/out.rs", ExclusionReason::BuildOutput),
        (".venv/lib/site.py", ExclusionReason::DependencyCache),
        ("Cargo.lock", ExclusionReason::Lockfile),
        ("image.png", ExclusionReason::Binary),
        ("huge.rs", ExclusionReason::LargeFile),
    ] {
        assert!(
            snapshot
                .excluded
                .iter()
                .any(|file| file.relative_path == path && file.reason == reason),
            "{path} should be excluded as {reason:?}: {:?}",
            snapshot.excluded
        );
    }
    assert!(snapshot.coverage.skipped_files >= 8);
    assert!(
        snapshot
            .coverage
            .reasons
            .contains_key(ExclusionReason::Vendor.as_str())
    );
}

#[test]
fn policy_include_and_exclude_overrides_are_reason_tagged() {
    let root = temp_root("policy_include_and_exclude_overrides_are_reason_tagged");
    fs::create_dir_all(root.join("vendor/allowed")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("vendor/allowed/lib.rs"), "pub fn allowed() {}\n").unwrap();
    fs::write(root.join("src/private.rs"), "pub fn private() {}\n").unwrap();
    fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        policy: IndexingPolicy {
            include: vec!["vendor/allowed/**".to_string()],
            exclude: vec!["src/private.rs".to_string()],
            include_classes: vec!["lockfile".to_string()],
            exclude_classes: Vec::new(),
        },
        ..CrawlOptions::default()
    })
    .crawl(&root)
    .unwrap();

    let indexed = snapshot
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert!(indexed.contains(&"vendor/allowed/lib.rs"));
    assert!(indexed.contains(&"Cargo.lock"));
    assert!(snapshot.excluded.iter().any(|file| {
        file.relative_path == "src/private.rs" && file.reason == ExclusionReason::UserExclude
    }));
}

#[test]
fn indexing_policy_covers_common_language_layouts() {
    for (dir, file) in [
        ("rust/target/debug", "lib.rs"),
        ("python/.venv/lib", "site.py"),
        ("js/node_modules/pkg", "index.ts"),
        ("go/vendor/pkg", "lib.go"),
        ("java/build/classes", "App.java"),
        ("csharp/bin/Debug", "Program.cs"),
        ("cpp/build/obj", "main.cpp"),
    ] {
        let root = temp_root(&format!("layout-{}-{}", dir.replace(['/', '.'], "_"), file));
        let excluded_dir = root.join(dir);
        fs::create_dir_all(&excluded_dir).unwrap();
        fs::write(root.join("README.md"), "# project\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(excluded_dir.join(file), "needle\n").unwrap();

        let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
            .crawl(&root)
            .unwrap();

        let excluded_path = format!("{dir}/{file}");
        assert!(
            snapshot
                .excluded
                .iter()
                .any(|entry| entry.relative_path == excluded_path),
            "{excluded_path}: {:?}",
            snapshot.excluded
        );
    }
}

#[test]
fn crawler_classifies_python_files() {
    let root = temp_root("crawler_classifies_python_files");
    fs::write(root.join("main.py"), "def main():\n    pass\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == "main.py")
            .unwrap()
            .language,
        LanguageKind::Python
    );
}

#[test]
fn crawler_skips_roots_without_code_signals() {
    let root = temp_root("crawler_skips_roots_without_code_signals");
    fs::write(root.join("notes.txt"), "plain notes\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(!snapshot.indexing_decision.should_index);
    assert!(snapshot.files.is_empty());
}

#[test]
fn readme_alone_is_only_a_weak_signal_for_non_git_roots() {
    let root = temp_root("readme_alone_is_only_a_weak_signal_for_non_git_roots");
    fs::write(root.join("README.md"), "# project\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(!snapshot.indexing_decision.should_index);
    assert!(snapshot.files.is_empty());
    assert!(
        snapshot
            .indexing_decision
            .positive_signals
            .contains(&"README at workspace root".to_string())
    );
}

#[test]
fn readme_plus_source_is_enough_signal_for_non_git_roots() {
    let root = temp_root("readme_plus_source_is_enough_signal_for_non_git_roots");
    fs::write(root.join("README.md"), "# project\n").unwrap();
    fs::write(root.join("main.py"), "def main():\n    pass\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
    assert!(
        snapshot
            .files
            .iter()
            .any(|file| file.relative_path == "README.md")
    );
}

#[test]
fn shallow_common_source_files_are_indexing_signals() {
    for (name, signal) in [
        ("App.java", "shallow Java source"),
        ("Program.cs", "shallow C# source"),
        ("main.cpp", "shallow C/C++ source"),
        ("index.ts", "shallow TypeScript source"),
        ("app.js", "shallow JavaScript source"),
    ] {
        let root = temp_root(&format!("source-signal-{name}"));
        fs::write(root.join(name), "code\n").unwrap();

        let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
            .crawl(&root)
            .unwrap();

        assert!(snapshot.indexing_decision.should_index, "{name}");
        assert!(
            snapshot
                .indexing_decision
                .positive_signals
                .iter()
                .any(|existing| existing == signal),
            "{name}: {:?}",
            snapshot.indexing_decision.positive_signals
        );
    }
}

#[test]
fn depth_two_source_files_are_indexing_signals() {
    let root = temp_root("depth_two_source_files_are_indexing_signals");
    fs::create_dir_all(root.join("src").join("commands")).unwrap();
    fs::write(
        root.join("src").join("commands").join("main.rs"),
        "fn main() {}\n",
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
    assert!(
        snapshot
            .indexing_decision
            .positive_signals
            .contains(&"shallow Rust source".to_string())
    );
}

#[test]
fn common_code_directories_are_indexing_signals_when_they_contain_source() {
    let root = temp_root("common_code_directories_are_indexing_signals");
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(root.join("app").join("index.ts"), "export {}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
    assert!(
        snapshot
            .indexing_decision
            .positive_signals
            .contains(&"code directory app contains source".to_string())
    );
}

#[test]
fn common_vcs_markers_are_indexing_signals() {
    for marker in [".git", ".jj", ".hg", ".svn"] {
        let root = temp_root(&format!("vcs-marker-{marker}"));
        fs::create_dir_all(root.join(marker)).unwrap();

        let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
            .crawl(&root)
            .unwrap();

        assert!(snapshot.indexing_decision.should_index, "{marker}");
        assert!(
            snapshot
                .indexing_decision
                .positive_signals
                .iter()
                .any(|signal| signal.contains(marker)),
            "{marker}: {:?}",
            snapshot.indexing_decision.positive_signals
        );
    }
}

#[test]
fn git_worktree_file_is_an_indexing_signal() {
    let root = temp_root("git_worktree_file_is_an_indexing_signal");
    fs::write(
        root.join(".git"),
        "gitdir: ../repo/.git/worktrees/example\n",
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
}

#[test]
fn common_project_config_is_an_indexing_signal() {
    let root = temp_root("common_project_config_is_an_indexing_signal");
    fs::write(root.join("package.json"), "{}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
    assert!(
        snapshot
            .indexing_decision
            .positive_signals
            .contains(&"project marker package.json".to_string())
    );
}

#[test]
fn extended_project_markers_are_indexing_signals() {
    for marker in [
        "Dockerfile",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "tox.ini",
        "noxfile.py",
        "BUILD.bazel",
    ] {
        let root = temp_root(&format!("project-marker-{marker}"));
        fs::write(root.join(marker), "\n").unwrap();

        let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
            .crawl(&root)
            .unwrap();

        assert!(snapshot.indexing_decision.should_index, "{marker}");
        assert!(
            snapshot
                .indexing_decision
                .positive_signals
                .contains(&format!("project marker {marker}")),
            "{marker}: {:?}",
            snapshot.indexing_decision.positive_signals
        );
    }
}

#[test]
fn personal_folder_name_is_a_negative_but_not_blocking_signal() {
    let parent = temp_root("personal_folder_name_is_a_negative_signal");
    let root = parent.join("Downloads");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.indexing_decision.should_index);
    assert!(
        snapshot
            .indexing_decision
            .negative_signals
            .contains(&"workspace root looks like a personal folder".to_string())
    );
}

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
