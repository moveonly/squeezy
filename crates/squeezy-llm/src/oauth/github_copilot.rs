//! GitHub Copilot OAuth (Copilot Chat API subscription) credential
//! source.
//!
//! Uses GitHub's device-code flow:
//!
//! 1. The CLI runs `squeezy auth github-copilot login`, optionally
//!    prompting for an enterprise domain.
//! 2. Squeezy POSTs `https://{domain}/login/device/code` to obtain a
//!    `user_code` + `verification_uri` pair, prints them to the
//!    terminal, and (when not `--no-browser`) opens the URL.
//! 3. We then poll `https://{domain}/login/oauth/access_token` every
//!    `interval` seconds until the user completes the GitHub consent
//!    step and the endpoint returns a long-lived `access_token` (the
//!    GitHub OAuth token).
//! 4. We exchange that GitHub OAuth token for a short-lived Copilot
//!    Chat API token at
//!    `https://api.{domain}/copilot_internal/v2/token` (Bearer auth +
//!    the editor headers VSCode's Copilot Chat extension sends).
//! 5. The Copilot Chat token carries the wire-format payload
//!    `tid=…;exp=…;proxy-ep=proxy.individual.githubcopilot.com;…`;
//!    [`base_url_from_token`] turns the `proxy-ep` host into the
//!    matching `api.` host so requests land on the per-account
//!    backend.
//! 6. We persist the GitHub OAuth token + Copilot token + expiry to
//!    `~/.squeezy/auth/github-copilot.json` (mode `0o600` on Unix).
//! 7. After login we optionally run [`enable_models`] which POSTs
//!    `/models/{id}/policy` for each model id, flipping the per-user
//!    "enabled" flag GitHub requires before the chat API will route
//!    requests for newer models (Claude, GPT-5.x, Gemini).
//!
//! At request time the [`GitHubCopilotOAuthSource`] returns the
//! cached Copilot token while it has more than a one-minute safety
//! window of life left; otherwise it re-runs step 4 under an
//! `RwLock` (so concurrent provider clients all see the same fresh
//! token) and rewrites the on-disk file. The GitHub OAuth token
//! itself never rotates short of the user revoking the app on
//! github.com — so the "refresh" path only ever re-hits the Copilot
//! exchange endpoint.
//!
//! The provider client side ([`GitHubCopilotProvider`]) is a thin
//! wrapper over [`crate::OpenAiCompatibleProvider`] configured with
//! the editor headers and the token-derived base URL: Copilot Chat
//! speaks Chat Completions on the wire, so the existing transport
//! already handles streaming, retries, and tool calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use squeezy_core::{OpenAiCompatiblePreset, ProviderTransportConfig, Result, SqueezyError};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::credentials::{ApiKeyFuture, ApiKeySource};
use crate::{LlmProvider, LlmRequest, LlmStream, OpenAiCompatibleProvider};

// ─── Public OAuth constants ────────────────────────────────────────────────

/// OAuth client id GitHub registered for the Copilot Chat VSCode
/// extension. Pi stores this as a base64 string
/// (`SXYxLmI1MDdhMDhjODdlY2ZlOTg=`) to keep it out of naive secret
/// scanners; we mirror that decoded value verbatim because GitHub
/// pins it server-side — any other id is rejected at the device-code
/// endpoint.
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Default GitHub domain. Enterprise installs override this with
/// their tenant hostname (e.g. `acme.ghe.com`).
pub const DEFAULT_DOMAIN: &str = "github.com";

/// OAuth scope requested on the GitHub side. `read:user` is enough
/// for Copilot to identify the subscriber — broader scopes would
/// trigger a more aggressive consent screen for no benefit.
pub const SCOPE: &str = "read:user";

