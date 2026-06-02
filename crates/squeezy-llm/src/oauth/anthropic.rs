//! Anthropic OAuth (Claude Pro/Max subscription) credential source.
//!
//! The flow:
//!
//! 1. The CLI runs `squeezy auth anthropic login`, which generates a
//!    PKCE pair and prints a `https://claude.ai/oauth/authorize?...`
//!    URL.
//! 2. The user completes login in their browser and is redirected to
//!    `http://localhost:54545/callback?code=...&state=...`. The CLI
//!    accepts either a callback-server capture or a manual paste of
//!    the code or full redirect URL.
//! 3. Squeezy exchanges the code at
//!    `https://platform.claude.com/v1/oauth/token` for a
//!    `{access_token, refresh_token, expires_in}` triple and persists
//!    it to `~/.squeezy/auth/anthropic.json` (mode 0600).
//! 4. At request time the [`AnthropicOAuthSource`] returns the cached
//!    access token if it has more than ~60 s of life left; otherwise it
//!    swaps a fresh access token in under an `RwLock` and rewrites the
//!    persisted file so concurrent provider clients all see the new
//!    value.
//!
//! The provider client side picks OAuth versus API-key auth by
//! sniffing the token prefix (`sk-ant-oat`) — that keeps the trait
//! surface narrow without forcing every existing static-key caller to
//! opt in.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use squeezy_core::{ProviderTransportConfig, Result, SqueezyError};
use tokio::sync::RwLock;

use crate::credentials::{ApiKeyFuture, ApiKeySource, TokenState};
use crate::transport::shared_client;

use super::pkce::PkceCodes;

/// Anthropic Claude Code public OAuth client id — Anthropic registers
/// this client with the platform OAuth server and pins the exact
/// redirect URI below, so any other id will be rejected.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Authorize endpoint. Hosted on `claude.ai` so the consent UI is
/// branded for the user's subscription.
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token endpoint. Distinct host from the authorize URL because the
/// platform service issues the actual access/refresh tokens.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Default callback the platform OAuth server is allowed to redirect
/// to. Anthropic pins the exact value. The `/callback` suffix is part
/// of the URI Anthropic registered, not a separately routed path.
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";

/// OAuth scopes Claude Code requests. The Pro/Max quota is gated on
/// `user:inference` + the `claude_code` session scope; the rest match
/// the consent screen users have already approved for Claude Code.
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// `anthropic-beta` value that flags a request as Claude-Code-issued
/// so it counts against the user's Pro/Max subscription quota rather
/// than the API-key billing path. Anthropic pins both halves of the
/// joined value.
pub const OAUTH_BETA_HEADER: &str = "claude-code-20250219,oauth-2025-04-20";

/// Prefix every Anthropic OAuth access token starts with. Used to
/// detect OAuth-driven sources at the HTTP layer without changing the
/// [`ApiKeySource`] trait.
pub const ANTHROPIC_OAUTH_TOKEN_PREFIX: &str = "sk-ant-oat";

/// Cushion between the issuer-reported expiry and the moment we refresh
/// proactively. Five minutes ensures a long-running streaming response
/// never starts on a key that's about to die mid-flight.
const REFRESH_LEAD_TIME: Duration = Duration::from_secs(5 * 60);

/// HTTP timeout for the token exchange and refresh round-trips. The
/// platform endpoint is normally sub-second; 30 s gives a wide
/// envelope for slow networks without letting a stuck connection
/// hang the agent.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// On-disk representation of the persisted OAuth tokens. Stored at
/// `~/.squeezy/auth/anthropic.json` with mode 0600 on Unix.
///
/// `expires_at_unix_ms` is an absolute epoch instant rather than the
/// raw `expires_in` so a process restart can decide on its own
/// whether the access token is still good without re-running the
/// clock math.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Free-form provider tag so a future multi-account refactor can
    /// tell two persisted accounts apart. Currently always
    /// `"anthropic-oauth"`.
    #[serde(default = "default_provider_tag")]
    pub provider: String,
}

fn default_provider_tag() -> String {
    "anthropic-oauth".to_string()
}

