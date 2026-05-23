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
fn crawler_marks_large_and_binary_files_as_excluded() {
    let root = temp_root("crawler_marks_large_and_binary_files_as_excluded");
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
        file.relative_path == "large.rs"
            && file.reason == ExclusionReason::LargeFile
            && !file.is_dir
    }));
    assert!(snapshot.excluded.iter().any(|file| {
        file.relative_path == "bytes.bin" && file.reason == ExclusionReason::Binary && !file.is_dir
    }));
}

#[test]
fn crawler_does_not_read_large_files_into_memory() {
    let root = temp_root("crawler_does_not_read_large_files_into_memory");
    // Write a 2 MiB file with a Rust extension so the size check is the
    // exclusion path, not the binary or generated path.
    let big = vec![b'a'; 2 * 1024 * 1024];
    fs::write(root.join("big.rs"), &big).unwrap();
    fs::write(root.join("small.rs"), "fn ok(){}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        max_file_bytes: 1024,
        ..CrawlOptions::default()
    })
    .crawl(&root)
    .unwrap();

    // The size-classified file is excluded with its real size, but the
    // crawler must not have read 2 MiB to discover that.
    let entry = snapshot
        .excluded
        .iter()
        .find(|file| file.relative_path == "big.rs")
        .expect("big.rs is excluded");
    assert_eq!(entry.reason, ExclusionReason::LargeFile);
    assert_eq!(entry.size_bytes, big.len() as u64);
    // small.rs is indexed normally.
    assert!(
        snapshot
            .files
            .iter()
            .any(|file| file.relative_path == "small.rs")
    );
}

#[test]
fn default_policy_prunes_well_known_directories_at_dir_level() {
    let root = temp_root("default_policy_prunes_well_known_directories_at_dir_level");
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

    // File-level exclusions (root-level files).
    for (path, reason) in [
        ("src/generated.rs", ExclusionReason::Generated),
        ("Cargo.lock", ExclusionReason::Lockfile),
        ("image.png", ExclusionReason::Binary),
        ("huge.rs", ExclusionReason::LargeFile),
    ] {
        assert!(
            snapshot.excluded.iter().any(|file| {
                file.relative_path == path && file.reason == reason && !file.is_dir
            }),
            "{path} should be excluded as {reason:?}: {:?}",
            snapshot.excluded
        );
    }

    // Directory-level exclusions: the pruned dir appears once, its
    // children do not appear in `excluded` at all.
    for (dir, reason) in [
        ("node_modules", ExclusionReason::DependencyCache),
        ("vendor", ExclusionReason::Vendor),
        ("target", ExclusionReason::BuildOutput),
        (".venv", ExclusionReason::DependencyCache),
    ] {
        assert!(
            snapshot
                .excluded
                .iter()
                .any(|file| { file.relative_path == dir && file.reason == reason && file.is_dir }),
            "{dir} should be excluded as a directory ({reason:?}): {:?}",
            snapshot.excluded
        );
        for file in &snapshot.excluded {
            assert!(
                !file.relative_path.starts_with(&format!("{dir}/")),
                "expected {dir} pruning to hide child {} in excluded list",
                file.relative_path
            );
        }
    }

    assert!(snapshot.coverage.skipped_dirs >= 4);
    assert!(
        snapshot
            .coverage
            .reasons
            .contains_key(ExclusionReason::Vendor.as_str())
    );
}

#[test]
fn policy_include_classes_re_enables_lockfile_indexing() {
    let root = temp_root("policy_include_classes_re_enables_lockfile_indexing");
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
        file.relative_path == "src/private.rs"
            && file.reason == ExclusionReason::UserExclude
            && !file.is_dir
    }));
}

