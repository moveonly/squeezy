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

#[tokio::test]
async fn raw_sidecar_writes_nothing_when_never_fed() {
    let store = Arc::new(ShellSpilloverStore::new());
    let sidecar = store
        .open_raw_sidecar("call_idle")
        .expect("open raw sidecar");
    let path = sidecar.path.clone();
    // No chunk ever written (the under-cap, zero-cost path): finalize must
    // report no spillover, leave no file, and charge nothing.
    assert!(sidecar.finalize().await.is_none());
    assert!(
        !path.exists(),
        "an unused raw sidecar must not leave a file behind: {path:?}",
    );
    assert_eq!(store.bytes_used(), 0, "an idle sidecar charges no budget");
}

#[tokio::test]
async fn raw_sidecar_persists_full_bytes_and_round_trips_via_read_range() {
    let store = Arc::new(ShellSpilloverStore::new());
    let sidecar = store
        .open_raw_sidecar("call_overflow")
        .expect("open raw sidecar");
    // Two chunks totalling more than any cap the live result keeps; the
    // sidecar must persist the FULL byte stream, not a capped prefix.
    let head = "A".repeat(5_000);
    let tail = "B".repeat(5_000);
    sidecar.write_chunk(&head).await;
    sidecar.write_chunk(&tail).await;
    let info = sidecar.finalize().await.expect("over-cap sidecar persists");

    let expected = format!("{head}{tail}");
    assert_eq!(info.bytes, expected.len() as u64);
    assert_eq!(
        store.bytes_used(),
        expected.len() as u64,
        "every persisted byte is charged to the session budget",
    );
    let on_disk = fs::read(&info.path).expect("read back raw sidecar");
    assert_eq!(on_disk, expected.as_bytes());

    // The exact recovery path the model uses: read_tool_output -> read_range.
    let recovered = store
        .read_range(&info.path.to_string_lossy(), 0, expected.len())
        .expect("read_range over the raw sidecar");
    assert_eq!(recovered.total_bytes, expected.len());
    assert_eq!(recovered.content.as_bytes(), expected.as_bytes());
}

#[tokio::test]
async fn raw_sidecar_filename_carries_the_call_id_raw_suffix() {
    let store = Arc::new(ShellSpilloverStore::new());
    let sidecar = store
        .open_raw_sidecar("call_named")
        .expect("open raw sidecar");
    let name = sidecar
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("sidecar file name");
    assert_eq!(name, "call_named-raw.txt");
}

#[tokio::test]
async fn raw_sidecar_is_bounded_by_the_session_budget() {
    // Budget below the chunk size: only the granted prefix is written and the
    // 100 MB-style cap still holds — the raw sidecar can never blow the
    // shared spillover budget.
    let store = Arc::new(ShellSpilloverStore::with_budget(32));
    let sidecar = store
        .open_raw_sidecar("call_budget")
        .expect("open raw sidecar");
    let big = "Z".repeat(64);
    sidecar.write_chunk(&big).await;
    // A second chunk after the budget is exhausted must be dropped entirely.
    sidecar.write_chunk("more").await;
    let info = sidecar.finalize().await.expect("partial bytes persisted");

    assert_eq!(info.bytes, 32, "only the budgeted prefix is written");
    assert_eq!(
        store.bytes_used(),
        32,
        "the session budget bounds the raw sidecar",
    );
    let on_disk = fs::read(&info.path).expect("read back");
    assert_eq!(on_disk, vec![b'Z'; 32]);
}

#[tokio::test]
async fn raw_sidecar_and_capped_spill_share_one_session_budget() {
    // The raw sidecar and the capped spill() draw from the same 100 MB-style
    // counter, so the two together can never exceed it.
    let store = Arc::new(ShellSpilloverStore::with_budget(40));
    let sidecar = store
        .open_raw_sidecar("call_shared")
        .expect("open raw sidecar");
    sidecar.write_chunk(&"R".repeat(30)).await;
    let info = sidecar.finalize().await.expect("raw bytes persisted");
    assert_eq!(info.bytes, 30);
    assert_eq!(store.bytes_used(), 30);

    // Only 10 bytes of budget remain; a capped spill of 8 fits, a second
    // spill of 8 does not.
    let first = store.spill("call_shared", &[b'x'; 8], b"");
    assert!(first.is_some(), "8-byte spill fits the remaining 10 bytes");
    assert_eq!(store.bytes_used(), 38);
    let second = store.spill("call_shared", &[b'y'; 8], b"");
    assert!(
        second.is_none(),
        "the shared budget refuses the spill once the raw sidecar consumed it",
    );
    assert_eq!(
        store.bytes_used(),
        38,
        "refused spill leaves usage unchanged"
    );
}

#[tokio::test]
async fn note_raw_and_overflowed_keys_on_combined_total() {
    let store = Arc::new(ShellSpilloverStore::new());
    let sidecar = store
        .open_raw_sidecar("call_combined")
        .expect("open raw sidecar");
    // Two streams sharing one sidecar: neither chunk alone exceeds cap=4096,
    // but their combined total does — mirroring the live shared truncation
    // budget. The overflow trigger must fire on the second.
    assert!(
        !sidecar.note_raw_and_overflowed(3_000, 4_096).await,
        "first stream alone stays under the shared cap",
    );
    assert!(
        sidecar.note_raw_and_overflowed(3_000, 4_096).await,
        "combined stdout+stderr crossing the cap must trigger overflow",
    );
}
