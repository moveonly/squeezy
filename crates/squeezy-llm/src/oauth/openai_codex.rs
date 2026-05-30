//! ChatGPT Plus/Pro subscription auth via the OpenAI Codex OAuth flow.
//!
//! Uses OpenAI's published OAuth constants — `client_id`, the
//! `auth.openai.com` authorize and token endpoints, the
//! `http://localhost:1455` redirect, and the
//! `openid profile email offline_access` scope set — so the resulting
//! access token is interchangeable with credentials minted by the
//! Codex CLI.
//!
//! Layout:
//! 1. PKCE + state generation (`getrandom` + SHA-256), authorize URL
//!    construction.
//! 2. Token exchange and refresh against `https://auth.openai.com`.
//! 3. JWT inspection to lift `chatgpt_account_id` out of the access
//!    token claims so [`OpenAiCodexProvider`] can stamp the
//!    subscription account on every request.
//! 4. [`OpenAiCodexTokenSet`] persistence at
//!    `~/.squeezy/auth/openai-codex.json` (mode `0o600` on Unix).
//! 5. [`OpenAiCodexOAuthSource`]: the [`ApiKeySource`]
//!    implementation. `current_key` returns the cached access token,
//!    refreshing automatically when the absolute expiry is within the
//!    safety window or after `invalidate`.
//! 6. [`OpenAiCodexProvider`]: a wire-compatible OpenAI Responses
//!    client that posts to the ChatGPT backend with the codex headers
//!    pi sets (`chatgpt-account-id`, `originator`, `OpenAI-Beta:
//!    responses=experimental`) and forces `store=false` (the backend
//!    rejects `store=true`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use squeezy_core::{OpenAiCodexConfig, ProviderTransportConfig, Result, SqueezyError};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::credentials::{ApiKeyFuture, ApiKeySource};
use crate::openai::{ReasoningAccumulator, parse_openai_event};
use crate::retry::{RetryPolicy, idle_timeout, send_with_auth_retry};
use crate::sse::SseDecoder;
use crate::transport::shared_client;
use crate::{LlmEvent, LlmProvider, LlmRequest, LlmStream, OpenAiProvider};

// ─── Public OAuth constants ────────────────────────────────────────────────

/// OAuth client id registered by OpenAI for the Codex CLI flow.
/// Public information; intentionally hard-coded so squeezy's flow is
/// wire-identical to the reference Codex CLI.
pub const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const OPENAI_CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const OPENAI_CODEX_SCOPE: &str = "openid profile email offline_access";
/// JWT custom claim path under which OpenAI stores subscription auth
/// metadata (`chatgpt_account_id`). The URL form is the literal claim
/// key, not an HTTP destination.
pub const OPENAI_CODEX_JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
pub const OPENAI_CODEX_CALLBACK_HOST: &str = "127.0.0.1";
pub const OPENAI_CODEX_CALLBACK_PORT: u16 = 1455;
/// Filename under `~/.squeezy/auth/` where the persisted Codex token
/// set lives. Hyphenated to match the Codex CLI's filename; the rest
/// of the codebase stays on snake_case provider section names.
pub const OPENAI_CODEX_AUTH_FILE_NAME: &str = "openai-codex.json";

/// Force a refresh when the cached access token has less than this
/// much life left. Mirrors a typical "refresh-on-near-expiry" budget
/// — the OAuth token lasts ~1 hour, so a one-minute safety window
/// keeps the next provider request from racing the expiry.
const REFRESH_LEEWAY: Duration = Duration::from_secs(60);

/// Number of random bytes the PKCE verifier and OAuth state nonce
/// consume. RFC 7636 §4.1 recommends 32 bytes for the verifier so the
/// base64url-encoded form lands at 43 chars; the state nonce uses 16
/// bytes for a 22-char value.
const PKCE_VERIFIER_BYTES: usize = 32;
const PKCE_STATE_BYTES: usize = 16;

// ─── Token set + persistence ───────────────────────────────────────────────