/// Device-code grant type literal. Spelled out per RFC 8628 §3.4.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Editor-impersonation headers pi sends so GitHub bills the request
/// against the Copilot Chat extension allotment instead of the API
/// allotment. Values match pi byte-for-byte; bumping them is a
/// coordinated change with the upstream pi mapping.
pub const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Fallback base URL when token parsing fails (or the user is on a
/// brand-new account whose token has not yet been observed). Enterprise
/// installs derive their base from the `enterprise_domain`.
pub const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

/// Filename under `~/.squeezy/auth/` where persisted Copilot tokens
/// live. Hyphenated to match the CLI subcommand (`github-copilot`);
/// the rest of the codebase stays on snake_case provider section
/// names.
pub const AUTH_FILE_NAME: &str = "github-copilot.json";

/// RFC 8628 §3.2 default polling interval when the device-code
/// response omits `interval`.
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// RFC 8628 §3.5 enforces a minimum polling interval; we clamp to a
/// second so a misbehaving server cannot trick us into a tight loop.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `slow_down` responses (RFC 8628 §3.5) must bump the polling
/// interval by five seconds going forward.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// Cushion subtracted from the issuer-reported expiry. The Copilot
/// token endpoint reports absolute seconds-since-epoch via
/// `expires_at`; pi shaves five minutes off so a long-running stream
/// never starts on a credential about to die mid-flight. We mirror
/// that.
const REFRESH_LEAD_TIME: Duration = Duration::from_secs(5 * 60);

/// Safety window the OAuth source uses to decide a cached token is
/// "near expiry" and should be refreshed proactively. One minute on
/// top of the lead-time keeps the next streaming request from racing
/// the absolute expiry.
const NEAR_EXPIRY_LEEWAY: Duration = Duration::from_secs(60);

/// HTTP timeout for the token + policy round-trips. The endpoints
/// are normally sub-second; 30 s gives a wide envelope for slow
/// networks without letting a stuck connection hang the agent.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bundled list of Copilot Chat model ids whose per-user policy must
/// be flipped to `enabled` after sign-in. Mirrors the curated set
/// pi enables (see `others/pi/packages/ai/src/models.generated.ts`)
/// — anything missing from this list still works at the API layer
/// once the user accepts policy in the VSCode UI; the bundled list
/// is "best-effort, do not silently fail" rather than authoritative.
pub const DEFAULT_POLICY_MODELS: &[&str] = &[
    "claude-haiku-4.5",
    "claude-opus-4.5",
    "claude-opus-4.6",
    "claude-opus-4.7",
    "claude-sonnet-4.5",
    "claude-sonnet-4.6",
    "gemini-2.5-pro",
    "gemini-3-flash-preview",
    "gemini-3.5-flash",
    "gpt-4.1",
    "gpt-4o",
    "gpt-5-mini",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "grok-code-fast-1",
    "o4-mini",
];

// ─── On-disk persistence ───────────────────────────────────────────────────

/// Persisted Copilot credential set. `github_token` is the long-lived
/// GitHub OAuth token (the device-code grant output); `copilot_token`
/// is the short-lived Copilot Chat API token derived from it. The
/// CLI re-runs only the latter exchange on refresh, so the GitHub
/// token outlives the Copilot token across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedGitHubCopilotTokens {
    /// Long-lived GitHub OAuth bearer used to mint Copilot tokens.
    /// Persisted alongside the short-lived token so a process restart
    /// can refresh without re-running the device-code flow.
    pub github_token: String,
    /// Short-lived Copilot Chat API token. The `proxy-ep` segment
    /// inside this string carries the per-account API host; see
    /// [`base_url_from_token`].
    pub copilot_token: String,
    /// Absolute expiry of `copilot_token` as Unix milliseconds, with
    /// the [`REFRESH_LEAD_TIME`] cushion already applied.
    pub expires_at_unix_ms: u64,
    /// Optional GitHub Enterprise domain (e.g. `acme.ghe.com`). `None`
    /// means the user is on github.com.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_domain: Option<String>,
    /// Free-form provider tag so a future multi-account refactor can
    /// tell two persisted accounts apart. Always
    /// `"github-copilot-oauth"`.
    #[serde(default = "default_provider_tag")]
    pub provider: String,
}

fn default_provider_tag() -> String {
    "github-copilot-oauth".to_string()
}

impl PersistedGitHubCopilotTokens {
    fn is_near_expiry(&self, now: SystemTime) -> bool {
        let expires_at = UNIX_EPOCH + Duration::from_millis(self.expires_at_unix_ms);
        match expires_at.checked_sub(NEAR_EXPIRY_LEEWAY) {
            Some(safe_until) => now >= safe_until,
            None => true,
        }
    }
}

/// Path to the persisted Copilot auth file. Honors
/// `SQUEEZY_GITHUB_COPILOT_AUTH_FILE` so tests and unusual
/// deployments can redirect persistence without touching the user's
/// real `~/.squeezy/auth/github-copilot.json`.
pub fn auth_file_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("SQUEEZY_GITHUB_COPILOT_AUTH_FILE")
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    default_auth_path()
}

/// Canonical persistence path:
/// `<home>/.squeezy/auth/github-copilot.json`. Separated from
/// [`auth_file_path`] so callers (and tests) can inspect the default
/// without consulting env vars.
pub fn default_auth_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".squeezy").join("auth").join(AUTH_FILE_NAME))
}

