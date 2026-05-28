use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "squeezy_mention_cache_{tag}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn detects_mention_at_start_of_input() {
    let q = detect_mention("@gra", 4).expect("expected mention");
    assert_eq!(q.start, 0);
    assert_eq!(q.end, 4);
    assert_eq!(q.query, "gra");
}

#[test]
fn detects_mention_after_whitespace() {
    let q = detect_mention("hello @foo", 10).expect("mention after space");
    assert_eq!(q.query, "foo");
}

#[test]
fn does_not_detect_mention_mid_word() {
    // `email@host` — no popup because `@` is preceded by `l`.
    let result = detect_mention("email@host", 10);
    assert!(result.is_none(), "got: {result:?}");
}

#[test]
fn does_not_detect_when_cursor_is_before_at() {
    let result = detect_mention("hello @foo", 5);
    assert!(result.is_none(), "got: {result:?}");
}

#[test]
fn returns_empty_query_just_after_at() {
    let q = detect_mention("@", 1).expect("just `@`");
    assert_eq!(q.query, "");
}

#[test]
fn ranks_prefix_match_above_subsequence() {
    let files = vec![
        PathBuf::from("docs/zebra.md"),
        PathBuf::from("crates/graph/lib.rs"),
        PathBuf::from("readme.md"),
    ];
    let out = rank_files("gra", &files);
    assert_eq!(out.first().unwrap(), &PathBuf::from("crates/graph/lib.rs"));
}

#[test]
fn ranks_filename_prefix_above_path_substring() {
    let files = vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
        PathBuf::from("graph_helpers.rs"),
    ];
    let out = rank_files("graph", &files);
    assert_eq!(out[0], PathBuf::from("graph_helpers.rs"));
}

#[test]
fn rank_empty_query_returns_first_n_paths() {
    let files: Vec<PathBuf> = (0..20)
        .map(|i| PathBuf::from(format!("file{i}.rs")))
        .collect();
    let out = rank_files("", &files);
    assert_eq!(out.len(), MAX_MATCHES);
}

#[test]
fn apply_inserts_path_and_returns_new_cursor() {
    let q = MentionQuery {
        start: 6,
        end: 10,
        query: "gra".to_string(),
    };
    let popup = MentionPopup::from_query(q, vec![PathBuf::from("crates/squeezy-graph/src/lib.rs")]);
    let (new_input, cursor) = popup.apply("hello @gra").expect("apply");
    assert_eq!(new_input, "hello crates/squeezy-graph/src/lib.rs ");
    assert_eq!(cursor, new_input.len());
}

#[test]
fn mention_rank_uses_subsequence_for_abbreviations() {
    // A camel/snake abbreviation like `grphmgr` should still match
    // `graph_manager.rs` via the case-insensitive subsequence matcher.
    let files = vec![PathBuf::from("src/graph_manager.rs")];
    let out = rank_files("grphmgr", &files);
    assert_eq!(out.first(), Some(&PathBuf::from("src/graph_manager.rs")));
}

#[test]
fn mention_rank_keeps_filename_prefix_priority() {
    // `@lib` should return the shorter `lib.rs` before the longer
    // path whose basename also matches.
    let files = vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
        PathBuf::from("lib.rs"),
    ];
    let out = rank_files("lib", &files);
    assert_eq!(out.first(), Some(&PathBuf::from("lib.rs")));
}

#[test]
fn popup_navigation_clamps_at_bounds() {
    let q = MentionQuery {
        start: 0,
        end: 4,
        query: "a".to_string(),
    };
    let mut popup = MentionPopup::from_query(q, vec![PathBuf::from("a"), PathBuf::from("b")]);
    popup.move_up();
    assert_eq!(popup.selected, 0);
    popup.move_down();
    popup.move_down();
    assert_eq!(popup.selected, 1, "should clamp");
}

#[test]
fn workspace_cache_throttled_when_git_index_unchanged() {
    let tmp = TempRoot::new("throttle");
    fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
    let cache = WorkspaceFileCache::build(tmp.path());
    // Sample `now` < 5 s after build → cache reports fresh.
    let now = cache.built_at_for_tests() + Duration::from_secs(1);
    assert!(!cache.should_rebuild_at(tmp.path(), now));
}

#[test]
fn workspace_cache_invalidates_after_refresh_floor() {
    let tmp = TempRoot::new("floor");
    fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
    let cache = WorkspaceFileCache::build(tmp.path());
    // Sample `now` past the 5 s floor → cache wants a rebuild even though
    // nothing on disk changed, so newly added untracked files are seen.
    let now = cache.built_at_for_tests() + WORKSPACE_REFRESH_THROTTLE;
    assert!(cache.should_rebuild_at(tmp.path(), now));
}

#[test]
fn workspace_cache_invalidates_on_git_index_change() {
    let tmp = TempRoot::new("gitindex");
    let git_dir = tmp.path().join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    let index_path = git_dir.join("index");
    fs::write(&index_path, b"initial").unwrap();
    let cache = WorkspaceFileCache::build(tmp.path());
    // Sleep past filesystem mtime resolution before re-writing the index
    // so the stat differs (HFS+ rounds to 1s; ext4/APFS finer).
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&index_path, b"after-checkout").unwrap();
    // `now` stays inside the 5 s floor — the rebuild must come from the
    // mtime change, not the floor.
    let now = cache.built_at_for_tests() + Duration::from_millis(50);
    assert!(cache.should_rebuild_at(tmp.path(), now));
}

#[test]
fn workspace_cache_files_includes_walked_paths() {
    let tmp = TempRoot::new("walk");
    fs::write(tmp.path().join("alpha.rs"), "fn a() {}").unwrap();
    fs::write(tmp.path().join("beta.rs"), "fn b() {}").unwrap();
    let cache = WorkspaceFileCache::build(tmp.path());
    let names: Vec<String> = cache
        .files()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "alpha.rs"), "got: {names:?}");
    assert!(names.iter().any(|n| n == "beta.rs"), "got: {names:?}");
}