impl PersistedTokens {
    /// Build from the raw token endpoint response.
    pub fn from_token_response(response: &TokenResponse, now_ms: u64) -> Self {
        let expires_at_unix_ms = now_ms
            .saturating_add(response.expires_in.saturating_mul(1000))
            .saturating_sub(REFRESH_LEAD_TIME.as_secs().saturating_mul(1000));
        Self {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
            expires_at_unix_ms,
            scope: response.scope.clone(),
            provider: default_provider_tag(),
        }
    }

    fn to_token_state(&self) -> TokenState {
        TokenState {
            access_token: self.access_token.clone(),
            refresh_token: Some(self.refresh_token.clone()),
            expires_at: Some(UNIX_EPOCH + Duration::from_millis(self.expires_at_unix_ms)),
        }
    }
}

/// Raw shape returned by `platform.claude.com/v1/oauth/token` for
/// both `authorization_code` and `refresh_token` grants. `scope` is
/// optional because the refresh response sometimes omits it.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Configuration knobs for the login flow. Exposed so tests can point
/// the URLs at a local mock server without monkey-patching
/// constants.
#[derive(Debug, Clone)]
pub struct AnthropicLoginConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    pub scopes: String,
}

impl Default for AnthropicLoginConfig {
    fn default() -> Self {
        Self {
            client_id: CLIENT_ID.to_string(),
            authorize_url: AUTHORIZE_URL.to_string(),
            token_url: TOKEN_URL.to_string(),
            redirect_uri: REDIRECT_URI.to_string(),
            scopes: SCOPES.to_string(),
        }
    }
}

impl AnthropicLoginConfig {
    /// Build the `https://claude.ai/oauth/authorize?...` URL the user
    /// opens in their browser. `code=true` is the platform's
    /// idiosyncratic opt-in for the auth-code flow.
    pub fn authorize_url(&self, codes: &PkceCodes) -> String {
        let params = [
            ("code", "true"),
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", self.scopes.as_str()),
            ("code_challenge", codes.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", codes.verifier.as_str()),
        ];
        let query_len = params
            .iter()
            .map(|(key, value)| key.len() + 1 + value.len())
            .sum::<usize>()
            + params.len().saturating_sub(1);
        let mut url = String::with_capacity(self.authorize_url.len() + 1 + query_len);
        url.push_str(&self.authorize_url);
        url.push('?');
        for (index, (key, value)) in params.iter().enumerate() {
            if index > 0 {
                url.push('&');
            }
            url.push_str(key);
            url.push('=');
            push_url_encoded(&mut url, value);
        }
        url
    }
}

/// Parsed authorization input — what the user pastes into the prompt
/// (or what the local callback captured). The CLI accepts a bare code,
/// a `code#state` joined string, a query-string fragment, or the full
/// redirect URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAuthorization {
    pub code: Option<String>,
    pub state: Option<String>,
}

/// Parse the raw input the user pasted into the login prompt. Returns
/// `code` and `state` independently so the caller can validate state
/// against the verifier separately.
pub fn parse_authorization_input(input: &str) -> ParsedAuthorization {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return ParsedAuthorization {
            code: None,
            state: None,
        };
    }

    // 1) Full URL form.
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" if !value.trim().is_empty() => code = Some(value.into_owned()),
                "state" if !value.trim().is_empty() => state = Some(value.into_owned()),
                _ => {}
            }
        }
        if code.is_some() || state.is_some() {
            return ParsedAuthorization { code, state };
        }
    }

    // 2) `code#state` joined form (the compact callback display).
    if trimmed.contains('#') && !trimmed.contains('=') {
        let mut parts = trimmed.splitn(2, '#');
        let code = parts.next().map(str::to_string).filter(|s| !s.is_empty());
        let state = parts.next().map(str::to_string).filter(|s| !s.is_empty());
        return ParsedAuthorization { code, state };
    }

    // 3) `code=...&state=...` query-string fragment form.
    if trimmed.contains("code=") {
        let mut code = None;
        let mut state = None;
        for pair in trimmed.trim_start_matches('?').split('&') {
            let mut kv = pair.splitn(2, '=');
            let key = kv.next().unwrap_or("");
            let value = kv.next().unwrap_or("");
            match key {
                "code" if !value.is_empty() => code = Some(url_decode(value)),
                "state" if !value.is_empty() => state = Some(url_decode(value)),
                _ => {}
            }
        }
        return ParsedAuthorization { code, state };
    }

    // 4) Bare code — the user pasted just the authorization code
    //    without any wrapping URL.
    ParsedAuthorization {
        code: Some(trimmed.to_string()),
        state: None,
    }
}