/// Read persisted tokens from disk. Returns `Ok(None)` when the
/// file is absent (the natural state before the first login); a
/// hard error on every other parse / I/O failure so the caller can
/// distinguish "never logged in" from "logged in but stored file is
/// corrupt".
pub fn read_tokens(path: &Path) -> Result<Option<PersistedGitHubCopilotTokens>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "failed to read {}: {err}",
                path.display()
            )));
        }
    };
    let tokens: PersistedGitHubCopilotTokens = serde_json::from_slice(&bytes).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!(
            "github-copilot auth file {} is not valid JSON: {err}",
            path.display()
        ))
    })?;
    Ok(Some(tokens))
}

/// Persist tokens to disk with `chmod 600` on Unix. Parent directory
/// is created with mode `0o700`; on Windows we fall back to the
/// default permission set (the AGENTS guide notes Windows sandbox is
/// best-effort).
pub fn write_tokens(path: &Path, tokens: &PersistedGitHubCopilotTokens) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            SqueezyError::Config(format!(
                "failed to create {} for github-copilot OAuth tokens: {err}",
                parent.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(parent) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(parent, perms);
            }
        }
    }
    let serialized = serde_json::to_vec_pretty(tokens).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to serialize github-copilot OAuth tokens: {err}"
        ))
    })?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &serialized).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to write {} for github-copilot OAuth tokens: {err}",
            tmp.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to rename {} to {} for github-copilot OAuth tokens: {err}",
            tmp.display(),
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ─── Domain + URL helpers ──────────────────────────────────────────────────

/// Normalize a user-supplied enterprise domain or URL into a bare
/// host. Returns `None` for an empty/whitespace input so the caller
/// can fall back to [`DEFAULT_DOMAIN`]. Lets an enterprise user paste
/// either `acme.ghe.com` or `https://acme.ghe.com/`.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url_text = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = reqwest::Url::parse(&url_text).ok()?;
    let host = url.host_str()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Endpoints derived from a base GitHub host. Stored in one struct
/// so tests can swap the host once and not lose track of which URL
/// goes where.
#[derive(Debug, Clone)]
pub struct GitHubCopilotUrls {
    pub device_code_url: String,
    pub access_token_url: String,
    pub copilot_token_url: String,
}

impl GitHubCopilotUrls {
    pub fn for_domain(domain: &str) -> Self {
        Self {
            device_code_url: format!("https://{domain}/login/device/code"),
            access_token_url: format!("https://{domain}/login/oauth/access_token"),
            copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
        }
    }
}

/// Extract the per-account API base URL from a Copilot Chat token.
/// The token payload is a `;`-delimited key/value string and
/// `proxy-ep=<host>` carries the host of the inference proxy; the
/// matching API host is the same value with the `proxy.` prefix
/// rewritten to `api.`.
pub fn base_url_from_token(token: &str) -> Option<String> {
    for part in token.split(';') {
        let (key, value) = part.split_once('=')?;
        if key.trim() == "proxy-ep" {
            let host = value.trim();
            if host.is_empty() {
                return None;
            }
            let api_host = host
                .strip_prefix("proxy.")
                .map(|rest| format!("api.{rest}"))
                .unwrap_or_else(|| host.to_string());
            return Some(format!("https://{api_host}"));
        }
    }
    None
}

/// Resolve the API base URL for the supplied Copilot token, falling
/// back to the per-account default and finally to the
/// `enterprise_domain`-derived host for enterprise installs.
pub fn resolve_base_url(token: &str, enterprise_domain: Option<&str>) -> String {
    if let Some(url) = base_url_from_token(token) {
        return url;
    }
    if let Some(domain) = enterprise_domain
        && !domain.trim().is_empty()
    {
        return format!("https://copilot-api.{domain}");
    }
    DEFAULT_BASE_URL.to_string()
}

// ─── Device-code request shapes ────────────────────────────────────────────

/// Outcome of the initial `/login/device/code` POST. Mirrors RFC 8628
/// §3.2 with the optional `interval` and the human-presented
/// `verification_uri` GitHub returns.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub interval: Option<u64>,
    pub expires_in: u64,
}

/// Successful `/login/oauth/access_token` POST. GitHub returns a
/// bearer token plus an opaque scope string; we surface both for
/// `auth status`-style diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DeviceTokenSuccessResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Error response GitHub returns while the user is still completing
/// the device-code flow (`authorization_pending`, `slow_down`) or
/// when the flow has terminated unrecoverably (`expired_token`,
/// `access_denied`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DeviceTokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Outcome of the Copilot token endpoint. `expires_at` is absolute
/// seconds-since-epoch; the persistence layer converts to
/// milliseconds with the [`REFRESH_LEAD_TIME`] cushion applied.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: i64,
}