/// Persisted Codex credential set. Field names follow the OAuth
/// `credentials` shape so a future tool can read both stores in the
/// same struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCodexTokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// Absolute expiry as Unix milliseconds.
    pub expires_at_unix_ms: u64,
    pub account_id: String,
}

impl OpenAiCodexTokenSet {
    fn expires_at_systemtime(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.expires_at_unix_ms)
    }

    fn is_near_expiry(&self, now: SystemTime) -> bool {
        let cutoff = self.expires_at_systemtime();
        let safe_until = cutoff.checked_sub(REFRESH_LEEWAY).unwrap_or(UNIX_EPOCH);
        now >= safe_until
    }
}

/// Path to the persisted Codex auth file. Honors
/// `SQUEEZY_OPENAI_CODEX_AUTH_FILE` so tests and unusual deployments
/// can redirect persistence without touching the user's real
/// `~/.squeezy/auth/openai-codex.json`.
pub fn codex_auth_file_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("SQUEEZY_OPENAI_CODEX_AUTH_FILE")
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    default_codex_auth_path()
}

/// Canonical persistence path: `<home>/.squeezy/auth/openai-codex.json`.
/// Separated from [`codex_auth_file_path`] so callers (and tests) can
/// inspect the default location without consulting env vars.
pub fn default_codex_auth_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".squeezy")
            .join("auth")
            .join(OPENAI_CODEX_AUTH_FILE_NAME),
    )
}

/// Load the token set from disk. Returns `Ok(None)` when the file is
/// missing; surfaces a typed error for malformed JSON or filesystem
/// trouble so the caller can fail loudly instead of silently logging
/// the user out.
pub fn load_codex_token(path: &Path) -> Result<Option<OpenAiCodexTokenSet>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "could not read codex auth file {}: {err}",
                path.display()
            )));
        }
    };
    let token: OpenAiCodexTokenSet = serde_json::from_str(&text).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!(
            "codex auth file {} is not valid JSON: {err}",
            path.display()
        ))
    })?;
    Ok(Some(token))
}

