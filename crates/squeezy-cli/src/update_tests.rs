use super::*;
use std::cmp::Ordering;
use std::sync::Mutex;

// `set_var` / `remove_var` is process-global; serialize the env-touching
// tests so a parallel runner doesn't let them race with each other or with
// the doctor tests in the sibling module.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScopedEnv {
    keys: Vec<String>,
}

impl ScopedEnv {
    fn new() -> Self {
        Self { keys: Vec::new() }
    }
    fn set(&mut self, key: &str, value: &str) {
        // SAFETY: callers hold `ENV_LOCK`; mutations stay serialised across
        // this test crate.
        unsafe { std::env::set_var(key, value) };
        self.keys.push(key.to_string());
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for key in &self.keys {
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn isolated_cache_dir(label: &str) -> std::path::PathBuf {
    let suffix: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "squeezy-update-{label}-{pid}-{suffix}",
        pid = std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp cache dir");
    dir
}

#[test]
fn compare_versions_treats_v_prefix_as_equal() {
    assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
    assert_eq!(compare_versions("V1.2.3", "v1.2.3"), Ordering::Equal);
}

#[test]
fn compare_versions_orders_patch_releases() {
    assert_eq!(compare_versions("v1.2.3", "v1.2.4"), Ordering::Less);
    assert_eq!(compare_versions("v1.2.4", "v1.2.3"), Ordering::Greater);
}

#[test]
fn compare_versions_compares_numerically_not_lexically() {
    // The whole reason a string compare is wrong: "1.10.0" < "1.2.9" lexically
    // even though semantically the inequality flips.
    assert_eq!(compare_versions("v1.10.0", "v1.2.9"), Ordering::Greater);
    assert_eq!(compare_versions("v1.2.9", "v1.10.0"), Ordering::Less);
}

#[test]
fn compare_versions_pads_missing_components() {
    assert_eq!(compare_versions("v1.2", "v1.2.0"), Ordering::Equal);
    assert_eq!(compare_versions("v1", "v1.0.1"), Ordering::Less);
}

#[test]
fn compare_versions_strips_prerelease_suffix() {
    // `1.0.0-rc1` should sort identically to `1.0.0` for our purposes; we
    // never want to nag a user who is running an RC to "upgrade" to the
    // matching final.
    assert_eq!(compare_versions("v1.0.0-rc1", "v1.0.0"), Ordering::Equal);
    assert_eq!(
        compare_versions("v1.0.0+sha.deadbeef", "v1.0.0"),
        Ordering::Equal,
    );
}

#[test]
fn strip_v_prefix_handles_both_cases() {
    assert_eq!(strip_v_prefix("v1.2.3"), "1.2.3");
    assert_eq!(strip_v_prefix("V1.2.3"), "1.2.3");
    assert_eq!(strip_v_prefix("1.2.3"), "1.2.3");
}

#[test]
fn version_cache_is_fresh_within_ttl() {
    let now = 1_700_000_000_u64;
    let cache = VersionCache {
        checked_at: now,
        latest: Some("1.0.0".to_string()),
        banner_acked_version: None,
    };
    // Within the 24h window.
    assert!(cache.is_fresh(now + 60 * 60));
    assert!(cache.is_fresh(now + 23 * 60 * 60));
    // At and past the 24h window.
    assert!(!cache.is_fresh(now + 24 * 60 * 60));
    assert!(!cache.is_fresh(now + 48 * 60 * 60));
}

#[test]
fn check_with_clock_uses_fresh_cache_without_network() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("fresh-cache");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());
    // Point the live endpoint at a host that will never resolve, so the
    // test fails loudly if anything tries to escape the cache.
    env.set(
        "SQUEEZY_RELEASE_API_OVERRIDE",
        "http://127.0.0.1:1/should-never-be-hit",
    );

    let now = 1_700_000_000_u64;
    write_cache(&VersionCache {
        checked_at: now,
        latest: Some("9.9.9".to_string()),
        banner_acked_version: None,
    })
    .expect("seed cache");

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check_with_clock("1.0.0", now + 60));

