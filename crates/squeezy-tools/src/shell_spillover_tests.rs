use super::*;

#[test]
fn spill_writes_stdout_only_when_stderr_is_empty() {
    let store = ShellSpilloverStore::new();
    let info = store
        .spill("call_one", b"hello world", b"")
        .expect("spill should succeed");
    assert!(
        info.path.starts_with(store.session_dir()),
        "spill path {:?} must live under the session dir {:?}",
        info.path,
        store.session_dir(),
    );
    let on_disk = fs::read(&info.path).expect("read back spillover");
    assert_eq!(on_disk, b"hello world");
    assert_eq!(info.bytes, on_disk.len() as u64);
}

#[test]
fn spill_appends_stderr_separator_when_present() {
    let store = ShellSpilloverStore::new();
    let info = store
        .spill("call_two", b"OUT", b"ERR")
        .expect("spill should succeed");
    let on_disk = fs::read(&info.path).expect("read back");
    let expected = format!("OUT{STDERR_SEPARATOR}ERR");
    assert_eq!(on_disk, expected.as_bytes());
}

#[test]
fn spill_returns_none_for_empty_streams() {
    let store = ShellSpilloverStore::new();
    assert!(store.spill("call_empty", b"", b"").is_none());
    assert_eq!(store.bytes_used(), 0);
}

#[test]
fn read_range_returns_byte_window_for_path() {
    let store = ShellSpilloverStore::new();
    let payload: Vec<u8> = (b'a'..=b'z').cycle().take(2048).collect();
    let info = store
        .spill("call_read", &payload, b"")
        .expect("spill should succeed");
    let path_str = info.path.to_string_lossy().to_string();

    let head = store.read_range(&path_str, 0, 64).expect("read head");
    assert_eq!(head.offset, 0);
    assert_eq!(head.bytes_returned, 64);
    assert_eq!(head.total_bytes, payload.len());
    assert!(head.truncated, "small limit must report truncated");
    assert_eq!(head.content.as_bytes(), &payload[..64]);

    let tail = store.read_range(&path_str, 2000, 1024).expect("read tail");
    assert_eq!(tail.offset, 2000);
    assert_eq!(tail.bytes_returned, payload.len() - 2000);
    assert_eq!(tail.total_bytes, payload.len());
    assert!(!tail.truncated, "full remaining tail must not be truncated");
    assert_eq!(tail.content.as_bytes(), &payload[2000..]);
}

#[test]
fn read_range_rejects_paths_outside_the_session_dir() {
    let store = ShellSpilloverStore::new();
    // Existing file outside the session dir.
    let outside = env::temp_dir().join(format!(
        "squeezy_spillover_outside_{}_{}.txt",
        std::process::id(),
        SESSION_NONCE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::write(&outside, b"forbidden").expect("write outside file");
    let err = store
        .read_range(&outside.to_string_lossy(), 0, 16)
        .expect_err("outside paths must be rejected");
    assert!(
        err.contains("outside the session directory"),
        "unexpected error: {err}",
    );
    let _ = fs::remove_file(outside);
}

#[test]
fn read_range_rejects_empty_path() {
    let store = ShellSpilloverStore::new();
    assert!(store.read_range("", 0, 16).is_err());
}

#[test]
fn spill_refuses_when_budget_would_be_exceeded() {
    let store = ShellSpilloverStore::with_budget(32);
    let big = vec![b'x'; 64];
    assert!(
        store.spill("call_big", &big, b"").is_none(),
        "spill exceeding budget must return None",
    );
    assert_eq!(store.bytes_used(), 0, "budget reservation must roll back");
    let small = vec![b'y'; 16];
    let info = store
        .spill("call_small_a", &small, b"")
        .expect("first small spill fits the budget");
    assert_eq!(info.bytes, 16);
    assert_eq!(store.bytes_used(), 16);
    let info2 = store
        .spill("call_small_b", &small, b"")
        .expect("second small spill fits the budget");
    assert_eq!(info2.bytes, 16);
    assert_eq!(store.bytes_used(), 32);
    assert!(
        store.spill("call_overflow", &small, b"").is_none(),
        "third spill must be refused once cap is hit",
    );
    assert_eq!(
        store.bytes_used(),
        32,
        "refused spill leaves usage unchanged"
    );
}

#[test]
fn session_dir_is_cleaned_up_on_drop() {
    let session_dir = {
        let store = ShellSpilloverStore::new();
        let info = store
            .spill("call_drop", b"persist?", b"")
            .expect("spill should succeed");
        let session_dir = store.session_dir().to_path_buf();
        assert!(info.path.exists(), "spill file exists while store is alive");
        session_dir
    };
    assert!(
        !session_dir.exists(),
        "session dir must be removed on Drop: {session_dir:?}",
    );
}

#[test]
fn sanitize_call_id_replaces_unsafe_characters() {
    assert_eq!(sanitize_call_id("simple_call.1"), "simple_call.1");
    assert_eq!(sanitize_call_id("call/with..slashes"), "call_with..slashes");
    assert_eq!(sanitize_call_id(""), "call");
    assert_eq!(
        sanitize_call_id("../../etc/passwd"),
        ".._.._etc_passwd",
        "path traversal characters must be neutralized",
    );
}

#[test]
fn distinct_stores_get_distinct_session_directories() {
    let a = ShellSpilloverStore::new();
    let b = ShellSpilloverStore::new();
    assert_ne!(
        a.session_dir(),
        b.session_dir(),
        "every store must mint a fresh session directory",
    );
}