/// Convenience: convenience wrapper around [`is_anthropic_oauth_token`].
///
/// `true` for any access token Anthropic issues through the OAuth
/// flow. Used by the provider HTTP path to switch from `x-api-key` to
/// `Authorization: Bearer` and inject the Claude Code identity
/// headers + system prompt.
pub fn is_anthropic_oauth_token(token: &str) -> bool {
    token.starts_with(ANTHROPIC_OAUTH_TOKEN_PREFIX)
}

/// Returns the `anthropic-beta` header value that marks a request as
/// Claude-Code-issued. Provider clients merge this with any
/// caller-supplied beta opt-ins so the OAuth quota path always lights
/// up alongside other betas (extended thinking, 1M context, etc.).
pub fn anthropic_oauth_beta_header() -> &'static str {
    OAUTH_BETA_HEADER
}

/// Exchange an authorization code for a token pair at the platform
/// OAuth endpoint. The `state` is passed through verbatim — Anthropic's
/// token endpoint accepts it as an extra field even though PKCE alone
/// would suffice.
pub async fn exchange_authorization_code(
    client: &reqwest::Client,
    config: &AnthropicLoginConfig,
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "redirect_uri": config.redirect_uri,
        "code_verifier": verifier,
    });
    post_token_request(client, &config.token_url, &body).await
}

/// Trade a refresh token for a fresh access/refresh pair. Returns the
/// raw response so callers can decide how to persist it.
pub async fn refresh_anthropic_token(
    client: &reqwest::Client,
    config: &AnthropicLoginConfig,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": config.client_id,
        "refresh_token": refresh_token,
    });
    post_token_request(client, &config.token_url, &body).await
}

async fn post_token_request(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<TokenResponse> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .json(body)
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("Anthropic OAuth POST failed: {err}"))
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|err| {
        SqueezyError::ProviderRequest(format!("Anthropic OAuth body read failed: {err}"))
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(SqueezyError::ProviderRequest(format!(
            "Anthropic OAuth token endpoint returned {status}: {body}"
        )));
    }
    serde_json::from_slice::<TokenResponse>(&bytes).map_err(|err| {
        let body = String::from_utf8_lossy(&bytes);
        SqueezyError::ProviderRequest(format!(
            "Anthropic OAuth token response was not valid JSON: {err}; body={body}"
        ))
    })
}

/// Default location of the persisted OAuth tokens. Honors
/// `SQUEEZY_ANTHROPIC_OAUTH_FILE` so tests and unusual deployments can
/// redirect it without touching the user's real home directory.
pub fn default_storage_path() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("SQUEEZY_ANTHROPIC_OAUTH_FILE")
        && !explicit.trim().is_empty()
    {
        return Ok(PathBuf::from(explicit));
    }
    let home = dirs::home_dir().ok_or_else(|| {
        SqueezyError::Config(
            "could not resolve home directory for ~/.squeezy/auth/anthropic.json".to_string(),
        )
    })?;
    Ok(home.join(".squeezy").join("auth").join("anthropic.json"))
}

/// Read persisted tokens from disk. Returns `Ok(None)` when the file
/// is absent — that's the natural state before the first login —
/// and a hard error when the file exists but can't be parsed.
pub fn read_tokens(path: &Path) -> Result<Option<PersistedTokens>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let tokens: PersistedTokens = serde_json::from_slice(&bytes).map_err(|err| {
                SqueezyError::Config(format!("failed to parse {}: {err}", path.display()))
            })?;
            Ok(Some(tokens))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(SqueezyError::Config(format!(
            "failed to read {}: {err}",
            path.display()
        ))),
    }
}