/// Persist the token set with `chmod 600` on Unix. The parent
/// directory is created with `0o700`; on Windows we fall back to the
/// default permission set (the AGENTS guidance notes Windows sandbox
/// is best-effort).
pub fn save_codex_token(path: &Path, token: &OpenAiCodexTokenSet) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            SqueezyError::Config(format!(
                "could not create codex auth directory {}: {err}",
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
    let json = serde_json::to_string_pretty(token)
        .map_err(|err| SqueezyError::Config(format!("could not serialize codex token: {err}")))?;
    std::fs::write(path, json).map_err(|err| {
        SqueezyError::Config(format!(
            "could not write codex auth file {}: {err}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|err| {
                SqueezyError::Config(format!(
                    "could not stat codex auth file {}: {err}",
                    path.display()
                ))
            })?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|err| {
            SqueezyError::Config(format!(
                "could not chmod 600 codex auth file {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

// ─── PKCE + base64url helpers ──────────────────────────────────────────────

/// Generate `n` cryptographically secure random bytes via the OS RNG.
/// Wraps [`getrandom::fill`] so the rest of the module can ignore
/// the platform-specific failure modes.
fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|err| {
        SqueezyError::Config(format!(
            "could not read {n} random bytes from the OS RNG: {err}"
        ))
    })?;
    Ok(bytes)
}

/// URL-safe base64 without padding, the encoding required for PKCE
/// verifier / challenge values (RFC 7636 §4.1, §4.2).
fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = bytes[i + 1] as u32;
        let b2 = bytes[i + 2] as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let remaining = bytes.len() - i;
    if remaining == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
    } else if remaining == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
    }
    out
}

/// Permissive base64 decoder used for JWT payload extraction. Accepts
/// both URL-safe (`-`/`_`) and standard (`+`/`/`) alphabets and
/// tolerates missing padding so it can decode JWT segments straight
/// off the wire.
fn base64_decode_loose(input: &str) -> Option<Vec<u8>> {
    let mut value = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '-' => value.push('+'),
            '_' => value.push('/'),
            c if c.is_alphanumeric() || c == '+' || c == '/' => value.push(c),
            '=' => break,
            _ => return None,
        }
    }
    while !value.len().is_multiple_of(4) {
        value.push('=');
    }
    let mut out = Vec::with_capacity(value.len() / 4 * 3);
    let mut buf = [0u8; 4];
    let mut filled = 0;
    for ch in value.chars() {
        if ch == '=' {
            break;
        }
        let code = match ch {
            'A'..='Z' => ch as u8 - b'A',
            'a'..='z' => ch as u8 - b'a' + 26,
            '0'..='9' => ch as u8 - b'0' + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buf[filled] = code;
        filled += 1;
        if filled == 4 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
            out.push((buf[1] << 4) | (buf[2] >> 2));
            out.push((buf[2] << 6) | buf[3]);
            filled = 0;
        }
    }
    match filled {
        0 => {}
        2 => out.push((buf[0] << 2) | (buf[1] >> 4)),
        3 => {
            out.push((buf[0] << 2) | (buf[1] >> 4));
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        _ => return None,
    }
    Some(out)
}

/// PKCE pair: a random URL-safe verifier and its SHA-256/base64url
/// challenge. Exposed `pub(crate)` for the login orchestrator and
/// tests; not part of the public crate surface.
pub(crate) struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

pub(crate) fn generate_pkce() -> Result<PkcePair> {
    let verifier_bytes = random_bytes(PKCE_VERIFIER_BYTES)?;
    let verifier = base64url_encode(&verifier_bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge_bytes = hasher.finalize();
    let challenge = base64url_encode(&challenge_bytes);
    Ok(PkcePair {
        verifier,
        challenge,
    })
}

/// 16 random bytes → base64url; matches the entropy budget pi uses for
/// its hex `state`. The CSRF check is byte-comparison so either
/// encoding works as long as both sides see the same value.
pub(crate) fn generate_state() -> Result<String> {
    let state = random_bytes(PKCE_STATE_BYTES)?;
    Ok(base64url_encode(&state))
}

/// Build the OAuth authorization URL the user opens in their browser.
/// `originator` is stamped through as a free-form attribution tag so
/// OpenAI can attribute traffic to squeezy in their dashboards.
pub(crate) fn build_authorize_url(challenge: &str, state: &str, originator: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
        ("redirect_uri", OPENAI_CODEX_REDIRECT_URI),
        ("scope", OPENAI_CODEX_SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", originator),
    ];
    let mut url = String::from(OPENAI_CODEX_AUTHORIZE_URL);
    url.push('?');
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        url.push_str(k);
        url.push('=');
        url.push_str(&url_form_encode(v));
    }
    url
}

/// Form-url-encode a single component. Limited to the
/// space/`+`/`&`/`=`/`?`/`#`/`/` cases relevant to query strings;
/// pulls in no new dependencies.
fn url_form_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

// ─── JWT account-id extraction ─────────────────────────────────────────────

/// Decode the access token's middle JWT segment and pluck the
/// `chatgpt_account_id` claim. Returns a typed error so the login
/// flow can guide the user to a working ChatGPT subscription account
/// instead of failing silently.
pub(crate) fn extract_account_id(access_token: &str) -> Result<String> {
    let mut parts = access_token.split('.');
    let _header = parts.next();
    let payload = parts.next().ok_or_else(|| {
        SqueezyError::ProviderNotConfigured(
            "openai codex access token is not a JWT (missing payload segment)".to_string(),
        )
    })?;
    let bytes = base64_decode_loose(payload).ok_or_else(|| {
        SqueezyError::ProviderNotConfigured(
            "openai codex access token payload is not valid base64".to_string(),
        )
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!(
            "openai codex access token payload is not valid JSON: {err}"
        ))
    })?;
    let account_id = value
        .get(OPENAI_CODEX_JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            SqueezyError::ProviderNotConfigured(
                "openai codex access token has no chatgpt_account_id claim; \
                 sign in to a ChatGPT Plus or Pro account"
                    .to_string(),
            )
        })?;
    Ok(account_id.to_string())
}

