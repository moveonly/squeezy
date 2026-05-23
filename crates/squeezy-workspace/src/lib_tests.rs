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
fn readme_is_enough_signal_for_non_git_roots() {
    let root = temp_root("readme_is_enough_signal_for_non_git_roots");
    fs::write(root.join("README.md"), "# project\n").unwrap();

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

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
