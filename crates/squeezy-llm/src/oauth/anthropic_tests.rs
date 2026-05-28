use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::*;
use crate::credentials::ApiKeySource;
use crate::oauth::pkce::{challenge_for, generate_pkce};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_token_path(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-anthropic-oauth-{}-{}-{}",
        prefix,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("anthropic.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fresh_persisted_tokens(suffix: &str) -> PersistedTokens {
    PersistedTokens {
        access_token: format!("sk-ant-oat-access-{suffix}"),
        refresh_token: format!("sk-ant-rfr-{suffix}"),
        // Expire well into the future so `needs_refresh` is false.
        expires_at_unix_ms: now_ms() + 3_600_000,
        scope: Some(SCOPES.to_string()),
        provider: "anthropic-oauth".to_string(),
    }
}

fn expired_persisted_tokens(suffix: &str) -> PersistedTokens {
    PersistedTokens {
        access_token: format!("sk-ant-oat-stale-{suffix}"),
        refresh_token: format!("sk-ant-rfr-{suffix}"),
        // Already expired one minute ago.
        expires_at_unix_ms: now_ms().saturating_sub(60_000),
        scope: None,
        provider: "anthropic-oauth".to_string(),
    }
}

#[tokio::test]
async fn current_key_returns_cached_access_token_when_not_expired() {
    let path = temp_token_path("fresh");
    let source = AnthropicOAuthSource::from_tokens(fresh_persisted_tokens("a"), path);
    let key = source.current_key().await.expect("current_key");
    assert_eq!(key, "sk-ant-oat-access-a");
    let again = source.current_key().await.expect("current_key again");
    assert_eq!(again, "sk-ant-oat-access-a");
    assert_eq!(source.provider_label(), "anthropic-oauth");
}

#[tokio::test]
async fn invalidate_then_current_key_triggers_refresh_through_mock_endpoint() {
    let token_url = spawn_token_mock(vec![json!({
        "access_token": "sk-ant-oat-refreshed",
        "refresh_token": "sk-ant-rfr-new",
        "expires_in": 3600,
    })])
    .await;

    let path = temp_token_path("invalidate");
    let mut config = AnthropicLoginConfig::default();
    config.token_url = token_url;
    let source = AnthropicOAuthSource::with_parts(
        fresh_persisted_tokens("orig"),
        path.clone(),
        config,
        reqwest::Client::new(),
    );

    // Before invalidate: the cached token is served.
    let before = source.current_key().await.expect("before");
    assert_eq!(before, "sk-ant-oat-access-orig");

    source.invalidate().await.expect("invalidate");

    // After invalidate: the next current_key must refresh via the
    // mock endpoint and return the freshly-issued access token.
    let after = source.current_key().await.expect("after");
    assert_eq!(after, "sk-ant-oat-refreshed");

    // Disk file reflects the refreshed tokens.
    let on_disk = read_tokens(&path).expect("read").expect("present");
    assert_eq!(on_disk.access_token, "sk-ant-oat-refreshed");
    assert_eq!(on_disk.refresh_token, "sk-ant-rfr-new");
}

#[tokio::test]
async fn expired_token_refreshes_automatically_on_current_key() {
    let token_url = spawn_token_mock(vec![json!({
        "access_token": "sk-ant-oat-rotated",
        "refresh_token": "sk-ant-rfr-rotated",
        "expires_in": 3600,
    })])
    .await;

    let path = temp_token_path("expired");
    let mut config = AnthropicLoginConfig::default();
    config.token_url = token_url;
    let source = AnthropicOAuthSource::with_parts(
        expired_persisted_tokens("e"),
        path.clone(),
        config,
        reqwest::Client::new(),
    );

    assert!(source.needs_refresh().await);
    let key = source
        .current_key()
        .await
        .expect("current_key triggers refresh");
    assert_eq!(key, "sk-ant-oat-rotated");
    assert!(!source.needs_refresh().await);
}

#[tokio::test]
async fn current_key_concurrent_calls_share_a_single_refresh() {
    // Two simultaneous current_key calls must funnel through the
    // same write lock and only fire one HTTP refresh between them.
    let token_url = spawn_token_mock(vec![
        json!({
            "access_token": "sk-ant-oat-rotated-once",
            "refresh_token": "sk-ant-rfr-once",
            "expires_in": 3600,
        }),
        // A second response queued so an accidental second request
        // would produce a *different* access token and break the
        // assertion.
        json!({
            "access_token": "sk-ant-oat-rotated-twice",
            "refresh_token": "sk-ant-rfr-twice",
            "expires_in": 3600,
        }),
    ])
    .await;

    let path = temp_token_path("concurrent");
    let mut config = AnthropicLoginConfig::default();
    config.token_url = token_url;
    let source = Arc::new(AnthropicOAuthSource::with_parts(
        expired_persisted_tokens("c"),
        path,
        config,
        reqwest::Client::new(),
    ));

    let a = source.clone();
    let b = source.clone();
    let (first, second) = tokio::join!(a.current_key(), b.current_key());
    let first = first.expect("first");
    let second = second.expect("second");
    assert_eq!(first, "sk-ant-oat-rotated-once");
    assert_eq!(second, first, "both callers must observe the same refresh");
}

#[test]
fn write_then_read_round_trips_persisted_tokens_to_disk() {
    let path = temp_token_path("roundtrip");
    let original = fresh_persisted_tokens("rt");
    write_tokens(&path, &original).expect("write");

    let loaded = read_tokens(&path).expect("read").expect("present");
    assert_eq!(loaded, original);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).expect("meta");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tokens file must be mode 0600");
    }
}

