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

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
