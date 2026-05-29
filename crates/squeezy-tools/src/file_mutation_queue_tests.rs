use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::{lock_paths_for_mutation, mutation_key};

static TEST_WORKSPACE_NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_dir(name: &str) -> PathBuf {
    let counter = TEST_WORKSPACE_NONCE.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "squeezy_file_mutation_queue_{name}_{pid}_{counter}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_files_lock_in_parallel() {
    let root = temp_dir("distinct");
    let path_a = root.join("a.txt");
    let path_b = root.join("b.txt");
    fs::write(&path_a, b"a").expect("write a");
    fs::write(&path_b, b"b").expect("write b");

    // Use a Barrier to force both tasks to acquire their locks before
    // either releases. If the locks gate distinct realpaths independently,
    // both tasks will sit on the barrier simultaneously and total elapsed
    // time stays close to a single hold duration; if they serialised, the
    // second task could not reach the barrier until the first releases
    // and the barrier would deadlock past the timeout.
    let hold = Duration::from_millis(150);
    let timeout = Duration::from_millis(500);
    let barrier = Arc::new(Barrier::new(2));

    let task_a = {
        let path_a = path_a.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let guard = lock_paths_for_mutation([&path_a]).await;
            assert_eq!(guard.held_count(), 1, "task_a should hold exactly one lock");
            // Use the synchronous Barrier (not tokio's) so the wait does
            // not yield through tokio's task scheduler, which would let
            // task_b cross the barrier even if task_a still held the
            // lock under a faulty implementation.
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                std::thread::sleep(hold);
            })
            .await
            .expect("blocking barrier");
            drop(guard);
            started.elapsed()
        })
    };

    let task_b = {
        let path_b = path_b.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let guard = lock_paths_for_mutation([&path_b]).await;
            assert_eq!(guard.held_count(), 1, "task_b should hold exactly one lock");
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                std::thread::sleep(hold);
            })
            .await
            .expect("blocking barrier");
            drop(guard);
            started.elapsed()
        })
    };

    let (elapsed_a, elapsed_b) = tokio::time::timeout(timeout, async {
        let a = task_a.await.expect("task_a");
        let b = task_b.await.expect("task_b");
        (a, b)
    })
    .await
    .expect("distinct-file locks must not block each other");

    // Both tasks reached the barrier so each measured at least one hold
    // window, but they overlapped — the max should be close to a single
    // hold window plus scheduling slack, far below 2 * hold.
    let max = elapsed_a.max(elapsed_b);
    assert!(
        max < hold + Duration::from_millis(200),
        "distinct-file mutations did not overlap: max elapsed {:?} (hold {:?})",
        max,
        hold,
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_file_lock_serialises() {
    let root = temp_dir("same");
    let path = root.join("shared.txt");
    fs::write(&path, b"shared").expect("write shared");

    // Counter increments inside the critical section; the maximum value
    // observed inside any single hold must be 1 if the mutex is honoured.
    let inside_now = Arc::new(AtomicU64::new(0));
    let max_observed = Arc::new(AtomicU64::new(0));
    let hold = Duration::from_millis(120);

    let make_task = |label: &'static str| {
        let path = path.clone();
        let inside_now = inside_now.clone();
        let max_observed = max_observed.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let _guard = lock_paths_for_mutation([&path]).await;
            let now = inside_now.fetch_add(1, Ordering::SeqCst) + 1;
            max_observed.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(hold).await;
            inside_now.fetch_sub(1, Ordering::SeqCst);
            (label, started.elapsed())
        })
    };

    let task_a = make_task("a");
    let task_b = make_task("b");
    let task_c = make_task("c");
    let (a, b, c) = tokio::join!(task_a, task_b, task_c);
    let (_, ea) = a.expect("a");
    let (_, eb) = b.expect("b");
    let (_, ec) = c.expect("c");

    assert_eq!(
        max_observed.load(Ordering::SeqCst),
        1,
        "concurrent same-realpath mutations must serialise"
    );

    // Three serialised holds of `hold` each should take >= ~3 * hold.
    // Allow some slack for scheduling jitter but reject parallel overlap.
    let total_max = ea.max(eb).max(ec);
    assert!(
        total_max >= hold * 3 - Duration::from_millis(40),
        "same-file holds did not stack: ea={:?} eb={:?} ec={:?} hold={:?}",
        ea,
        eb,
        ec,
        hold,
    );

    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symlink_collapses_to_target_realpath() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("symlink");
    let target = root.join("target.txt");
    let link = root.join("link.txt");
    fs::write(&target, b"contents").expect("write target");
    symlink(&target, &link).expect("create symlink");

    let key_via_target = mutation_key(&target);
    let key_via_link = mutation_key(&link);
    assert_eq!(
        key_via_target, key_via_link,
        "symlink should collapse onto its realpath"
    );

    // Holding the target's lock must also serialise an acquisition keyed by
    // the symlink. Force the order by acquiring `target` first, then try to
    // acquire `link` from another task with a short timeout; the timeout
    // must elapse because the inner async mutex is held.
    let target_guard = lock_paths_for_mutation([&target]).await;

    let link_path = link.clone();
    let mut waiter = tokio::spawn(async move { lock_paths_for_mutation([&link_path]).await });

    // The waiter should not finish while we hold the target's lock.
    let race = tokio::time::timeout(Duration::from_millis(80), &mut waiter).await;
    assert!(
        race.is_err(),
        "symlink acquisition should block on the target realpath"
    );

    drop(target_guard);
    let _link_guard = waiter
        .await
        .expect("waiter must complete once the target lock is released");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_path_uses_parent_realpath() {
    let root = temp_dir("missing");
    let parent = root.canonicalize().expect("canonicalize parent");
    let new_file = root.join("does-not-exist-yet.txt");
    let key = mutation_key(&new_file);

    assert!(
        key.starts_with(&parent),
        "missing-file key should sit under the canonical parent: {:?}",
        key
    );
    assert_eq!(
        key.file_name().and_then(|s| s.to_str()),
        Some("does-not-exist-yet.txt"),
    );

    // Two acquisitions of the same not-yet-existing path must still
    // serialise on a single lock.
    let inside = Arc::new(AtomicU64::new(0));
    let max_observed = Arc::new(AtomicU64::new(0));

    let make_task = || {
        let path = new_file.clone();
        let inside = inside.clone();
        let max_observed = max_observed.clone();
        tokio::spawn(async move {
            let _guard = lock_paths_for_mutation([&path]).await;
            let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
            max_observed.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            inside.fetch_sub(1, Ordering::SeqCst);
        })
    };
    let (a, b) = tokio::join!(make_task(), make_task());
    a.expect("a");
    b.expect("b");
    assert_eq!(max_observed.load(Ordering::SeqCst), 1);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_multi_path_callers_do_not_deadlock() {
    let root = temp_dir("overlap");
    let a = root.join("a.txt");
    let b = root.join("b.txt");
    let c = root.join("c.txt");
    for path in [&a, &b, &c] {
        fs::write(path, b"x").expect("write");
    }

    // Two callers acquire overlapping path sets. With sorted-key
    // acquisition both reach the inner Mutex in the same order
    // ({a,b,c}) so neither can hold a tail lock while waiting on a
    // head lock the other holds.
    let task_one = {
        let a = a.clone();
        let b = b.clone();
        tokio::spawn(async move {
            let _g = lock_paths_for_mutation([&a, &b]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
    };
    let task_two = {
        let b = b.clone();
        let c = c.clone();
        tokio::spawn(async move {
            let _g = lock_paths_for_mutation([&b, &c]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
    };

    tokio::time::timeout(Duration::from_secs(2), async {
        task_one.await.expect("task_one");
        task_two.await.expect("task_two");
    })
    .await
    .expect("overlapping multi-path callers must not deadlock");

    let _ = fs::remove_dir_all(&root);
}