// ─── Token endpoint round trips ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn parse_token_response(body: &str) -> Result<TokenResponse> {
    serde_json::from_str(body).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!(
            "openai codex token endpoint returned unexpected JSON: {err}"
        ))
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Exchange an authorization code for a token set against `token_url`.
/// Public so the login orchestrator and tests can hit a captive
/// endpoint; the `token_url` knob exists exclusively for the test
/// suite — production callers go through [`login_openai_codex_interactive`].
pub(crate) async fn exchange_authorization_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OpenAiCodexTokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    let response = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderNotConfigured(format!(
                "openai codex token exchange failed: {err}"
            ))
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "openai codex token exchange failed ({status}): {body}"
        )));
    }
    let parsed = parse_token_response(&body)?;
    let account_id = extract_account_id(&parsed.access_token)?;
    Ok(OpenAiCodexTokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at_unix_ms: now_unix_ms().saturating_add(parsed.expires_in.saturating_mul(1000)),
        account_id,
    })
}

/// Refresh against `token_url` and return a fresh token set. The
/// caller is responsible for persisting the result; the OAuth source
/// does so automatically.
pub(crate) async fn refresh_codex_token(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<OpenAiCodexTokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
    ];
    let response = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .map_err(|err| {
            SqueezyError::ProviderNotConfigured(format!("openai codex token refresh failed: {err}"))
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "openai codex token refresh failed ({status}): {body}"
        )));
    }
    let parsed = parse_token_response(&body)?;
    let account_id = extract_account_id(&parsed.access_token)?;
    Ok(OpenAiCodexTokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at_unix_ms: now_unix_ms().saturating_add(parsed.expires_in.saturating_mul(1000)),
        account_id,
    })
}

// ─── Local callback server ─────────────────────────────────────────────────

/// Outcome reported by [`login_openai_codex_interactive`]. The token
/// set is already persisted by the time this returns.
#[derive(Debug, Clone)]
pub struct OpenAiCodexLoginOutcome {
    pub account_id: String,
    pub expires_at_unix_ms: u64,
    pub auth_file: PathBuf,
}

/// Drive the OAuth authorization-code flow end-to-end:
///
/// 1. Generate PKCE + state.
/// 2. Start a local HTTP listener on `127.0.0.1:1455`.
/// 3. Hand the authorize URL to `on_open_url` (callers typically open
///    a browser; tests stub it).
/// 4. Wait for the callback, verify state, exchange code.
/// 5. Persist the token set to `auth_path`.
///
/// The blocking pieces (HTTP listener, token exchange) run on the
/// provided Tokio runtime; `auth_path` is the same path
/// [`codex_auth_file_path`] returns by default but is explicit so
/// tests can redirect persistence.
pub async fn login_openai_codex_interactive<F>(
    originator: &str,
    auth_path: &Path,
    on_open_url: F,
) -> Result<OpenAiCodexLoginOutcome>
where
    F: FnOnce(&str) -> Result<()>,
{
    let pair = generate_pkce()?;
    let state = generate_state()?;
    let authorize_url = build_authorize_url(&pair.challenge, &state, originator);

    let listener =
        tokio::net::TcpListener::bind((OPENAI_CODEX_CALLBACK_HOST, OPENAI_CODEX_CALLBACK_PORT))
            .await
            .map_err(|err| {
                SqueezyError::ProviderNotConfigured(format!(
                    "could not bind {}:{} for the OAuth callback: {err}",
                    OPENAI_CODEX_CALLBACK_HOST, OPENAI_CODEX_CALLBACK_PORT
                ))
            })?;

    on_open_url(&authorize_url)?;

    let code = wait_for_callback_code(listener, &state).await?;

    let client = shared_client(&ProviderTransportConfig::default());
    let token = exchange_authorization_code(
        &client,
        OPENAI_CODEX_TOKEN_URL,
        &code,
        &pair.verifier,
        OPENAI_CODEX_REDIRECT_URI,
    )
    .await?;
    save_codex_token(auth_path, &token)?;
    Ok(OpenAiCodexLoginOutcome {
        account_id: token.account_id.clone(),
        expires_at_unix_ms: token.expires_at_unix_ms,
        auth_file: auth_path.to_path_buf(),
    })
}