#[test]
fn read_tokens_returns_none_when_file_is_absent() {
    let path = temp_token_path("missing");
    // Ensure the file does not exist.
    let _ = std::fs::remove_file(&path);
    let result = read_tokens(&path).expect("read missing");
    assert!(result.is_none());
}

#[test]
fn load_from_path_returns_provider_not_configured_when_absent() {
    let path = temp_token_path("absent-load");
    let _ = std::fs::remove_file(&path);
    let err = AnthropicOAuthSource::load_from_path(path).expect_err("absent must error");
    assert!(
        matches!(err, squeezy_core::SqueezyError::ProviderNotConfigured(_)),
        "expected ProviderNotConfigured, got {err:?}",
    );
}

#[test]
fn token_response_to_persisted_subtracts_refresh_lead_time() {
    let response = TokenResponse {
        access_token: "sk-ant-oat-fresh".to_string(),
        refresh_token: "sk-ant-rfr-fresh".to_string(),
        expires_in: 3600,
        scope: None,
    };
    let now = 1_700_000_000_000_u64;
    let persisted = PersistedTokens::from_token_response(&response, now);
    // 3600 s minus the 5 minute lead time -> 3300 s -> 3_300_000 ms.
    assert_eq!(
        persisted.expires_at_unix_ms,
        now + 3_300_000,
        "lead time must offset the absolute expiry",
    );
    assert_eq!(persisted.access_token, "sk-ant-oat-fresh");
}

#[test]
fn parse_authorization_input_handles_pi_input_shapes() {
    let url = parse_authorization_input("https://example.com/callback?code=abc123&state=xyz789");
    assert_eq!(url.code.as_deref(), Some("abc123"));
    assert_eq!(url.state.as_deref(), Some("xyz789"));

    let hashed = parse_authorization_input("abc123#xyz789");
    assert_eq!(hashed.code.as_deref(), Some("abc123"));
    assert_eq!(hashed.state.as_deref(), Some("xyz789"));

    let query = parse_authorization_input("code=abc123&state=xyz789");
    assert_eq!(query.code.as_deref(), Some("abc123"));
    assert_eq!(query.state.as_deref(), Some("xyz789"));

    let bare = parse_authorization_input("abc123");
    assert_eq!(bare.code.as_deref(), Some("abc123"));
    assert_eq!(bare.state, None);

    let blank = parse_authorization_input("   ");
    assert_eq!(blank.code, None);
    assert_eq!(blank.state, None);
}

#[test]
fn authorize_url_carries_all_pkce_parameters() {
    let codes = PkceCodes {
        verifier: "verifier-value".to_string(),
        challenge: "challenge-value".to_string(),
    };
    let url = AnthropicLoginConfig::default().authorize_url(&codes);
    assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
    assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    assert!(url.contains("code_challenge=challenge-value"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=verifier-value"));
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback"),
        "redirect_uri must be percent-encoded: {url}"
    );
}

#[test]
fn is_anthropic_oauth_token_detects_pi_prefix() {
    assert!(is_anthropic_oauth_token("sk-ant-oat-abc"));
    assert!(is_anthropic_oauth_token("sk-ant-oat"));
    assert!(!is_anthropic_oauth_token("sk-ant-api-anything"));
    assert!(!is_anthropic_oauth_token("OPENAI_KEY"));
    assert!(!is_anthropic_oauth_token(""));
}

#[test]
fn anthropic_oauth_beta_header_lists_claude_code_and_oauth_betas() {
    let header = anthropic_oauth_beta_header();
    assert!(header.contains("claude-code-"));
    assert!(header.contains("oauth-"));
}

#[test]
fn challenge_matches_verifier_round_trip() {
    // Quick cross-check that the PKCE helper used by the login flow
    // round-trips through `challenge_for`.
    let codes = generate_pkce().expect("pkce");
    assert_eq!(codes.challenge, challenge_for(&codes.verifier));
}

// ---------- mock token server ---------------------------------------------

/// Tiny HTTP/1.1 mock for the platform OAuth token endpoint. Returns
/// each queued JSON body in order; any extra request beyond the
/// queued responses returns `500 Internal Server Error` so a stray
/// extra refresh would fail the test instead of silently succeeding.
async fn spawn_token_mock(responses: Vec<serde_json::Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let queue = Arc::new(Mutex::new(responses.into_iter().collect::<Vec<_>>()));
    let queue_for_loop = queue.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let queue = queue_for_loop.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(&mut socket);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.is_err() {
                    return;
                }
                let mut content_length: usize = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        if let Ok(len) = rest.trim().parse::<usize>() {
                            content_length = len;
                        }
                    }
                }
                if content_length > 0 {
                    let mut buf = vec![0u8; content_length];
                    if tokio::io::AsyncReadExt::read_exact(&mut reader, &mut buf)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let next = {
                    let mut guard = queue.lock().await;
                    if guard.is_empty() {
                        None
                    } else {
                        Some(guard.remove(0))
                    }
                };
                let (status, body) = match next {
                    Some(value) => (200_u16, value.to_string()),
                    None => (500_u16, json!({"error": "mock exhausted"}).to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                    status = status,
                    len = body.as_bytes().len(),
                    body = body,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{}/v1/oauth/token", addr)
}
