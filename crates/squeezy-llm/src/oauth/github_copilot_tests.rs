use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;
use crate::credentials::ApiKeySource;

static NONCE: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-github-copilot-oauth-{}-{}-{}",
        tag,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir.join("github-copilot.json")
}

fn future_expiry_ms(offset_secs: u64) -> u64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64)
        + Duration::from_secs(offset_secs).as_millis() as u64
}

#[test]
fn base_url_from_token_extracts_proxy_ep_and_rewrites_to_api() {
    let token = "tid=abc;exp=9999;proxy-ep=proxy.individual.githubcopilot.com;sku=copilot";
    let url = base_url_from_token(token).expect("parses");
    assert_eq!(url, "https://api.individual.githubcopilot.com");
}

#[test]
fn base_url_from_token_handles_enterprise_proxy_host() {
    let token = "tid=ent;exp=1;proxy-ep=proxy.copilot.acme.ghe.com";
    let url = base_url_from_token(token).expect("parses");
    assert_eq!(url, "https://api.copilot.acme.ghe.com");
}

#[test]
fn base_url_from_token_returns_none_when_proxy_ep_missing() {
    let token = "tid=abc;exp=1;sku=copilot";
    assert!(base_url_from_token(token).is_none());
}

#[test]
fn resolve_base_url_falls_back_to_default_individual_host() {
    // Token without a `proxy-ep` segment should land on the default
    // individual host so the provider client has a base URL even on
    // odd / malformed tokens.
    let url = resolve_base_url("tid=abc;exp=1", None);
    assert_eq!(url, DEFAULT_BASE_URL);
}

#[test]
fn resolve_base_url_uses_enterprise_domain_when_token_lacks_proxy_ep() {
    let url = resolve_base_url("tid=abc;exp=1", Some("acme.ghe.com"));
    assert_eq!(url, "https://copilot-api.acme.ghe.com");
}

#[test]
fn normalize_domain_accepts_bare_host_and_full_url() {
    assert_eq!(
        normalize_domain("acme.ghe.com").as_deref(),
        Some("acme.ghe.com")
    );
    assert_eq!(
        normalize_domain(" https://acme.ghe.com/login ").as_deref(),
        Some("acme.ghe.com")
    );
    assert_eq!(normalize_domain("").as_deref(), None);
    assert_eq!(normalize_domain("   ").as_deref(), None);
}

#[test]
fn github_copilot_urls_for_domain_match_pi_layout() {
    let urls = GitHubCopilotUrls::for_domain("github.com");
    assert_eq!(urls.device_code_url, "https://github.com/login/device/code");
    assert_eq!(
        urls.access_token_url,
        "https://github.com/login/oauth/access_token"
    );
    assert_eq!(
        urls.copilot_token_url,
        "https://api.github.com/copilot_internal/v2/token"
    );

    // Enterprise hosts route through `https://{tenant}` not
    // `https://api.{tenant}` for the device-code + access-token
    // endpoints, but the copilot token endpoint sits under
    // `https://api.{tenant}` (matching pi's layout).
    let ent = GitHubCopilotUrls::for_domain("acme.ghe.com");
    assert_eq!(
        ent.device_code_url,
        "https://acme.ghe.com/login/device/code"
    );
    assert_eq!(
        ent.copilot_token_url,
        "https://api.acme.ghe.com/copilot_internal/v2/token"
    );
}

#[test]
fn copilot_headers_carry_editor_impersonation_set() {
    let headers = copilot_headers();
    assert_eq!(
        headers.get("user-agent").map(String::as_str),
        Some(COPILOT_USER_AGENT)
    );
    assert_eq!(
        headers.get("editor-version").map(String::as_str),
        Some(COPILOT_EDITOR_VERSION)
    );
    assert_eq!(
        headers.get("editor-plugin-version").map(String::as_str),
        Some(COPILOT_EDITOR_PLUGIN_VERSION)
    );
    assert_eq!(
        headers.get("copilot-integration-id").map(String::as_str),
        Some(COPILOT_INTEGRATION_ID)
    );
}