/// Accept exactly one HTTP request on `listener`, verify the OAuth
/// state and extract the `code` query parameter. Closes the socket
/// after a one-shot HTML response so the browser tab can be safely
/// closed by the user.
async fn wait_for_callback_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut socket, _addr) = listener.accept().await.map_err(|err| {
            SqueezyError::ProviderNotConfigured(format!("OAuth callback accept failed: {err}"))
        })?;
        let mut buf = vec![0u8; 4096];
        let n = socket.read(&mut buf).await.map_err(|err| {
            SqueezyError::ProviderNotConfigured(format!("OAuth callback read failed: {err}"))
        })?;
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let Some(request_line) = request.lines().next() else {
            send_callback_response(
                &mut socket,
                400,
                "Bad Request",
                "Invalid OAuth callback request.",
            )
            .await;
            continue;
        };
        let mut parts = request_line.split_whitespace();
        let _method = parts.next();
        let target = parts.next().unwrap_or("");
        // Restrict to the known callback path; everything else gets
        // a 404 so a stray health-checker poll doesn't terminate the
        // login flow.
        let Some(query) = target.split_once('?').map(|(_, q)| q) else {
            send_callback_response(&mut socket, 404, "Not Found", "Callback route not found.")
                .await;
            continue;
        };
        if !target.starts_with("/auth/callback") {
            send_callback_response(&mut socket, 404, "Not Found", "Callback route not found.")
                .await;
            continue;
        }

        let params = parse_query(query);
        let state = params.get("state").map(String::as_str).unwrap_or_default();
        if state != expected_state {
            send_callback_response(&mut socket, 400, "Bad Request", "State mismatch.").await;
            return Err(SqueezyError::ProviderNotConfigured(
                "openai codex OAuth state mismatch; possible CSRF, aborting".to_string(),
            ));
        }
        let Some(code) = params.get("code").cloned() else {
            send_callback_response(
                &mut socket,
                400,
                "Bad Request",
                "Missing authorization code.",
            )
            .await;
            return Err(SqueezyError::ProviderNotConfigured(
                "openai codex OAuth callback did not include a code".to_string(),
            ));
        };
        let body = "<!doctype html><html><body><h2>You are signed in to ChatGPT.</h2>\
                    <p>You can close this tab and return to your terminal.</p></body></html>";
        send_callback_response(&mut socket, 200, "OK", body).await;
        let _ = socket.shutdown().await;
        return Ok(code);
    }
}

async fn send_callback_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) {
    use tokio::io::AsyncWriteExt;

    let payload = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    );
    let _ = socket.write_all(payload.as_bytes()).await;
    let _ = socket.flush().await;
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for entry in query.split('&') {
        if entry.is_empty() {
            continue;
        }
        let mut iter = entry.splitn(2, '=');
        let raw_key = iter.next().unwrap_or("");
        let raw_value = iter.next().unwrap_or("");
        out.insert(url_form_decode(raw_key), url_form_decode(raw_value));
    }
    out
}

fn url_form_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_value(bytes[i + 1]);
                let lo = hex_value(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ─── ApiKeySource implementation ───────────────────────────────────────────

/// Refresh-aware [`ApiKeySource`] backed by a persisted Codex token
/// set. The state is held under an `Arc<RwLock<_>>` so a single
/// provider client can serve every concurrent request a session
/// issues without rebuilding the client when the token rotates.
pub struct OpenAiCodexOAuthSource {
    state: Arc<RwLock<OpenAiCodexTokenSet>>,
    auth_path: PathBuf,
    token_url: String,
    http_client: reqwest::Client,
    label: String,
}

impl std::fmt::Debug for OpenAiCodexOAuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCodexOAuthSource")
            .field("auth_path", &self.auth_path)
            .field("token_url", &self.token_url)
            .field("label", &self.label)
            .field("state", &"<redacted>")
            .finish()
    }
}

