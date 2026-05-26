//! GitHub release lookup with a 24h on-disk cache.
//!
//! `doctor` surfaces the result as an "update" row; the TUI uses
//! `banner_message` to print a one-shot startup banner the first time it sees a
//! newer version. The cache lives next to the other `~/.cache/squeezy` state
//! and remembers which `latest` it already nudged the user about so repeat
//! startups within the same release stay quiet.
//!
//! Network failures (offline runs, GitHub 5xx, rate limits) degrade to
//! `Unavailable` — there is no fallback, no panic, and no blocking, because the
//! doctor row is a hint, not a gate.
//!
//! There is intentionally NO automatic binary swap here: this module is pure
//! detection. The user runs the printed `cargo install` / curl command.
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Public GitHub releases endpoint for the squeezy repo. The workspace
/// `Cargo.toml`'s `repository` field is the source of truth; we hardcode the
/// derived URL here rather than parsing it at build time because the API path
/// shape (`/repos/<owner>/<repo>/releases/latest`) is GitHub-specific and any
/// fork that wants its own endpoint should override via env (see
/// `SQUEEZY_RELEASE_API_OVERRIDE`).
const DEFAULT_RELEASE_API: &str = "https://api.github.com/repos/esqueezy/squeezy/releases/latest";

/// Override the release endpoint for tests / forks. Tests point this at a
/// `file://` URL or a non-existent host so they can exercise the cache and
/// degraded-network branches without hitting the live API.
const RELEASE_API_OVERRIDE_ENV: &str = "SQUEEZY_RELEASE_API_OVERRIDE";

/// Disable the network call entirely (cache is still consulted). Used by
/// tests and by CI environments where outbound HTTP is blocked.
const DISABLE_ENV: &str = "SQUEEZY_DISABLE_UPDATE_CHECK";

/// Override the cache file path. Tests point this at a tempdir so they can
/// round-trip the JSON without touching the user's real cache.
const CACHE_PATH_OVERRIDE_ENV: &str = "SQUEEZY_VERSION_CACHE_PATH";

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// On-disk shape of the version cache. Kept tiny so the file stays
/// human-inspectable (`cat ~/.cache/squeezy/version_check.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct VersionCache {
    /// Unix-seconds timestamp of the last successful GitHub poll. Used to
    /// gate the 24h TTL.
    pub checked_at: u64,
    /// `tag_name` from the GitHub release, normalised through
    /// `strip_v_prefix`. `None` only when a poll has been attempted but
    /// failed (e.g. rate-limited) — we still write the cache so we don't
    /// hammer the API on every doctor run.
    pub latest: Option<String>,
    /// Most recent latest-version string we've already shown a TUI banner
    /// for. Stays in sync with `latest` so the banner is one-shot per
    /// release: a newer `latest` clears the suppression naturally.
    #[serde(default)]
    pub banner_acked_version: Option<String>,
}

impl VersionCache {
    fn is_fresh(&self, now_secs: u64) -> bool {
        now_secs.saturating_sub(self.checked_at) < CACHE_TTL.as_secs()
    }
}

/// Outcome of a single update check. Doctor + TUI consume this; they
/// intentionally never observe the cache file shape directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// `current == latest` (or `current` is ahead of `latest`, e.g.
    /// running an unreleased dev build).
    UpToDate { current: String, latest: String },
    /// GitHub reports a newer release than the running binary.
    NewerAvailable {
        current: String,
        latest: String,
        /// True when this status came from the on-disk cache rather than a
        /// fresh HTTP request. Doctor doesn't need this; the TUI uses it
        /// indirectly via `banner_message`.
        from_cache: bool,
    },
    /// Network failure, rate limit, or `SQUEEZY_DISABLE_UPDATE_CHECK=1` set.
    /// Doctor renders this as a warn row, never a hard failure.
    Unavailable { current: String, reason: String },
}