#[cfg(unix)]
#[test]
fn write_tokens_persists_with_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let path = tmp_path("write-mode");
    let tokens = PersistedGitHubCopilotTokens {
        github_token: "gh-token".to_string(),
        copilot_token: "cp-token".to_string(),
        expires_at_unix_ms: 1_700_000_000_000,
        enterprise_domain: None,
        provider: "github-copilot-oauth".to_string(),
    };
    write_tokens(&path, &tokens).expect("write");
    let meta = std::fs::metadata(&path).expect("stat");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "token file should be chmod 600, got {:o}",
        mode
    );
    let loaded = read_tokens(&path).expect("read").expect("present");
    assert_eq!(loaded.github_token, "gh-token");
    assert_eq!(loaded.copilot_token, "cp-token");
    assert_eq!(loaded.expires_at_unix_ms, 1_700_000_000_000);
    assert!(loaded.enterprise_domain.is_none());
}

#[test]
fn read_tokens_returns_none_when_missing() {
    let path = tmp_path("missing");
    let _ = std::fs::remove_file(&path);
    let loaded = read_tokens(&path).expect("read missing");
    assert!(loaded.is_none(), "missing file should yield None");
}

#[test]
fn read_tokens_errors_on_malformed_json() {
    let path = tmp_path("malformed");
    std::fs::write(&path, "{ this is not json").expect("write");
    let err = read_tokens(&path).expect_err("malformed");
    assert!(err.to_string().contains("not valid JSON"), "{err}");
}

#[test]
fn read_tokens_round_trips_persisted_enterprise_domain() {
    let path = tmp_path("ent-roundtrip");
    let tokens = PersistedGitHubCopilotTokens {
        github_token: "gh".to_string(),
        copilot_token: "cp".to_string(),
        expires_at_unix_ms: 1_700_000_000_000,
        enterprise_domain: Some("acme.ghe.com".to_string()),
        provider: "github-copilot-oauth".to_string(),
    };
    write_tokens(&path, &tokens).expect("write");
    let loaded = read_tokens(&path).expect("read").expect("present");
    assert_eq!(
        loaded.enterprise_domain.as_deref(),
        Some("acme.ghe.com"),
        "enterprise_domain must round-trip through the on-disk JSON"
    );
}

// ─── Polling outcome classification ────────────────────────────────────────

