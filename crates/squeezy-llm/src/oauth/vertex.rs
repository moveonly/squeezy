//! Vertex AI OAuth credential source (H-28 / VX-1).
//!
//! Google's Vertex AI OpenAI-compat endpoint requires an OAuth2
//! bearer with scope `https://www.googleapis.com/auth/cloud-platform`
//! and a TTL of ~1 hour. Today every other provider grabs `VERTEX_ACCESS_TOKEN`
//! at client construction (`compatible.rs:84,93` →
//! `static_api_key_source`); the same snapshot is reused for the rest
//! of the session. Sessions exceeding the TTL hard-fail because
//! `send_with_auth_retry` re-reads the unchanged snapshot when the
//! provider returns `401` and bubbles up the second `401`.
//!
//! The fix this module provides is the minimum that the audit calls
//! out as acceptable: a [`VertexOAuthSource`] whose `current_key()`
//! shells out to `gcloud auth application-default print-access-token`
//! and caches the result for ~1 hour. `invalidate()` clears the
//! cache so the next `current_key()` re-runs the subprocess.
//! `GOOGLE_APPLICATION_CREDENTIALS` is honored implicitly: the
//! `gcloud` CLI itself reads that env var when present and otherwise
//! falls back to the cached user credentials laid down by `gcloud
//! auth application-default login` (the documented ADC chain).
//!
//! This module deliberately does **not** wire itself into the Vertex
//! preset. The preset's `from_config` path still calls
//! `static_api_key_source`; switching it over is Phase 4I's scope.
//! Until that lands the type lives here as a building block so
//! Phase 4I can drop the source in without rewriting the call site.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use squeezy_core::{Result, SqueezyError};
use tokio::process::Command;
use tokio::sync::RwLock;

use crate::credentials::{ApiKeyFuture, ApiKeySource};

/// How long a freshly-minted token is considered valid in our cache.
/// Google documents Vertex AI access tokens at "service account access
/// tokens last for 1 hour"; we refresh slightly early to avoid a
/// streaming response starting on a key that's about to die.
const TOKEN_TTL: Duration = Duration::from_secs(55 * 60);

/// Default executable name. Vendored as a const so a test or unusual
/// install can point the source elsewhere via [`VertexOAuthSource::with_command`].
pub const DEFAULT_GCLOUD_COMMAND: &str = "gcloud";

/// Arguments the source passes to `gcloud` to mint a fresh ADC access
/// token. Spelled out per Google's canonical recipe:
/// `gcloud auth application-default print-access-token`.
pub const GCLOUD_PRINT_TOKEN_ARGS: &[&str] = &["auth", "application-default", "print-access-token"];

/// Hard timeout on the `gcloud` subprocess. A healthy ADC refresh
/// (cached refresh token → Google OAuth round-trip) is sub-second;
/// 30 s gives a wide envelope for slow networks without letting a
/// stuck process hang the agent indefinitely.
const GCLOUD_TIMEOUT: Duration = Duration::from_secs(30);

/// Cached access-token snapshot. Stored as `(token, expires_at)` so
/// the cache check is a cheap `Instant` comparison; no need to parse
/// the JWT's `exp` claim (the cache TTL is a fixed-ratio undercut of
/// Google's 1 h documented value, which is the only contract we get).
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// Monotonic deadline. We use [`Instant`] rather than `SystemTime`
    /// so a wall-clock jump (NTP correction, suspend/resume) can't
    /// extend or curtail the cached token's lifetime.
    expires_at: Instant,
}