// ─── Device-code HTTP round-trips ──────────────────────────────────────────

/// POST `https://{domain}/login/device/code` and return the parsed
/// device-code packet. Public so the CLI can call it directly when
/// staging a login from a host that already has the browser open.
pub async fn start_device_flow(
    client: &reqwest::Client,
    urls: &GitHubCopilotUrls,
) -> Result<DeviceCodeResponse> {
    let response = client
        .post(&urls.device_code_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("user-agent", COPILOT_USER_AGENT)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("github-copilot device-code POST failed: {err}"))
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|err| {
        SqueezyError::ProviderRequest(format!(
            "github-copilot device-code body read failed: {err}"
        ))
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(SqueezyError::ProviderRequest(format!(
            "github-copilot device-code endpoint returned {status}: {body}"
        )));
    }
    serde_json::from_slice::<DeviceCodeResponse>(&bytes).map_err(|err| {
        let body = String::from_utf8_lossy(&bytes);
        SqueezyError::ProviderRequest(format!(
            "github-copilot device-code response was not valid JSON: {err}; body={body}"
        ))
    })
}

/// One pass of the polling loop. Surfaces the four states RFC 8628
/// distinguishes so the outer loop can apply the right backoff.
#[derive(Debug, PartialEq, Eq)]
pub enum DevicePollOutcome {
    Pending,
    SlowDown,
    Complete(String),
    Failed(String),
}

/// POST `https://{domain}/login/oauth/access_token` once. Returns
/// the polling outcome — `Pending` and `SlowDown` are recoverable;
/// `Failed` carries a user-facing message. `Complete` carries the
/// GitHub OAuth bearer.
pub async fn poll_access_token_once(
    client: &reqwest::Client,
    urls: &GitHubCopilotUrls,
    device_code: &str,
) -> Result<DevicePollOutcome> {
    let response = client
        .post(&urls.access_token_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("user-agent", COPILOT_USER_AGENT)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", DEVICE_CODE_GRANT_TYPE),
        ])
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("github-copilot access-token POST failed: {err}"))
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|err| {
        SqueezyError::ProviderRequest(format!(
            "github-copilot access-token body read failed: {err}"
        ))
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(SqueezyError::ProviderRequest(format!(
            "github-copilot access-token endpoint returned {status}: {body}"
        )));
    }

    if let Ok(success) = serde_json::from_slice::<DeviceTokenSuccessResponse>(&bytes)
        && !success.access_token.is_empty()
    {
        return Ok(DevicePollOutcome::Complete(success.access_token));
    }
    if let Ok(error) = serde_json::from_slice::<DeviceTokenErrorResponse>(&bytes) {
        return Ok(classify_device_error(&error));
    }
    let body = String::from_utf8_lossy(&bytes);
    Ok(DevicePollOutcome::Failed(format!(
        "github-copilot device flow: unexpected response body: {body}"
    )))
}

fn classify_device_error(err: &DeviceTokenErrorResponse) -> DevicePollOutcome {
    match err.error.as_str() {
        "authorization_pending" => DevicePollOutcome::Pending,
        "slow_down" => DevicePollOutcome::SlowDown,
        other => {
            let suffix = err
                .error_description
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(": {s}"))
                .unwrap_or_default();
            DevicePollOutcome::Failed(format!(
                "github-copilot device flow failed: {other}{suffix}"
            ))
        }
    }
}