    match result {
        UpdateStatus::NewerAvailable {
            current,
            latest,
            from_cache,
        } => {
            assert_eq!(current, "1.0.0");
            assert_eq!(latest, "9.9.9");
            assert!(from_cache);
        }
        other => panic!("expected NewerAvailable from cache, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cached_banner_for_startup_uses_only_fresh_local_cache() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("startup-cache-only");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());
    env.set(
        "SQUEEZY_RELEASE_API_OVERRIDE",
        "http://127.0.0.1:1/should-never-be-hit",
    );

    assert!(
        cached_banner_for_startup().is_none(),
        "missing cache should not trigger a startup network probe"
    );

    write_cache(&VersionCache {
        checked_at: now_secs(),
        latest: Some("9.9.9".to_string()),
        banner_acked_version: None,
    })
    .expect("seed cache");

    let banner = cached_banner_for_startup().expect("fresh newer cache should render banner");
    assert!(banner.contains("v9.9.9"), "{banner}");
    let cache = read_cache().expect("read cache").expect("cache present");
    assert_eq!(
        cache.banner_acked_version.as_deref(),
        Some("9.9.9"),
        "startup banner should still preserve the one-shot ack behavior"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_with_clock_reports_up_to_date_when_current_matches_cache() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("up-to-date");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());
    env.set(
        "SQUEEZY_RELEASE_API_OVERRIDE",
        "http://127.0.0.1:1/should-never-be-hit",
    );

    let now = 1_700_000_000_u64;
    write_cache(&VersionCache {
        checked_at: now,
        latest: Some("1.0.0".to_string()),
        banner_acked_version: None,
    })
    .expect("seed cache");

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check_with_clock("v1.0.0", now + 60));

    match result {
        UpdateStatus::UpToDate { current, latest } => {
            assert_eq!(current, "1.0.0");
            assert_eq!(latest, "1.0.0");
        }
        other => panic!("expected UpToDate, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_with_clock_marks_stale_cache_and_disabled_check_unavailable() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("stale-disabled");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());
    env.set("SQUEEZY_DISABLE_UPDATE_CHECK", "1");

    let now = 1_700_000_000_u64;
    // Older than 24h — should be considered stale, forcing the network path.
    write_cache(&VersionCache {
        checked_at: now,
        latest: Some("9.9.9".to_string()),
        banner_acked_version: None,
    })
    .expect("seed cache");

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        // 48h later → cache is stale → network is disabled → unavailable.
        .block_on(check_with_clock("1.0.0", now + 48 * 60 * 60));

    match result {
        UpdateStatus::Unavailable { current, reason } => {
            assert_eq!(current, "1.0.0");
            assert!(
                reason.contains("SQUEEZY_DISABLE_UPDATE_CHECK"),
                "reason should explain why: {reason}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_with_clock_recovers_from_failure_cache_reads_after_ttl() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("failure-then-ttl");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());

    let now = 1_700_000_000_u64;
    // Previous attempt failed (latest=None). Within the TTL we trust that
    // cache; past it we'd hit the network again.
    write_cache(&VersionCache {
        checked_at: now,
        latest: None,
        banner_acked_version: None,
    })
    .expect("seed cache");

    // Within TTL: returns Unavailable from cache without making any network
    // call (we never set the override, so a real attempt would either hit
    // GitHub or fail — the cache path means we never get there).
    let within_ttl = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check_with_clock("1.0.0", now + 60 * 60));
    match within_ttl {
        UpdateStatus::Unavailable { reason, .. } => {
            assert!(
                reason.contains("cached"),
                "in-TTL failure reason should say cached: {reason}"
            );
        }
        other => panic!("expected Unavailable from failure cache, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn banner_for_startup_acks_latest_and_suppresses_repeat() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("banner-ack");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());

    let status = UpdateStatus::NewerAvailable {
        current: "1.0.0".to_string(),
        latest: "1.1.0".to_string(),
        from_cache: false,
    };
    let first = banner_for_startup(&status);
    assert!(
        first.as_deref().is_some_and(|s| s.contains("1.1.0")),
        "first call should produce a banner mentioning the new version, got {first:?}"
    );

    // Same release on the second TUI startup: stay quiet.
    let second = banner_for_startup(&status);
    assert!(
        second.is_none(),
        "banner should suppress on a repeat startup for the same latest version, got {second:?}"
    );

    // A newer release wakes the banner back up.
    let bumped = UpdateStatus::NewerAvailable {
        current: "1.0.0".to_string(),
        latest: "1.2.0".to_string(),
        from_cache: true,
    };
    let third = banner_for_startup(&bumped);
    assert!(
        third.as_deref().is_some_and(|s| s.contains("1.2.0")),
        "a new latest tag should produce a fresh banner, got {third:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn banner_for_startup_is_quiet_when_up_to_date_or_unavailable() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("banner-quiet");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());

    assert!(
        banner_for_startup(&UpdateStatus::UpToDate {
            current: "1.0.0".into(),
            latest: "1.0.0".into(),
        })
        .is_none()
    );

    assert!(
        banner_for_startup(&UpdateStatus::Unavailable {
            current: "1.0.0".into(),
            reason: "offline".into(),
        })
        .is_none()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_detail_strings_render_each_variant() {
    let up_to_date = UpdateStatus::UpToDate {
        current: "1.0.0".into(),
        latest: "1.0.0".into(),
    };
    assert!(up_to_date.doctor_detail().contains("up to date"));
    assert!(!up_to_date.is_warning());

    let newer = UpdateStatus::NewerAvailable {
        current: "1.0.0".into(),
        latest: "1.1.0".into(),
        from_cache: false,
    };
    let detail = newer.doctor_detail();
    assert!(detail.contains("v1.1.0"));
    // The upgrade hint is platform-specific: winget on Windows, cargo on others.
    if cfg!(target_os = "windows") {
        assert!(
            detail.contains("winget"),
            "Windows detail should suggest winget: {detail}"
        );
    } else {
        assert!(
            detail.contains("cargo install"),
            "non-Windows detail should suggest cargo: {detail}"
        );
    }
    assert!(newer.is_warning());

    let unavailable = UpdateStatus::Unavailable {
        current: "1.0.0".into(),
        reason: "offline".into(),
    };
    let detail = unavailable.doctor_detail();
    assert!(detail.contains("unavailable"));
    assert!(!unavailable.is_warning());
}

#[test]
fn cache_round_trip_persists_and_loads_json() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = isolated_cache_dir("round-trip");
    let cache_file = dir.join("version_check.json");
    let mut env = ScopedEnv::new();
    env.set("SQUEEZY_VERSION_CACHE_PATH", cache_file.to_str().unwrap());

    let value = VersionCache {
        checked_at: 1_700_000_000,
        latest: Some("1.2.3".to_string()),
        banner_acked_version: Some("1.1.0".to_string()),
    };
    write_cache(&value).expect("write");
    let loaded = read_cache().expect("read").expect("present");
    assert_eq!(loaded, value);
    let _ = std::fs::remove_dir_all(&dir);
}