#[test]
fn exclude_classes_overrides_include_glob_for_that_class() {
    let root = temp_root("exclude_classes_overrides_include_glob_for_that_class");
    fs::create_dir_all(root.join("vendor/allowed")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("vendor/allowed/lib.rs"), "pub fn allowed() {}\n").unwrap();
    fs::write(root.join("src/private.rs"), "pub fn private() {}\n").unwrap();

    // `include` glob would normally re-enable `vendor/allowed/**`, but
    // `exclude_classes = ["vendor"]` forces the class to remain excluded.
    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        policy: IndexingPolicy {
            include: vec!["vendor/allowed/**".to_string()],
            exclude: Vec::new(),
            include_classes: Vec::new(),
            exclude_classes: vec!["vendor".to_string()],
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
    assert!(!indexed.contains(&"vendor/allowed/lib.rs"));
    assert!(
        snapshot
            .excluded
            .iter()
            .any(|file| file.relative_path == "vendor"
                && file.reason == ExclusionReason::Vendor
                && file.is_dir),
        "expected vendor dir to remain pruned despite include glob: {:?}",
        snapshot.excluded
    );
}

#[test]
fn invalid_glob_in_policy_surfaces_as_config_error() {
    let bad = IndexingPolicy {
        include: Vec::new(),
        exclude: vec!["[unterminated".to_string()],
        include_classes: Vec::new(),
        exclude_classes: Vec::new(),
    };
    let err = bad
        .compile()
        .expect_err("invalid glob should fail to compile");
    assert!(matches!(err, SqueezyError::Config(_)), "{err:?}");

    let crawl = CrawlOptions {
        policy: bad,
        ..CrawlOptions::default()
    };
    let err = WorkspaceCrawler::try_new(crawl).expect_err("try_new must propagate compile errors");
    assert!(matches!(err, SqueezyError::Config(_)), "{err:?}");
}

#[test]
fn indexing_policy_prunes_common_language_layouts_at_top_dir() {
    for (layout_dir, layout_file, expected_dir) in [
        ("rust/target/debug", "lib.rs", "rust/target"),
        ("python/.venv/lib", "site.py", "python/.venv"),
        ("js/node_modules/pkg", "index.ts", "js/node_modules"),
        ("go/vendor/pkg", "lib.go", "go/vendor"),
        ("java/build/classes", "App.java", "java/build"),
        ("csharp/bin/Debug", "Program.cs", "csharp/bin"),
        ("cpp/build/obj", "main.cpp", "cpp/build"),
    ] {
        let root = temp_root(&format!(
            "layout-{}-{}",
            layout_dir.replace(['/', '.'], "_"),
            layout_file
        ));
        let layout_path = root.join(layout_dir);
        fs::create_dir_all(&layout_path).unwrap();
        fs::write(root.join("README.md"), "# project\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(layout_path.join(layout_file), "needle\n").unwrap();

        let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
            .crawl(&root)
            .unwrap();

        assert!(
            snapshot
                .excluded
                .iter()
                .any(|entry| entry.relative_path == expected_dir && entry.is_dir),
            "{expected_dir} should appear as a pruned directory: {:?}",
            snapshot.excluded
        );
        // None of the children show up as separate excluded files: pruning
        // means the walker never visited them.
        for entry in &snapshot.excluded {
            assert!(
                !entry.relative_path.starts_with(&format!("{expected_dir}/")),
                "child {} leaked into excluded list",
                entry.relative_path
            );
        }
    }
}

#[test]
fn unrecognized_hidden_paths_are_skipped_when_include_hidden_is_false() {
    let root = temp_root("unrecognized_hidden_paths_are_skipped_when_include_hidden_is_false");
    fs::create_dir_all(root.join(".idea")).unwrap();
    fs::write(root.join(".idea/workspace.xml"), "<xml />\n").unwrap();
    fs::write(root.join(".bashrc"), "alias l='ls -la'\n").unwrap();
    fs::write(root.join("main.rs"), "fn main(){}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    let indexed = snapshot
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert!(indexed.contains(&"main.rs"));
    assert!(!indexed.iter().any(|path| path.starts_with(".idea/")));
    assert!(!indexed.contains(&".bashrc"));

    // The hidden directory should appear in excluded as `Hidden`.
    assert!(
        snapshot.excluded.iter().any(|file| {
            file.relative_path == ".idea" && file.reason == ExclusionReason::Hidden && file.is_dir
        }),
        ".idea should be pruned as Hidden: {:?}",
        snapshot.excluded
    );
}

#[test]
fn include_hidden_true_indexes_unrecognized_hidden_paths() {
    let root = temp_root("include_hidden_true_indexes_unrecognized_hidden_paths");
    fs::create_dir_all(root.join(".tools")).unwrap();
    fs::write(root.join(".tools/script.rs"), "fn t(){}\n").unwrap();
    fs::write(root.join("main.rs"), "fn main(){}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions {
        include_hidden: true,
        ..CrawlOptions::default()
    })
    .crawl(&root)
    .unwrap();

    let indexed = snapshot
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert!(indexed.contains(&".tools/script.rs"));
    assert!(indexed.contains(&"main.rs"));
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
fn crawler_classifies_java_files() {
    let root = temp_root("crawler_classifies_java_files");
    fs::write(
        root.join("Main.java"),
        "package com.example;\nclass Main {}\n",
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == "Main.java")
            .unwrap()
            .language,
        LanguageKind::Java
    );
}

#[test]
fn crawler_classifies_go_files() {
    let root = temp_root("crawler_classifies_go_files");
    fs::write(root.join("main.go"), "package main\nfunc main() {}\n").unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == "main.go")
            .unwrap()
            .language,
        LanguageKind::Go
    );
}

#[cfg(unix)]
#[test]
fn crawler_indexes_internal_symlinked_source_files() {
    let root = temp_root("crawler_indexes_internal_symlinked_source_files");
    fs::create_dir_all(root.join("real")).unwrap();
    fs::create_dir_all(root.join("linked")).unwrap();
    fs::write(root.join("real").join("example.go"), "package linked\n").unwrap();
    std::os::unix::fs::symlink(
        root.join("real").join("example.go"),
        root.join("linked").join("example.go"),
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert!(snapshot.files.iter().any(|file| {
        file.relative_path == "linked/example.go" && file.language == LanguageKind::Go
    }));
}

#[test]
fn crawler_allows_larger_java_sources_by_default() {
    let root = temp_root("crawler_allows_larger_java_sources_by_default");
    let mut source = "class Large {\n".to_string();
    source.push_str(&"void method() {}\n".repeat(80_000));
    source.push_str("}\n");
    assert!(source.len() > 1_000_000);
    assert!(source.len() < 2_000_000);
    fs::write(root.join("Large.java"), source).unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();

    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == "Large.java")
            .unwrap()
            .language,
        LanguageKind::Java
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