impl OpenAiCodexOAuthSource {
    pub fn new(token: OpenAiCodexTokenSet, auth_path: PathBuf) -> Self {
        Self {
            state: Arc::new(RwLock::new(token)),
            auth_path,
            token_url: OPENAI_CODEX_TOKEN_URL.to_string(),
            http_client: shared_client(&ProviderTransportConfig::default()),
            label: "openai_codex".to_string(),
        }
    }

    /// Test-friendly constructor that lets the integration tests point
    /// the refresh round-trip at a captive HTTP server. Production
    /// callers use [`Self::new`] which hard-codes
    /// [`OPENAI_CODEX_TOKEN_URL`].
    #[doc(hidden)]
    pub fn with_token_url(
        token: OpenAiCodexTokenSet,
        auth_path: PathBuf,
        token_url: impl Into<String>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(token)),
            auth_path,
            token_url: token_url.into(),
            http_client: shared_client(&ProviderTransportConfig::default()),
            label: "openai_codex".to_string(),
        }
    }

    /// Shared handle to the underlying token state; useful for tests
    /// or external monitoring that wants to observe rotations without
    /// going through the `ApiKeySource` round-trip.
    pub fn state_handle(&self) -> Arc<RwLock<OpenAiCodexTokenSet>> {
        self.state.clone()
    }

    async fn refresh_now(&self) -> Result<String> {
        let refresh_token = {
            let guard = self.state.read().await;
            guard.refresh_token.clone()
        };
        let new_token =
            refresh_codex_token(&self.http_client, &self.token_url, &refresh_token).await?;
        save_codex_token(&self.auth_path, &new_token)?;
        let access = new_token.access_token.clone();
        {
            let mut guard = self.state.write().await;
            *guard = new_token;
        }
        Ok(access)
    }
}

impl ApiKeySource for OpenAiCodexOAuthSource {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        Box::pin(async move {
            let needs_refresh = {
                let guard = self.state.read().await;
                guard.is_near_expiry(SystemTime::now())
            };
            if needs_refresh {
                return self.refresh_now().await;
            }
            let guard = self.state.read().await;
            Ok(guard.access_token.clone())
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut guard = self.state.write().await;
                // Stamp the expiry to the epoch so the next
                // `current_key` runs through the refresh path. We can't
                // safely throw the access token away here — a
                // concurrent in-flight request might still be waiting
                // for it — but the read-after-write lock ordering on
                // `refresh_now` guarantees the rotation happens
                // before the next attempt sees the cached value.
                guard.expires_at_unix_ms = 0;
            }
            Ok(())
        })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

// ─── Codex provider client ─────────────────────────────────────────────────

/// OpenAI Responses provider that targets the ChatGPT Codex backend
/// (`https://chatgpt.com/backend-api/codex/responses`) with the
/// subscription headers pi sets. Reuses
/// [`OpenAiProvider::request_body`] for the request body and the
/// shared SSE parser so the wire-level event handling stays in lock-
/// step with the regular `/v1/responses` provider.
pub struct OpenAiCodexProvider {
    name: &'static str,
    client: reqwest::Client,
    source: Arc<dyn ApiKeySource>,
    account_id: String,
    originator: String,
    base_url: String,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for OpenAiCodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCodexProvider")
            .field("name", &self.name)
            .field("account_id", &self.account_id)
            .field("originator", &self.originator)
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .field("source", &self.source)
            .finish()
    }
}