impl CachedToken {
    fn is_fresh(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

/// [`ApiKeySource`] backed by `gcloud auth application-default
/// print-access-token`.
///
/// On `current_key()`:
/// 1. Fast path: if the cached token has time left, return it
///    without spawning a subprocess.
/// 2. Slow path: spawn `gcloud auth application-default
///    print-access-token`, capture stdout, treat the trimmed value as
///    the new access token, and stash it under the cache with a TTL.
///
/// On `invalidate()`: drop the cache so the next `current_key()` runs
/// the subprocess. The auth-retry layer calls this on `401`/`403`.
///
/// `GOOGLE_APPLICATION_CREDENTIALS`: the subprocess inherits the
/// parent's environment, so any value the user has exported flows
/// through to `gcloud` for free. We document the env-var dependency
/// here so a future doctor step can hint when it's unset.
pub struct VertexOAuthSource {
    cache: Arc<RwLock<Option<CachedToken>>>,
    /// Executable name (almost always `"gcloud"`; tests override).
    command: String,
    /// Arguments after the executable. Stored as `Vec<String>` so
    /// tests can swap in a script that emits a known token without
    /// `gcloud` actually being installed.
    args: Vec<String>,
    /// Cache lifetime. Fixed in production; tests shorten it.
    ttl: Duration,
    /// Subprocess timeout. Fixed in production; tests shorten it.
    timeout: Duration,
    label: String,
}

impl VertexOAuthSource {
    /// Build the production source: spawns `gcloud auth
    /// application-default print-access-token` and caches the result.
    pub fn new() -> Self {
        Self::with_command(DEFAULT_GCLOUD_COMMAND, GCLOUD_PRINT_TOKEN_ARGS)
    }

    /// Build with an explicit command + args. Used by tests to point
    /// at a captive script; production callers should use [`Self::new`].
    pub fn with_command<I, S>(command: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            cache: Arc::new(RwLock::new(None)),
            command: command.into(),
            args: args.into_iter().map(|a| a.as_ref().to_string()).collect(),
            ttl: TOKEN_TTL,
            timeout: GCLOUD_TIMEOUT,
            label: "vertex".to_string(),
        }
    }

    /// Test-only escape hatch: shorten the cache TTL so a test can
    /// observe expiry without sleeping for 55 minutes.
    #[doc(hidden)]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Test-only escape hatch: shorten the subprocess timeout.
    #[doc(hidden)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns `true` when the cached token is missing or stale.
    /// Useful for diagnostics.
    pub async fn needs_refresh(&self) -> bool {
        match self.cache.read().await.as_ref() {
            Some(cached) => !cached.is_fresh(),
            None => true,
        }
    }

    /// Force a refresh round-trip. Concurrent callers funnel through
    /// the same write lock so two parallel `current_key` calls only
    /// fire one subprocess.
    pub async fn force_refresh(&self) -> Result<String> {
        let mut guard = self.cache.write().await;
        // Re-check inside the lock: another writer may have refreshed
        // while we were queued.
        if let Some(cached) = guard.as_ref()
            && cached.is_fresh()
        {
            return Ok(cached.token.clone());
        }
        let token = run_gcloud(&self.command, &self.args, self.timeout).await?;
        *guard = Some(CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + self.ttl,
        });
        Ok(token)
    }

    /// Path to the configured `gcloud` executable. Exposed so a
    /// future doctor step can include it in `auth status` output.
    pub fn command_path(&self) -> PathBuf {
        PathBuf::from(&self.command)
    }
}

impl Default for VertexOAuthSource {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for VertexOAuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the cached token; expose the command + label so
        // a `Debug`-printed provider client is still useful.
        f.debug_struct("VertexOAuthSource")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("label", &self.label)
            .field("cached", &"<redacted>")
            .finish()
    }
}

impl ApiKeySource for VertexOAuthSource {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        Box::pin(async move {
            // Fast path: read-lock and serve the cached token if it
            // still has time left.
            {
                let guard = self.cache.read().await;
                if let Some(cached) = guard.as_ref()
                    && cached.is_fresh()
                {
                    return Ok(cached.token.clone());
                }
            }
            self.force_refresh().await
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.cache.write().await;
            *guard = None;
            Ok(())
        })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

/// Spawn `command args...`, wait up to `timeout`, return the trimmed
/// stdout. Translates every failure mode into a `SqueezyError` whose
/// message hints the user toward the missing prerequisite (the
/// `gcloud` CLI, ADC login).
async fn run_gcloud(command: &str, args: &[String], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        // Keep the subprocess detached from our stdin so a paused
        // agent doesn't gum it up waiting for input.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Prevent the subprocess from inheriting our controlling
        // terminal: gcloud sometimes prompts when re-auth is needed
        // and we want that to fail closed rather than hang.
        .kill_on_drop(true);

    let child_future = cmd.output();
    let output = tokio::time::timeout(timeout, child_future)
        .await
        .map_err(|_| {
            SqueezyError::ProviderNotConfigured(format!(
                "`{command}` did not respond within {timeout:?}; \
                 ensure `gcloud auth application-default login` has been completed \
                 and try again"
            ))
        })?
        .map_err(|err| {
            // `Err` from `output()` is the spawn failure case — most
            // commonly an `ENOENT` because `gcloud` isn't on `$PATH`.
            // We surface a structured error so the CLI can hint at
            // the install link rather than dumping a raw IO error.
            let kind = err.kind();
            if kind == std::io::ErrorKind::NotFound {
                SqueezyError::ProviderNotConfigured(format!(
                    "`{command}` not found on $PATH; install the Google Cloud CLI \
                     (https://cloud.google.com/sdk/docs/install) and run \
                     `gcloud auth application-default login`"
                ))
            } else {
                SqueezyError::ProviderNotConfigured(format!("failed to spawn `{command}`: {err}"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "`{command} {args}` failed (exit {status}): {stderr}",
            args = args.join(" "),
            status = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr = stderr.trim(),
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!("`{command}` produced non-UTF-8 stdout: {err}"))
    })?;
    let token = stdout.trim().to_string();
    if token.is_empty() {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "`{command} {}` produced an empty token; run \
             `gcloud auth application-default login` and try again",
            args.join(" "),
        )));
    }
    Ok(token)
}

#[cfg(test)]
#[path = "vertex_tests.rs"]
mod tests;