/// Drive the polling loop to completion. Bounded by `expires_in`,
/// sleeps for at least `interval` seconds between pollings, applies
/// [`SLOW_DOWN_INCREMENT`] whenever the server says `slow_down`, and
/// surfaces a typed `Cancelled` error when the supplied
/// [`CancellationToken`] fires.
pub async fn poll_for_github_token(
    client: &reqwest::Client,
    urls: &GitHubCopilotUrls,
    device: &DeviceCodeResponse,
    cancel: &CancellationToken,
) -> Result<String> {
    let deadline = SystemTime::now().checked_add(Duration::from_secs(device.expires_in));
    let mut interval = Duration::from_secs(
        device
            .interval
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
    );
    if interval < MIN_POLL_INTERVAL {
        interval = MIN_POLL_INTERVAL;
    }
    let mut slow_down_observed = false;

    loop {
        if cancel.is_cancelled() {
            return Err(SqueezyError::ProviderRequest(
                "github-copilot device flow cancelled".to_string(),
            ));
        }
        if let Some(deadline) = deadline
            && SystemTime::now() >= deadline
        {
            return Err(SqueezyError::ProviderRequest(if slow_down_observed {
                "github-copilot device flow timed out after one or more slow_down responses; \
                 clock drift in a VM/WSL host is a common cause — sync the system clock and retry"
                    .to_string()
            } else {
                "github-copilot device flow timed out before the user completed the consent step"
                    .to_string()
            }));
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                return Err(SqueezyError::ProviderRequest(
                    "github-copilot device flow cancelled".to_string(),
                ));
            }
            _ = tokio::time::sleep(interval) => {}
        }
        match poll_access_token_once(client, urls, &device.device_code).await? {
            DevicePollOutcome::Complete(token) => return Ok(token),
            DevicePollOutcome::Pending => continue,
            DevicePollOutcome::SlowDown => {
                slow_down_observed = true;
                interval = interval.saturating_add(SLOW_DOWN_INCREMENT);
            }
            DevicePollOutcome::Failed(message) => {
                return Err(SqueezyError::ProviderRequest(message));
            }
        }
    }
}

// ─── Copilot token refresh ─────────────────────────────────────────────────

/// Exchange a long-lived GitHub OAuth bearer for a short-lived
/// Copilot Chat API token. Used both during the initial login (after
/// the device-code flow returns the GitHub token) and on every
/// subsequent refresh — pi treats this exchange as the equivalent of
/// the OAuth refresh round-trip.
pub async fn refresh_copilot_token(
    client: &reqwest::Client,
    urls: &GitHubCopilotUrls,
    github_token: &str,
) -> Result<(String, u64)> {
    let response = client
        .get(&urls.copilot_token_url)
        .bearer_auth(github_token)
        .header("accept", "application/json")
        .header("user-agent", COPILOT_USER_AGENT)
        .header("editor-version", COPILOT_EDITOR_VERSION)
        .header("editor-plugin-version", COPILOT_EDITOR_PLUGIN_VERSION)
        .header("copilot-integration-id", COPILOT_INTEGRATION_ID)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!(
                "github-copilot token exchange request failed: {err}"
            ))
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|err| {
        SqueezyError::ProviderRequest(format!(
            "github-copilot token exchange body read failed: {err}"
        ))
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(SqueezyError::ProviderRequest(format!(
            "github-copilot token exchange returned {status}: {body}"
        )));
    }
    let parsed: CopilotTokenResponse = serde_json::from_slice(&bytes).map_err(|err| {
        let body = String::from_utf8_lossy(&bytes);
        SqueezyError::ProviderRequest(format!(
            "github-copilot token exchange response was not valid JSON: {err}; body={body}"
        ))
    })?;
    if parsed.expires_at <= 0 {
        return Err(SqueezyError::ProviderRequest(format!(
            "github-copilot token exchange returned non-positive expires_at: {}",
            parsed.expires_at
        )));
    }
    let absolute_ms = (parsed.expires_at as u64).saturating_mul(1000);
    let adjusted = absolute_ms
        .saturating_sub(REFRESH_LEAD_TIME.as_secs().saturating_mul(1000))
        .max(now_unix_ms().saturating_add(1));
    Ok((parsed.token, adjusted))
}

// ─── Model policy enablement ───────────────────────────────────────────────

/// Outcome of [`enable_models`]: per-model HTTP success flag so the
/// CLI can display a checklist without exposing the per-model errors
/// (which are usually "policy already enabled" 4xxs).
#[derive(Debug, Clone)]
pub struct PolicyEnablementOutcome {
    pub model_id: String,
    pub success: bool,
}

/// POST `/models/{id}/policy` for each model id, asking GitHub to
/// flip the user's per-model "enabled" flag. Best-effort: a failed
/// request does not abort the loop. Returns the per-model outcome so
/// the CLI can report it without surfacing each underlying error.
pub async fn enable_models(
    client: &reqwest::Client,
    base_url: &str,
    copilot_token: &str,
    model_ids: &[&str],
) -> Vec<PolicyEnablementOutcome> {
    let mut out = Vec::with_capacity(model_ids.len());
    let trimmed = base_url.trim_end_matches('/');
    for &model in model_ids {
        let url = format!("{trimmed}/models/{model}/policy");
        let success = client
            .post(&url)
            .bearer_auth(copilot_token)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("user-agent", COPILOT_USER_AGENT)
            .header("editor-version", COPILOT_EDITOR_VERSION)
            .header("editor-plugin-version", COPILOT_EDITOR_PLUGIN_VERSION)
            .header("copilot-integration-id", COPILOT_INTEGRATION_ID)
            .header("openai-intent", "chat-policy")
            .header("x-interaction-type", "chat-policy")
            .timeout(TOKEN_REQUEST_TIMEOUT)
            .json(&serde_json::json!({ "state": "enabled" }))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        out.push(PolicyEnablementOutcome {
            model_id: model.to_string(),
            success,
        });
    }
    out
}