/// Write persisted tokens to disk, creating the parent directory
/// (mode 0700 on Unix) and forcing 0600 on the file itself. The
/// write is best-effort atomic: write to `<path>.tmp` and rename so a
/// crash mid-write doesn't leave a half-written token file behind.
pub fn write_tokens(path: &Path, tokens: &PersistedTokens) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            SqueezyError::Config(format!(
                "failed to create {} for Anthropic OAuth tokens: {err}",
                parent.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let serialized = serde_json::to_vec_pretty(tokens).map_err(|err| {
        SqueezyError::Config(format!("failed to serialize Anthropic OAuth tokens: {err}"))
    })?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &serialized).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to write {} for Anthropic OAuth tokens: {err}",
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
            "failed to rename {} to {} for Anthropic OAuth tokens: {err}",
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

/// Pluggable OAuth-driven [`ApiKeySource`] for Claude Pro/Max
/// subscriptions. Holds the live token triple under
/// `Arc<RwLock<TokenState>>` (so concurrent provider calls all see
/// the same refresh) plus enough metadata to persist + refresh
/// without an external coordinator.
pub struct AnthropicOAuthSource {
    state: Arc<RwLock<InnerState>>,
    storage_path: PathBuf,
    config: AnthropicLoginConfig,
    http: reqwest::Client,
    label: String,
}

#[derive(Debug)]
struct InnerState {
    tokens: PersistedTokens,
    /// `true` after [`ApiKeySource::invalidate`] until the next
    /// successful refresh — forces `current_key` to refresh even when
    /// the cached expiry would otherwise pass the lead-time gate.
    dirty: bool,
    /// Last error surfaced by [`AnthropicOAuthSource::force_refresh`].
    /// When populated, the next [`ApiKeySource::current_key`] call
    /// short-circuits and returns the cached error string rather than
    /// re-firing the same failed network round-trip. `invalidate()`
    /// clears the flag so an operator-triggered re-login can retry
    /// the refresh. See `.audit/providers/anthropic.md` HIGH #5.
    last_refresh_err: Option<String>,
}

impl AnthropicOAuthSource {
    /// Build a source from already-known tokens. Used by the login
    /// flow (which has the freshly-exchanged tokens in hand) and by
    /// tests.
    pub fn from_tokens(tokens: PersistedTokens, storage_path: PathBuf) -> Self {
        Self::with_parts(
            tokens,
            storage_path,
            AnthropicLoginConfig::default(),
            shared_client(&ProviderTransportConfig::default()),
        )
    }

    /// Full-parameter constructor — exposed so tests can swap the
    /// HTTP client and point `token_url` at a local mock.
    pub fn with_parts(
        tokens: PersistedTokens,
        storage_path: PathBuf,
        config: AnthropicLoginConfig,
        http: reqwest::Client,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState {
                tokens,
                dirty: false,
                last_refresh_err: None,
            })),
            storage_path,
            config,
            http,
            label: "anthropic-oauth".to_string(),
        }
    }

    /// Load tokens from the default `~/.squeezy/auth/anthropic.json`
    /// path. Returns `ProviderNotConfigured` if no tokens have been
    /// persisted yet so the caller can hint the user toward `squeezy
    /// auth anthropic login`.
    pub fn load() -> Result<Self> {
        Self::load_from_path(default_storage_path()?)
    }

    /// Load tokens from an explicit path. Returns
    /// `ProviderNotConfigured` when the file is absent.
    pub fn load_from_path(path: PathBuf) -> Result<Self> {
        let tokens = read_tokens(&path)?.ok_or_else(|| {
            SqueezyError::ProviderNotConfigured(format!(
                "no Anthropic OAuth credentials at {}; run `squeezy auth anthropic login`",
                path.display()
            ))
        })?;
        Ok(Self::from_tokens(tokens, path))
    }

    /// Snapshot of the [`TokenState`] mirror — same shape as
    /// [`crate::credentials::RefreshableToken::state_handle`] so the
    /// existing test scaffolding can observe in-place rotation.
    pub async fn token_state(&self) -> TokenState {
        self.state.read().await.tokens.to_token_state()
    }

    /// Whether the cached access token is past or near expiry
    /// (`<60s` of life left, or `dirty` after an invalidate).
    pub async fn needs_refresh(&self) -> bool {
        let guard = self.state.read().await;
        guard.dirty || access_token_is_stale(&guard.tokens)
    }

    /// Persisted-tokens snapshot — useful for `auth status` style
    /// commands. Does not include the in-memory `dirty` flag because
    /// callers should treat the disk file as the source of truth.
    pub async fn persisted_tokens(&self) -> PersistedTokens {
        self.state.read().await.tokens.clone()
    }

    /// Refresh the access token using the stored refresh token, persist
    /// the new triple to disk, and return the resulting tokens.
    ///
    /// Concurrent callers funnel through the same write lock, so two
    /// simultaneous `current_key` calls only fire one network request.
    pub async fn force_refresh(&self) -> Result<PersistedTokens> {
        let mut guard = self.state.write().await;
        // Re-check inside the lock: another writer may have refreshed
        // while we were queued.
        if !guard.dirty && !access_token_is_stale(&guard.tokens) {
            return Ok(guard.tokens.clone());
        }
        // Short-circuit when a prior refresh failed and `invalidate()`
        // has not been called since: re-firing the same network
        // round-trip against a revoked refresh token would just burn
        // quota and slow the auth error to surface. Operators clear
        // the cached error by calling `invalidate()` (e.g. after
        // `squeezy auth anthropic login`).
        if let Some(cached) = guard.last_refresh_err.as_deref() {
            return Err(SqueezyError::ProviderRequest(cached.to_string()));
        }
        let refresh_token = guard.tokens.refresh_token.clone();
        let response = match refresh_anthropic_token(&self.http, &self.config, &refresh_token).await
        {
            Ok(response) => response,
            Err(err) => {
                // Keep `dirty=true` so the next caller still sees the
                // stale token as needing refresh. Only *permanent*
                // failures (the refresh token is revoked / rejected:
                // token-endpoint HTTP 400/401/403 or an `invalid_grant`
                // body) latch into `last_refresh_err` so concurrent /
                // repeat callers don't re-fire a POST that can only
                // fail the same way until an explicit `invalidate()`
                // (the operator-triggered re-login path) wipes it.
                //
                // Transient failures — network errors ("POST failed" /
                // "body read failed"), 5xx, timeouts, or a malformed
                // body from a flaky proxy — are deliberately NOT
                // cached: latching them would permanently wedge the
                // source for the process lifetime, because the only
                // caller that ever reaches `invalidate()` is the
                // messages-endpoint 401/403 path, which `current_key()`
                // short-circuits before any request is sent. Letting
                // them fall through means the next `current_key()`
                // simply retries the refresh.
                let message = err.to_string();
                if refresh_error_is_permanent(&message) {
                    guard.last_refresh_err = Some(message);
                }
                return Err(err);
            }
        };
        let now_ms = current_unix_ms();
        let tokens = PersistedTokens::from_token_response(&response, now_ms);
        // Persistence first; if the rename fails we still hold the
        // refreshed tokens in memory so the current turn proceeds,
        // but the next process restart will redo the round-trip.
        if let Err(err) = write_tokens(&self.storage_path, &tokens) {
            tracing::warn!(
                target: "squeezy_llm::oauth::anthropic",
                "failed to persist refreshed Anthropic OAuth tokens to {}: {err}",
                self.storage_path.display()
            );
        }
        guard.tokens = tokens.clone();
        guard.dirty = false;
        guard.last_refresh_err = None;
        Ok(tokens)
    }

    /// Storage path the source persists to. Exposed for diagnostics
    /// (`auth status`, `doctor`).
    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }
}