impl UpdateStatus {
    /// Detail string for the doctor row. Pairs with `is_warning` so the
    /// row prints e.g. `[warn] update  v0.2.0 available (cargo install ...)`
    /// or `[ok] update  up to date (v0.1.0)`.
    pub fn doctor_detail(&self) -> String {
        match self {
            UpdateStatus::UpToDate { current, .. } => format!("up to date (v{current})"),
            UpdateStatus::NewerAvailable {
                current, latest, ..
            } => format!(
                "v{latest} available (cargo install squeezy --version {latest} \
                 or curl -sSL https://github.com/esqueezy/squeezy/releases/latest/download/install.sh | sh) \
                 — running v{current}"
            ),
            UpdateStatus::Unavailable { current, reason } => {
                format!("version check unavailable ({reason}) — running v{current}")
            }
        }
    }

    /// True when the doctor row should render as `warn`. `ok` for
    /// up-to-date or unavailable (so a binary in a network-isolated CI
    /// environment doesn't flap to warn just because GitHub was
    /// unreachable); `warn` is reserved for a real "you should upgrade".
    pub fn is_warning(&self) -> bool {
        matches!(self, UpdateStatus::NewerAvailable { .. })
    }
}

/// The version baked into this binary at build time. Trim any leading `v`
/// so callers can compare with normalised tag names.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Run a check: consult the cache, fall back to the network, persist the
/// result. Tokio-friendly because the existing `reqwest` deps are async.
pub async fn check_for_update() -> UpdateStatus {
    check_with_clock(current_version(), now_secs()).await
}

/// Same as `check_for_update` but with the clock + current version threaded
/// in for tests. Keeps the deterministic logic out of the live `tokio::main`
/// entrypoint.
pub(crate) async fn check_with_clock(current: &str, now_secs: u64) -> UpdateStatus {
    let current_norm = strip_v_prefix(current).to_string();

    // Fast path: a fresh cache entry skips the HTTP round trip entirely.
    let mut cache = read_cache().ok().flatten();
    if let Some(entry) = cache.as_ref()
        && entry.is_fresh(now_secs)
    {
        if let Some(latest) = entry.latest.as_deref() {
            return status_from_versions(&current_norm, latest, /*from_cache=*/ true);
        }
        // Cache exists but the last poll failed. Don't re-poll until the
        // TTL expires — the API rate-limit window is also 60 minutes
        // unauthenticated, and one failure per day is plenty of nag.
        return UpdateStatus::Unavailable {
            current: current_norm,
            reason: "last poll did not return a release (cached)".to_string(),
        };
    }

    if std::env::var(DISABLE_ENV)
        .ok()
        .is_some_and(|v| !v.is_empty())
    {
        return UpdateStatus::Unavailable {
            current: current_norm,
            reason: format!("{DISABLE_ENV} is set"),
        };
    }

    let endpoint = release_api_endpoint();
    match fetch_latest_tag(&endpoint).await {
        Ok(latest_tag) => {
            let normalised = strip_v_prefix(&latest_tag).to_string();
            // Preserve `banner_acked_version` across polls — that's the only
            // bit of state the TUI banner cares about.
            let acked = cache.take().and_then(|c| c.banner_acked_version);
            let _ = write_cache(&VersionCache {
                checked_at: now_secs,
                latest: Some(normalised.clone()),
                banner_acked_version: acked,
            });
            status_from_versions(&current_norm, &normalised, /*from_cache=*/ false)
        }
        Err(err) => {
            // Persist the failure too so we don't retry on every doctor run
            // within the TTL window.
            let acked = cache.take().and_then(|c| c.banner_acked_version);
            let _ = write_cache(&VersionCache {
                checked_at: now_secs,
                latest: None,
                banner_acked_version: acked,
            });
            UpdateStatus::Unavailable {
                current: current_norm,
                reason: err,
            }
        }
    }
}