// ─── Interactive login orchestrator ────────────────────────────────────────

/// Hooks the CLI implements to keep the interactive flow testable.
/// `on_device_code` is the user-presented `verification_uri` /
/// `user_code` pair; `on_browser_open` is called once with the
/// verification URI so the caller can attempt to open it (or skip
/// when `--no-browser`); `on_progress` carries free-form status
/// strings during the post-login policy step.
pub struct GitHubCopilotLoginHooks<'a> {
    pub on_device_code: &'a (dyn Fn(&DeviceCodeResponse) + Send + Sync),
    pub on_browser_open: &'a (dyn Fn(&str) + Send + Sync),
    pub on_progress: &'a (dyn Fn(&str) + Send + Sync),
}

/// Outcome of a successful login. The token set is already persisted
/// to `auth_path` by the time this returns.
#[derive(Debug, Clone)]
pub struct GitHubCopilotLoginOutcome {
    pub auth_file: PathBuf,
    pub enterprise_domain: Option<String>,
    pub copilot_token: String,
    pub expires_at_unix_ms: u64,
    pub policy_outcomes: Vec<PolicyEnablementOutcome>,
}

/// Drive the full device-code + token-exchange + policy-enablement
/// flow end-to-end. Persists the resulting token set to `auth_path`
/// before returning so a partial flow that aborts after the exchange
/// does not silently throw away the credentials the user just
/// approved.
#[allow(clippy::too_many_arguments)]
pub async fn login_github_copilot_interactive(
    enterprise_domain: Option<&str>,
    auth_path: &Path,
    hooks: &GitHubCopilotLoginHooks<'_>,
    cancel: &CancellationToken,
    skip_policy: bool,
    policy_models: &[&str],
) -> Result<GitHubCopilotLoginOutcome> {
    let domain = enterprise_domain.unwrap_or(DEFAULT_DOMAIN);
    let urls = GitHubCopilotUrls::for_domain(domain);
    let client = crate::transport::shared_client(&ProviderTransportConfig::default());

    let device = start_device_flow(&client, &urls).await?;
    (hooks.on_device_code)(&device);
    (hooks.on_browser_open)(&device.verification_uri);

    let github_token = poll_for_github_token(&client, &urls, &device, cancel).await?;

    (hooks.on_progress)("exchanging GitHub token for Copilot Chat token…");
    let (copilot_token, expires_at_unix_ms) =
        refresh_copilot_token(&client, &urls, &github_token).await?;

    let tokens = PersistedGitHubCopilotTokens {
        github_token,
        copilot_token: copilot_token.clone(),
        expires_at_unix_ms,
        enterprise_domain: enterprise_domain.map(str::to_string),
        provider: default_provider_tag(),
    };
    write_tokens(auth_path, &tokens)?;

    let policy_outcomes = if skip_policy || policy_models.is_empty() {
        Vec::new()
    } else {
        (hooks.on_progress)("enabling models…");
        let base_url = resolve_base_url(&copilot_token, enterprise_domain);
        enable_models(&client, &base_url, &copilot_token, policy_models).await
    };

    Ok(GitHubCopilotLoginOutcome {
        auth_file: auth_path.to_path_buf(),
        enterprise_domain: enterprise_domain.map(str::to_string),
        copilot_token,
        expires_at_unix_ms,
        policy_outcomes,
    })
}

// ─── ApiKeySource implementation ───────────────────────────────────────────

/// Refresh-aware [`ApiKeySource`] backed by a persisted Copilot
/// token set. State is held under an `Arc<RwLock<_>>` so a single
/// provider client can serve every concurrent request a session
/// issues without rebuilding the client when the token rotates.
pub struct GitHubCopilotOAuthSource {
    state: Arc<RwLock<InnerState>>,
    auth_path: PathBuf,
    urls: GitHubCopilotUrls,
    http_client: reqwest::Client,
    label: String,
}

#[derive(Debug)]
struct InnerState {
    tokens: PersistedGitHubCopilotTokens,
    /// Set by `invalidate` so the next `current_key` runs through the
    /// refresh path even when the cached expiry would otherwise pass
    /// the leeway gate.
    dirty: bool,
}