#[test]
fn classify_device_error_routes_pending_and_slow_down_separately() {
    let pending = DeviceTokenErrorResponse {
        error: "authorization_pending".to_string(),
        error_description: None,
    };
    assert_eq!(classify_device_error(&pending), DevicePollOutcome::Pending);

    let slow = DeviceTokenErrorResponse {
        error: "slow_down".to_string(),
        error_description: None,
    };
    assert_eq!(classify_device_error(&slow), DevicePollOutcome::SlowDown);

    let denied = DeviceTokenErrorResponse {
        error: "access_denied".to_string(),
        error_description: Some("user cancelled".to_string()),
    };
    match classify_device_error(&denied) {
        DevicePollOutcome::Failed(msg) => {
            assert!(msg.contains("access_denied"), "{msg}");
            assert!(msg.contains("user cancelled"), "{msg}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

// ─── Captive HTTP server for refresh + policy tests ────────────────────────

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.expect("read req");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

async fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) {
    let payload = format!(
        "HTTP/1.1 {status} OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    );
    stream.write_all(payload.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");
}

async fn spawn_token_server(
    response_status: u16,
    response_body: String,
) -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_http_request(&mut socket).await;
        write_http_response(&mut socket, response_status, &response_body).await;
        let _ = socket.shutdown().await;
        let _ = tx.send(request);
    });
    (addr, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_copilot_token_round_trips_against_captive_endpoint() {
    let expires_at = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 3600) as i64;
    let response = json!({
        "token": "tid=cap;exp=9999;proxy-ep=proxy.individual.githubcopilot.com;rotated=1",
        "expires_at": expires_at,
    });
    let (addr, req_rx) = spawn_token_server(200, response.to_string()).await;
    let copilot_token_url = format!("http://{addr}/copilot_internal/v2/token");
    let urls = GitHubCopilotUrls {
        device_code_url: String::new(),
        access_token_url: String::new(),
        copilot_token_url,
    };

    let client = reqwest::Client::new();
    let (copilot_token, expires_at_ms) = refresh_copilot_token(&client, &urls, "gh-token")
        .await
        .expect("refresh");

    assert!(copilot_token.contains("proxy-ep="), "{copilot_token}");
    // The persisted expires_at_unix_ms must already include the 5 minute
    // lead-time cushion. Allow a generous window so a slow CI host
    // doesn't flake.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let delta = expires_at_ms.saturating_sub(now_ms);
    assert!(
        delta < (3600 - 250) * 1000,
        "expires_at must have the refresh lead-time cushion applied, delta={delta}ms"
    );
    assert!(
        delta > (3600 - 360) * 1000,
        "expires_at must still be within ~1h, delta={delta}ms"
    );

    let request = req_rx.await.expect("captured");
    assert!(
        request.contains("authorization: Bearer gh-token")
            || request.contains("Authorization: Bearer gh-token"),
        "refresh request must Bearer-auth with the github token: {request}"
    );
    assert!(
        request.contains(COPILOT_USER_AGENT),
        "refresh request must stamp the Copilot user-agent: {request}"
    );
    assert!(
        request.contains(COPILOT_EDITOR_VERSION),
        "refresh request must stamp editor-version: {request}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_copilot_token_surfaces_endpoint_errors() {
    let (addr, _req_rx) = spawn_token_server(
        401,
        r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com"}"#
            .to_string(),
    )
    .await;
    let urls = GitHubCopilotUrls {
        device_code_url: String::new(),
        access_token_url: String::new(),
        copilot_token_url: format!("http://{addr}/copilot_internal/v2/token"),
    };
    let client = reqwest::Client::new();
    let err = refresh_copilot_token(&client, &urls, "gh-stale")
        .await
        .expect_err("non-2xx");
    let msg = err.to_string();
    assert!(
        msg.contains("token exchange returned"),
        "expected failure message, got {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_returns_cached_token_when_not_near_expiry() {
    let path = tmp_path("cached");
    let copilot_token = "tid=cache;exp=1;proxy-ep=proxy.individual.githubcopilot.com".to_string();
    let tokens = PersistedGitHubCopilotTokens {
        github_token: "gh-cached".to_string(),
        copilot_token: copilot_token.clone(),
        expires_at_unix_ms: future_expiry_ms(3600),
        enterprise_domain: None,
        provider: "github-copilot-oauth".to_string(),
    };
    write_tokens(&path, &tokens).expect("persist");
    let source = GitHubCopilotOAuthSource::with_copilot_token_url(
        tokens,
        path,
        // No HTTP server: hitting the URL would fail, so a refresh
        // attempt would surface as an error and fail this test.
        "http://127.0.0.1:0/never-called",
    );
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    let first = arc.current_key().await.expect("first");
    let second = arc.current_key().await.expect("second");
    assert_eq!(first, copilot_token);
    assert_eq!(second, copilot_token);
    assert_eq!(arc.provider_label(), "github_copilot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_refreshes_when_near_expiry() {
    let path = tmp_path("refresh");
    let initial = "tid=init;exp=1;proxy-ep=proxy.individual.githubcopilot.com".to_string();
    let rotated = "tid=rot;exp=2;proxy-ep=proxy.individual.githubcopilot.com".to_string();
    let tokens = PersistedGitHubCopilotTokens {
        github_token: "gh-stay".to_string(),
        copilot_token: initial.clone(),
        // Already expired so the leeway window triggers refresh.
        expires_at_unix_ms: 1,
        enterprise_domain: None,
        provider: "github-copilot-oauth".to_string(),
    };
    write_tokens(&path, &tokens).expect("persist");

    let expires_at = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 3600) as i64;
    let response = json!({
        "token": rotated.clone(),
        "expires_at": expires_at,
    });
    let (addr, _req_rx) = spawn_token_server(200, response.to_string()).await;
    let copilot_token_url = format!("http://{addr}/copilot_internal/v2/token");

    let source =
        GitHubCopilotOAuthSource::with_copilot_token_url(tokens, path.clone(), copilot_token_url);
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    let key = arc.current_key().await.expect("refreshed");
    assert_eq!(key, rotated);
    let persisted = read_tokens(&path).expect("read").expect("present");
    assert_eq!(persisted.copilot_token, rotated);
    // The github_token MUST survive a refresh — it is the long-lived
    // credential that backs every subsequent Copilot exchange.
    assert_eq!(persisted.github_token, "gh-stay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_invalidate_forces_next_refresh() {
    let path = tmp_path("invalidate");
    let initial = "tid=init;exp=1;proxy-ep=proxy.individual.githubcopilot.com".to_string();
    let rotated = "tid=rot;exp=2;proxy-ep=proxy.individual.githubcopilot.com".to_string();
    let tokens = PersistedGitHubCopilotTokens {
        github_token: "gh-stay".to_string(),
        copilot_token: initial.clone(),
        // Comfortably in the future so `current_key` would otherwise
        // return the cached value.
        expires_at_unix_ms: future_expiry_ms(3600),
        enterprise_domain: None,
        provider: "github-copilot-oauth".to_string(),
    };
    write_tokens(&path, &tokens).expect("persist");

    let expires_at = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 3600) as i64;
    let response = json!({
        "token": rotated.clone(),
        "expires_at": expires_at,
    });
    let (addr, _req_rx) = spawn_token_server(200, response.to_string()).await;
    let copilot_token_url = format!("http://{addr}/copilot_internal/v2/token");

    let source = GitHubCopilotOAuthSource::with_copilot_token_url(tokens, path, copilot_token_url);
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    let cached = arc.current_key().await.expect("cached");
    assert_eq!(cached, initial);
    arc.invalidate().await.expect("invalidate");
    let fresh = arc.current_key().await.expect("fresh");
    assert_eq!(fresh, rotated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_models_reports_per_model_outcome() {
    // Set up a captive server that accepts the first model and 4xxs
    // the second so we exercise both branches of the success flag
    // without forcing a full HTTP mocking dependency.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        for status in [200u16, 403u16] {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_http_request(&mut socket).await;
            write_http_response(&mut socket, status, "{}").await;
            let _ = socket.shutdown().await;
        }
    });

    let client = reqwest::Client::new();
    let outcomes = enable_models(
        &client,
        &format!("http://{addr}"),
        "cp-token",
        &["claude-sonnet-4.5", "claude-opus-4.7"],
    )
    .await;
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].model_id, "claude-sonnet-4.5");
    assert!(outcomes[0].success);
    assert_eq!(outcomes[1].model_id, "claude-opus-4.7");
    assert!(!outcomes[1].success);
}

#[test]
fn default_policy_models_contains_curated_set() {
    // Sanity check the bundled list so an accidental empty rewrite
    // would fail loudly here instead of silently leaving the user's
    // newly-enabled account without any model gates flipped.
    assert!(!DEFAULT_POLICY_MODELS.is_empty());
    assert!(DEFAULT_POLICY_MODELS.contains(&"claude-sonnet-4.5"));
    assert!(DEFAULT_POLICY_MODELS.contains(&"gpt-5.5"));
}