impl OpenAiCodexProvider {
    /// Build a Codex provider from the supplied config, loading the
    /// persisted token set and constructing an
    /// [`OpenAiCodexOAuthSource`]. The login flow must have been run
    /// previously; this errors with a `ProviderNotConfigured` value
    /// when the auth file is missing so the agent can surface a
    /// `squeezy auth openai-codex login` hint.
    pub fn from_config(config: &OpenAiCodexConfig) -> Result<Self> {
        let auth_path = codex_auth_file_path().ok_or_else(|| {
            SqueezyError::ProviderNotConfigured(
                "could not determine ~/.squeezy auth directory; \
                 set SQUEEZY_OPENAI_CODEX_AUTH_FILE or HOME"
                    .to_string(),
            )
        })?;
        let token = load_codex_token(&auth_path)?.ok_or_else(|| {
            SqueezyError::ProviderNotConfigured(format!(
                "no openai codex token at {}; \
                 run `squeezy auth openai-codex login` to authenticate",
                auth_path.display()
            ))
        })?;
        let account_id = token.account_id.clone();
        let source = OpenAiCodexOAuthSource::new(token, auth_path);
        Ok(Self {
            name: "openai_codex",
            client: shared_client(&config.transport),
            source: Arc::new(source),
            account_id,
            originator: if config.originator.trim().is_empty() {
                "squeezy".to_string()
            } else {
                config.originator.clone()
            },
            base_url: config.base_url.trim_end_matches('/').to_string(),
            transport: config.transport,
        })
    }

    /// Construct from a pre-built source. Used by tests and by any
    /// caller that wants to inject a custom [`ApiKeySource`] (e.g.
    /// for an in-memory recorder).
    pub fn with_source(
        source: Arc<dyn ApiKeySource>,
        account_id: impl Into<String>,
        originator: impl Into<String>,
        base_url: impl Into<String>,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self {
            name: "openai_codex",
            client: shared_client(&transport),
            source,
            account_id: account_id.into(),
            originator: originator.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            transport,
        }
    }

    pub(crate) fn build_codex_request_body(request: &LlmRequest) -> Value {
        let mut body = OpenAiProvider::request_body(request, "openai_codex");
        // Codex backend rejects `store=true` ("Store must be set to
        // false"); force it regardless of caller preference. Use the
        // same encrypted-content channel we already negotiate when
        // reasoning is in the body so multi-turn replay still works
        // without server-side state.
        body["store"] = Value::Bool(false);
        if !body
            .get("include")
            .map(|value| value.is_array())
            .unwrap_or(false)
        {
            body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
        }
        body
    }
}

impl LlmProvider for OpenAiCodexProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let client = self.client.clone();
        let source = self.source.clone();
        let transport = self.transport;
        let url = format!("{}/responses", self.base_url);
        let body = Self::build_codex_request_body(&request);
        let account_id = self.account_id.clone();
        let originator = self.originator.clone();

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &source,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    client
                        .post(&url)
                        .bearer_auth(key)
                        .header("chatgpt-account-id", &account_id)
                        .header("originator", &originator)
                        .header("OpenAI-Beta", "responses=experimental")
                        .header("accept", "text/event-stream")
                        .header("content-type", "application/json")
                        .json(&body)
                },
            ).await?;

            let status = response.status();
            let response = if status == StatusCode::OK {
                response
            } else {
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read error response".to_string());
                Err(SqueezyError::ProviderRequest(format!("{status}: {message}")))?;
                unreachable!("provider error returned above");
            };

            yield LlmEvent::Started;

            let mut decoder = SseDecoder::default();
            let mut saw_completed = false;
            let mut reasoning_acc = ReasoningAccumulator::default();
            let mut bytes = response.bytes_stream();
            loop {
                let polled = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield LlmEvent::Cancelled;
                        return;
                    }
                    next = timeout(idle_timeout(transport), bytes.next()) => next,
                };
                let next = polled.map_err(|_| {
                    SqueezyError::ProviderStream("OpenAI Codex stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    if let Some(llm_event) = parse_openai_event(&event, &mut reasoning_acc)? {
                        if matches!(llm_event, LlmEvent::Completed { .. }) {
                            saw_completed = true;
                        }
                        yield llm_event;
                    }
                }
            }

            for event in decoder.finish() {
                if let Some(llm_event) = parse_openai_event(&event, &mut reasoning_acc)? {
                    if matches!(llm_event, LlmEvent::Completed { .. }) {
                        saw_completed = true;
                    }
                    yield llm_event;
                }
            }

            if !saw_completed {
                Err(SqueezyError::ProviderStream(
                    "OpenAI Codex stream ended without response.completed".to_string(),
                ))?;
            }
        })
    }
}

#[cfg(test)]
#[path = "openai_codex_tests.rs"]
mod tests;