impl std::fmt::Debug for GitHubCopilotOAuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubCopilotOAuthSource")
            .field("auth_path", &self.auth_path)
            .field("copilot_token_url", &self.urls.copilot_token_url)
            .field("label", &self.label)
            .field("state", &"<redacted>")
            .finish()
    }
}

impl GitHubCopilotOAuthSource {
    /// Construct from already-known tokens. Used by the login flow
    /// (which has the freshly-exchanged token in hand) and by tests.
    pub fn from_tokens(tokens: PersistedGitHubCopilotTokens, auth_path: PathBuf) -> Self {
        let domain = tokens
            .enterprise_domain
            .clone()
            .unwrap_or_else(|| DEFAULT_DOMAIN.to_string());
        let urls = GitHubCopilotUrls::for_domain(&domain);
        Self {
            state: Arc::new(RwLock::new(InnerState {
                tokens,
                dirty: false,
            })),
            auth_path,
            urls,
            http_client: crate::transport::shared_client(&ProviderTransportConfig::default()),
            label: "github_copilot".to_string(),
        }
    }

    /// Test-friendly constructor that lets the integration tests
    /// point the refresh round-trip at a captive HTTP server.
    /// Production callers use [`Self::from_tokens`] (or load via
    /// [`Self::load`]) which derive `copilot_token_url` from the
    /// persisted enterprise domain.
    #[doc(hidden)]
    pub fn with_copilot_token_url(
        tokens: PersistedGitHubCopilotTokens,
        auth_path: PathBuf,
        copilot_token_url: impl Into<String>,
    ) -> Self {
        let urls = GitHubCopilotUrls {
            device_code_url: String::new(),
            access_token_url: String::new(),
            copilot_token_url: copilot_token_url.into(),
        };
        Self {
            state: Arc::new(RwLock::new(InnerState {
                tokens,
                dirty: false,
            })),
            auth_path,
            urls,
            http_client: crate::transport::shared_client(&ProviderTransportConfig::default()),
            label: "github_copilot".to_string(),
        }
    }

    /// Load tokens from the default
    /// `~/.squeezy/auth/github-copilot.json` path. Returns
    /// `ProviderNotConfigured` if no tokens have been persisted yet
    /// so the caller can hint the user toward `squeezy auth
    /// github-copilot login`.
    pub fn load() -> Result<Self> {
        let path = auth_file_path().ok_or_else(|| {
            SqueezyError::Config(
                "could not determine ~/.squeezy auth directory; \
                 set SQUEEZY_GITHUB_COPILOT_AUTH_FILE or HOME"
                    .to_string(),
            )
        })?;
        Self::load_from_path(path)
    }

    /// Load tokens from an explicit path. Returns
    /// `ProviderNotConfigured` when the file is absent.
    pub fn load_from_path(path: PathBuf) -> Result<Self> {
        let tokens = read_tokens(&path)?.ok_or_else(|| {
            SqueezyError::ProviderNotConfigured(format!(
                "no github-copilot OAuth credentials at {}; \
                 run `squeezy auth github-copilot login`",
                path.display()
            ))
        })?;
        Ok(Self::from_tokens(tokens, path))
    }

    /// Snapshot of the persisted tokens — useful for `auth status`
    /// style commands. The on-disk file remains the source of truth.
    pub async fn persisted_tokens(&self) -> PersistedGitHubCopilotTokens {
        self.state.read().await.tokens.clone()
    }

    /// Whether the cached Copilot token is past or near expiry.
    pub async fn needs_refresh(&self) -> bool {
        let guard = self.state.read().await;
        guard.dirty || guard.tokens.is_near_expiry(SystemTime::now())
    }

    /// Storage path the source persists to. Exposed for diagnostics
    /// (`auth status`, `doctor`).
    pub fn auth_path(&self) -> &Path {
        &self.auth_path
    }