/// Mark the current `latest` as one for which the TUI banner has already
/// been shown so the next startup within the same release stays quiet.
/// Returns the banner string the TUI should render, or `None` when nothing
/// new is available or the user has already been nagged for this version.
pub fn banner_for_startup(status: &UpdateStatus) -> Option<String> {
    let UpdateStatus::NewerAvailable {
        current, latest, ..
    } = status
    else {
        return None;
    };
    let mut cache = read_cache().ok().flatten().unwrap_or(VersionCache {
        checked_at: 0,
        latest: Some(latest.clone()),
        banner_acked_version: None,
    });
    if cache.banner_acked_version.as_deref() == Some(latest.as_str()) {
        return None;
    }
    cache.banner_acked_version = Some(latest.clone());
    // Best-effort persist; if disk is read-only the user just sees the
    // banner again next startup, which is the right safety stance.
    let _ = write_cache(&cache);
    Some(format!(
        "Update available: squeezy v{latest} (running v{current}). \
         Install: cargo install squeezy --version {latest}  \
         or  curl -sSL https://github.com/esqueezy/squeezy/releases/latest/download/install.sh | sh"
    ))
}

fn status_from_versions(current: &str, latest: &str, from_cache: bool) -> UpdateStatus {
    if compare_versions(current, latest) == std::cmp::Ordering::Less {
        UpdateStatus::NewerAvailable {
            current: current.to_string(),
            latest: latest.to_string(),
            from_cache,
        }
    } else {
        UpdateStatus::UpToDate {
            current: current.to_string(),
            latest: latest.to_string(),
        }
    }
}

fn release_api_endpoint() -> String {
    std::env::var(RELEASE_API_OVERRIDE_ENV).unwrap_or_else(|_| DEFAULT_RELEASE_API.to_string())
}

async fn fetch_latest_tag(endpoint: &str) -> Result<String, String> {
    let user_agent = format!("squeezy/{} (+update-check)", current_version());
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .map_err(|err| format!("http client init failed: {err}"))?;
    let response = client
        .get(endpoint)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|err| {
            // Surface the canonical "offline" cases as a single phrase so
            // tests can match without depending on the OS error text.
            if err.is_connect() || err.is_timeout() {
                "offline".to_string()
            } else {
                format!("request failed: {err}")
            }
        })?;
    if !response.status().is_success() {
        return Err(format!("github returned {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|err| format!("invalid JSON: {err}"))?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "github response missing tag_name".to_string())
}

fn read_cache() -> Result<Option<VersionCache>, String> {
    let path = match cache_path() {
        Some(p) => p,
        None => return Ok(None),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    serde_json::from_str::<VersionCache>(&text)
        .map(Some)
        .map_err(|err| err.to_string())
}

fn write_cache(cache: &VersionCache) -> Result<(), String> {
    let path = cache_path().ok_or_else(|| "no cache directory".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|err| err.to_string())?;
    std::fs::write(&path, text).map_err(|err| err.to_string())
}

/// `~/.cache/squeezy/version_check.json` on Linux / macOS,
/// `%LOCALAPPDATA%\squeezy\version_check.json` on Windows, mediated by the
/// `dirs` crate that the rest of the workspace already pulls in.
pub(crate) fn cache_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var(CACHE_PATH_OVERRIDE_ENV)
        && !override_path.is_empty()
    {
        return Some(PathBuf::from(override_path));
    }
    let dir = dirs::cache_dir()?;
    Some(dir.join("squeezy").join("version_check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip a leading `v` / `V` so `v1.2.3` and `1.2.3` compare equal.
pub(crate) fn strip_v_prefix(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

/// Numeric semver-ish compare: split on `.`, parse each component as `u64`,
/// pad missing components with `0`, ignore any pre-release tag (so
/// `v1.0.0-rc1` sorts the same as `v1.0.0`). The workspace doesn't pull in
/// the `semver` crate and we don't need range matching here — only a single
/// `Less / Equal / Greater` decision — so a hand-rolled parser keeps the
/// dependency surface unchanged.
pub(crate) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        // Drop pre-release / build metadata. `1.2.3-rc1+sha` becomes `1.2.3`.
        let core = s.split(['-', '+']).next().unwrap_or(s);
        core.split('.')
            .map(|piece| piece.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let a = parse(strip_v_prefix(a));
    let b = parse(strip_v_prefix(b));
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            non_eq => return non_eq,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
