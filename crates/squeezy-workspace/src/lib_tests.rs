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
    fs::write(root.join("src").join("lib.rs"), "pub fn visible() {}\n").unwrap();
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
    })
    .crawl(&root)
    .unwrap();

    assert!(snapshot.files.is_empty());
    assert!(snapshot.unsupported.iter().any(|file| {
        file.relative_path == "large.rs" && file.reason == UnsupportedReason::TooLarge
    }));
    assert!(snapshot.unsupported.iter().any(|file| {
        file.relative_path == "bytes.bin" && file.reason == UnsupportedReason::BinaryLike
    }));
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
fn crawler_classifies_js_ts_files() {
    let root = temp_root("crawler_classifies_js_ts_files");
    fs::write(root.join("package.json"), "{}\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("app.js"), "export const app = 1;\n").unwrap();
    fs::write(
        root.join("src").join("view.jsx"),
        "export const View = <main />;\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("types.ts"),
        "export type Name = string;\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("view.tsx"),
        "export const View = <main />;\n",
    )
    .unwrap();

    let snapshot = WorkspaceCrawler::new(CrawlOptions::default())
        .crawl(&root)
        .unwrap();
    let language_for = |path: &str| {
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == path)
            .map(|file| file.language)
            .unwrap()
    };

    assert_eq!(language_for("src/app.js"), LanguageKind::JavaScript);
    assert_eq!(language_for("src/view.jsx"), LanguageKind::Jsx);
    assert_eq!(language_for("src/types.ts"), LanguageKind::TypeScript);
    assert_eq!(language_for("src/view.tsx"), LanguageKind::Tsx);
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