    /// Force a refresh round-trip and persist the rotated Copilot
    /// token. Concurrent callers funnel through the same write lock,
    /// so two simultaneous `current_key` calls only fire one network
    /// request.
    pub async fn force_refresh(&self) -> Result<PersistedGitHubCopilotTokens> {
        let mut guard = self.state.write().await;
        // Re-check inside the lock: another writer may have refreshed
        // while we were queued.
        if !guard.dirty && !guard.tokens.is_near_expiry(SystemTime::now()) {
            return Ok(guard.tokens.clone());
        }
        let (copilot_token, expires_at_unix_ms) =
            refresh_copilot_token(&self.http_client, &self.urls, &guard.tokens.github_token)
                .await?;
        let rotated = PersistedGitHubCopilotTokens {
            github_token: guard.tokens.github_token.clone(),
            copilot_token,
            expires_at_unix_ms,
            enterprise_domain: guard.tokens.enterprise_domain.clone(),
            provider: default_provider_tag(),
        };
        // Persistence first; if the rename fails we still hold the
        // refreshed tokens in memory so the current turn proceeds,
        // but the next process restart will redo the round-trip.
        if let Err(err) = write_tokens(&self.auth_path, &rotated) {
            tracing::warn!(
                target: "squeezy_llm::oauth::github_copilot",
                "failed to persist refreshed github-copilot OAuth tokens to {}: {err}",
                self.auth_path.display()
            );
        }
        guard.tokens = rotated.clone();
        guard.dirty = false;
        Ok(rotated)
    }
}

impl ApiKeySource for GitHubCopilotOAuthSource {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        Box::pin(async move {
            {
                let guard = self.state.read().await;
                if !guard.dirty && !guard.tokens.is_near_expiry(SystemTime::now()) {
                    return Ok(guard.tokens.copilot_token.clone());
                }
            }
            let refreshed = self.force_refresh().await?;
            Ok(refreshed.copilot_token)
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            guard.dirty = true;
            Ok(())
        })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }

    fn can_rotate(&self) -> bool {
        true
    }
}

// ─── Provider client ───────────────────────────────────────────────────────

/// GitHub Copilot Chat provider client.
///
/// Wraps the existing [`OpenAiCompatibleProvider`] with the editor
/// headers VSCode's Copilot Chat extension sends and the
/// token-derived base URL. The underlying transport already speaks
/// Chat Completions on the wire, so streaming, retries, tool calls
/// and cancellation flow through unchanged.
#[derive(Debug)]
pub struct GitHubCopilotProvider {
    inner: OpenAiCompatibleProvider,
}

impl GitHubCopilotProvider {
    /// Build from a fully-constructed OAuth source. The base URL is
    /// extracted from the currently-cached Copilot token; if the
    /// proxy rotates between refreshes the user must re-log in (the
    /// per-account host is stable in normal operation).
    pub async fn from_source(
        source: Arc<GitHubCopilotOAuthSource>,
        transport: ProviderTransportConfig,
    ) -> Result<Self> {
        let tokens = source.persisted_tokens().await;
        let base_url = resolve_base_url(&tokens.copilot_token, tokens.enterprise_domain.as_deref());
        let inner = OpenAiCompatibleProvider::with_api_key_source(
            // `Custom` is the right preset for an OpenAI-compatible
            // host with no curated model registry on the squeezy
            // side. Copilot models are vendor-specific (Claude,
            // GPT-5.x, Gemini) and the wire flavor is plain Chat
            // Completions, so the namespace-aware tweaks
            // `OpenAiCompatibleProvider` applies elsewhere
            // (Anthropic cache markers, OpenRouter attribution)
            // would only confuse the Copilot endpoint.
            OpenAiCompatiblePreset::Custom,
            source as Arc<dyn ApiKeySource>,
            base_url,
            copilot_headers(),
            transport,
        );
        Ok(Self { inner })
    }

    /// Convenience: build a provider against the default
    /// `~/.squeezy/auth/github-copilot.json` token set. Returns
    /// `ProviderNotConfigured` if no tokens have been persisted yet.
    pub async fn from_default_auth(transport: ProviderTransportConfig) -> Result<Self> {
        let source = Arc::new(GitHubCopilotOAuthSource::load()?);
        Self::from_source(source, transport).await
    }
}

impl LlmProvider for GitHubCopilotProvider {
    fn name(&self) -> &'static str {
        "github_copilot"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        self.inner.stream_response(request, cancel)
    }
}

/// Editor headers the GitHub Copilot endpoint expects on every
/// request. Stamped on the provider client so they ride alongside
/// the rotating Bearer token without per-request bookkeeping.
pub fn copilot_headers() -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("user-agent".to_string(), COPILOT_USER_AGENT.to_string());
    headers.insert(
        "editor-version".to_string(),
        COPILOT_EDITOR_VERSION.to_string(),
    );
    headers.insert(
        "editor-plugin-version".to_string(),
        COPILOT_EDITOR_PLUGIN_VERSION.to_string(),
    );
    headers.insert(
        "copilot-integration-id".to_string(),
        COPILOT_INTEGRATION_ID.to_string(),
    );
    headers
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "github_copilot_tests.rs"]
mod tests;