impl std::fmt::Debug for AnthropicOAuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicOAuthSource")
            .field("label", &self.label)
            .field("storage_path", &self.storage_path)
            .field("token_url", &self.config.token_url)
            .field("state", &"<redacted>")
            .finish()
    }
}

impl ApiKeySource for AnthropicOAuthSource {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        Box::pin(async move {
            // Fast path: read-lock and serve the cached token if it
            // still has comfortable life left and no invalidate has
            // been requested.
            {
                let guard = self.state.read().await;
                if !guard.dirty && !access_token_is_stale(&guard.tokens) {
                    return Ok(guard.tokens.access_token.clone());
                }
            }
            let refreshed = self.force_refresh().await?;
            Ok(refreshed.access_token)
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            guard.dirty = true;
            // Wipe the cached refresh error so the operator-triggered
            // re-invalidate path (e.g. after a fresh `squeezy auth
            // anthropic login`) can fire the refresh again instead of
            // forever returning the stale error.
            guard.last_refresh_err = None;
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

fn access_token_is_stale(tokens: &PersistedTokens) -> bool {
    let expires_at = UNIX_EPOCH + Duration::from_millis(tokens.expires_at_unix_ms);
    let now = SystemTime::now();
    // Refresh proactively when there's less than 60 s of life left
    // *relative to the already-shifted expiry stamp*.
    // `tokens.expires_at_unix_ms` is the issuer-reported absolute
    // expiry minus [`REFRESH_LEAD_TIME`] (5 minutes) — see
    // [`PersistedTokens::from_token_response`]. So the effective
    // refresh fires roughly 6 minutes ahead of the real platform
    // expiry, which is intentional: a streaming response can run for
    // tens of seconds and we'd rather pay one extra refresh than die
    // mid-stream. Total cushion = 5m persisted + 60s runtime gate.
    // See `.audit/providers/anthropic.md` MEDIUM #8.
    let runtime_gate = Duration::from_secs(60);
    match expires_at.checked_sub(runtime_gate) {
        Some(threshold) => threshold <= now,
        None => true,
    }
}

/// Classify a [`refresh_anthropic_token`] failure as permanent (the
/// refresh token will never succeed again without a re-login) versus
/// transient (a retry on the next `current_key()` has a real chance of
/// succeeding).
///
/// The error is stringly-typed (`SqueezyError::ProviderRequest` carries
/// only a message), so we match on the shapes [`post_token_request`]
/// produces:
///
/// * `"Anthropic OAuth token endpoint returned {status}: {body}"` —
///   permanent for HTTP 400/401/403 or any body mentioning
///   `invalid_grant` (the platform's signal that the refresh token is
///   revoked/expired); a 5xx from this same shape is transient.
/// * `"Anthropic OAuth POST failed: ..."` / `"... body read failed: ..."`
///   — transport-level (DNS, connect, timeout, reset); transient.
/// * `"Anthropic OAuth token response was not valid JSON: ..."` — a
///   flaky proxy or partial body; transient.
///
/// Only permanent failures latch into `last_refresh_err`; anything we
/// can't positively classify as permanent is treated as transient so a
/// single hiccup never wedges the source for the process lifetime.
fn refresh_error_is_permanent(message: &str) -> bool {
    // An explicit `invalid_grant` anywhere in the surfaced body is the
    // canonical "this refresh token is dead" signal regardless of the
    // HTTP status the platform paired it with.
    if message.contains("invalid_grant") {
        return true;
    }
    // Otherwise only the HTTP-status error shape can be permanent, and
    // only for the auth-class 4xx codes. A 5xx (or any other status)
    // is transient.
    if let Some(rest) = message.strip_prefix("Anthropic OAuth token endpoint returned ") {
        let status = rest
            .split_once(':')
            .map(|(status, _)| status.trim())
            .unwrap_or_else(|| rest.trim());
        // `status` is the `Display` of `reqwest::StatusCode`, e.g.
        // "400 Bad Request"; the leading numeric code is enough.
        let code = status
            .split_whitespace()
            .next()
            .and_then(|code| code.parse::<u16>().ok());
        return matches!(code, Some(400) | Some(401) | Some(403));
    }
    false
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn push_url_encoded(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
