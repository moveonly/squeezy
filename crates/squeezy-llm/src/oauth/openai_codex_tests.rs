use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;
use crate::credentials::ApiKeySource;

static NONCE: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-codex-oauth-{}-{}-{}",
        tag,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir.join("openai-codex.json")
}

fn make_jwt_with_account(account_id: &str) -> String {
    // Construct the minimal three-segment JWT the Codex extractor
    // expects: an arbitrary header, a payload containing the
    // `https://api.openai.com/auth.chatgpt_account_id` claim, and an
    // empty signature. The signature is not verified by squeezy —
    // OpenAI does that server-side — so any string in the third
    // segment is fine for tests.
    let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload_json = json!({
        OPENAI_CODEX_JWT_CLAIM_PATH: { "chatgpt_account_id": account_id },
        "sub": "user-test",
    });
    let payload = base64url_encode(payload_json.to_string().as_bytes());
    format!("{header}.{payload}.signature")
}

#[test]
fn pkce_challenge_is_sha256_of_verifier_base64url() {
    let pair = generate_pkce().expect("pkce");
    // Verifier should be 43 chars (32 random bytes base64url, no
    // padding) and the challenge should match the SHA-256 we compute
    // here from the very same verifier.
    assert!(
        pair.verifier.len() >= 43 && pair.verifier.len() <= 44,
        "verifier length unexpected: {}",
        pair.verifier.len()
    );
    let mut hasher = Sha256::new();
    hasher.update(pair.verifier.as_bytes());
    let expected = base64url_encode(&hasher.finalize());
    assert_eq!(pair.challenge, expected);
}

#[test]
fn authorize_url_carries_codex_flow_flags() {
    let url = build_authorize_url("challenge_value", "state_value", "squeezy");
    assert!(url.starts_with(OPENAI_CODEX_AUTHORIZE_URL), "{url}");
    assert!(url.contains("response_type=code"), "{url}");
    assert!(
        url.contains(&format!("client_id={}", OPENAI_CODEX_CLIENT_ID)),
        "{url}"
    );
    assert!(url.contains("code_challenge=challenge_value"), "{url}");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(url.contains("state=state_value"), "{url}");
    // The redirect URI carries the literal `http://localhost:1455/auth/callback`;
    // the URL form-encoder turns `:` and `/` into `%3A` / `%2F`.
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455"),
        "{url}"
    );
    // Originator is configurable per call; verify it round-trips.
    assert!(url.contains("originator=squeezy"), "{url}");
    // The two pi-specific flags must be present so the upstream
    // recognises this as a Codex CLI flow and surfaces the right
    // consent screen.
    assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
    assert!(url.contains("id_token_add_organizations=true"), "{url}");
}

#[test]
fn extract_account_id_pulls_chatgpt_account_id_from_jwt() {
    let token = make_jwt_with_account("acct_test_42");
    let account = extract_account_id(&token).expect("account id");
    assert_eq!(account, "acct_test_42");
}

#[test]
fn extract_account_id_errors_on_missing_claim() {
    // Build a JWT that has a valid payload but no
    // `chatgpt_account_id` field; the extractor must surface a
    // typed `ProviderNotConfigured` so the CLI tells the user to
    // sign in to a paid ChatGPT plan.
    let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = base64url_encode(br#"{"sub":"free-user"}"#);
    let token = format!("{header}.{payload}.sig");
    let err = extract_account_id(&token).expect_err("missing claim");
    let msg = err.to_string();
    assert!(msg.contains("chatgpt_account_id"), "{msg}");
}

#[test]
fn extract_account_id_errors_on_non_jwt() {
    let err = extract_account_id("not-a-jwt").expect_err("missing payload");
    let msg = err.to_string();
    assert!(msg.contains("not a JWT"), "{msg}");
}

#[cfg(unix)]
#[test]
fn save_codex_token_writes_with_0600_perms() {
    use std::os::unix::fs::PermissionsExt;

    let path = tmp_path("save");
    let token = OpenAiCodexTokenSet {
        access_token: make_jwt_with_account("acct_save"),
        refresh_token: "refresh-save".to_string(),
        expires_at_unix_ms: 1_700_000_000_000,
        account_id: "acct_save".to_string(),
    };
    save_codex_token(&path, &token).expect("save");
    let meta = std::fs::metadata(&path).expect("stat");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "token file should be chmod 600, got {:o}",
        mode
    );
    let loaded = load_codex_token(&path).expect("load").expect("present");
    assert_eq!(loaded.account_id, "acct_save");
    assert_eq!(loaded.refresh_token, "refresh-save");
    assert_eq!(loaded.expires_at_unix_ms, 1_700_000_000_000);
}

#[test]
fn load_codex_token_returns_none_when_missing() {
    let path = tmp_path("missing");
    let _ = std::fs::remove_file(&path);
    let loaded = load_codex_token(&path).expect("load missing");
    assert!(loaded.is_none(), "missing file should yield None");
}

#[test]
fn load_codex_token_errors_on_malformed_json() {
    let path = tmp_path("malformed");
    std::fs::write(&path, "{ this is not json").expect("write");
    let err = load_codex_token(&path).expect_err("malformed");
    assert!(err.to_string().contains("not valid JSON"), "{err}");
}

// ─── refresh + ApiKeySource interactions ───────────────────────────────────

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

/// Spin up a one-shot token server that answers any POST with the
/// supplied JSON body and reports the form fields it received.
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
async fn refresh_codex_token_round_trips_against_captive_endpoint() {
    let fresh_token = make_jwt_with_account("acct_refresh");
    let response = json!({
        "access_token": fresh_token,
        "refresh_token": "rt-rotated",
        "expires_in": 3600u64,
    });
    let (addr, req_rx) = spawn_token_server(200, response.to_string()).await;
    let token_url = format!("http://{addr}/oauth/token");

    let client = reqwest::Client::new();
    let refreshed = refresh_codex_token(&client, &token_url, "rt-old")
        .await
        .expect("refresh");

    assert_eq!(refreshed.account_id, "acct_refresh");
    assert_eq!(refreshed.refresh_token, "rt-rotated");
    // Expires in 3600 seconds → expires_at must land roughly an hour
    // ahead of now. Allow a generous window so a slow CI host doesn't
    // flake this.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let delta = refreshed.expires_at_unix_ms.saturating_sub(now_ms);
    assert!(
        (3_500_000..=3_700_000).contains(&delta),
        "expires_at within ~1h, got delta {delta}ms"
    );

    let request = req_rx.await.expect("captured");
    assert!(
        request.contains("grant_type=refresh_token"),
        "form payload missing grant_type: {request}"
    );
    assert!(
        request.contains("refresh_token=rt-old"),
        "form payload missing old refresh: {request}"
    );
    assert!(
        request.contains(&format!("client_id={}", OPENAI_CODEX_CLIENT_ID)),
        "form payload missing client_id: {request}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_returns_cached_token_when_not_expiring() {
    let path = tmp_path("cached");
    let access = make_jwt_with_account("acct_cache");
    let token = OpenAiCodexTokenSet {
        access_token: access.clone(),
        refresh_token: "rt-noop".to_string(),
        expires_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + Duration::from_secs(3600).as_millis() as u64,
        account_id: "acct_cache".to_string(),
    };
    save_codex_token(&path, &token).expect("save");
    let source = OpenAiCodexOAuthSource::with_token_url(
        token,
        path,
        // No HTTP server is spawned because we expect no refresh.
        "http://127.0.0.1:0/never-called",
    );
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    let first = arc.current_key().await.expect("first");
    let second = arc.current_key().await.expect("second");
    assert_eq!(first, access);
    assert_eq!(second, access);
    assert_eq!(arc.provider_label(), "openai_codex");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_refreshes_when_near_expiry() {
    let path = tmp_path("refresh");
    let initial = make_jwt_with_account("acct_initial");
    let rotated = make_jwt_with_account("acct_rotated");
    let token = OpenAiCodexTokenSet {
        access_token: initial.clone(),
        refresh_token: "rt-initial".to_string(),
        // Already expired so the safety window triggers refresh.
        expires_at_unix_ms: 1,
        account_id: "acct_initial".to_string(),
    };
    save_codex_token(&path, &token).expect("save");

    let response = json!({
        "access_token": rotated.clone(),
        "refresh_token": "rt-fresh",
        "expires_in": 3600u64,
    });
    let (addr, _req_rx) = spawn_token_server(200, response.to_string()).await;
    let token_url = format!("http://{addr}/oauth/token");

    let source = OpenAiCodexOAuthSource::with_token_url(token, path.clone(), token_url);
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    let key = arc.current_key().await.expect("refreshed");
    assert_eq!(key, rotated);
    // The on-disk token must reflect the rotation so a subsequent
    // process opening the same session inherits the new credential.
    let persisted = load_codex_token(&path).expect("load").expect("present");
    assert_eq!(persisted.access_token, rotated);
    assert_eq!(persisted.refresh_token, "rt-fresh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_source_invalidate_forces_next_refresh() {
    let path = tmp_path("invalidate");
    let initial = make_jwt_with_account("acct_invalidate");
    let rotated = make_jwt_with_account("acct_invalidate_v2");
    let token = OpenAiCodexTokenSet {
        access_token: initial.clone(),
        refresh_token: "rt".to_string(),
        // Comfortably in the future so `current_key` would otherwise
        // return the cached value.
        expires_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + 3_600_000,
        account_id: "acct_invalidate".to_string(),
    };
    save_codex_token(&path, &token).expect("save");

    let response = json!({
        "access_token": rotated.clone(),
        "refresh_token": "rt-fresh",
        "expires_in": 3600u64,
    });
    let (addr, _req_rx) = spawn_token_server(200, response.to_string()).await;
    let token_url = format!("http://{addr}/oauth/token");

    let source = OpenAiCodexOAuthSource::with_token_url(token, path, token_url);
    let arc: Arc<dyn ApiKeySource> = Arc::new(source);
    // Before invalidate, the cached token must surface unchanged.
    let cached = arc.current_key().await.expect("cached");
    assert_eq!(cached, initial);
    // After invalidate, the next call must hit the refresh path.
    arc.invalidate().await.expect("invalidate");
    let fresh = arc.current_key().await.expect("fresh");
    assert_eq!(fresh, rotated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_codex_token_surfaces_endpoint_errors() {
    let (addr, _req_rx) = spawn_token_server(
        400,
        r#"{"error":"invalid_grant","error_description":"refresh_token expired"}"#.to_string(),
    )
    .await;
    let token_url = format!("http://{addr}/oauth/token");
    let client = reqwest::Client::new();
    let err = refresh_codex_token(&client, &token_url, "rt-stale")
        .await
        .expect_err("non-2xx");
    let msg = err.to_string();
    assert!(
        msg.contains("token refresh failed"),
        "expected failure message, got {msg}"
    );
}

#[test]
fn codex_request_body_forces_store_false_and_encrypted_content() {
    use crate::{LlmInputItem, LlmRequest};

    let request = LlmRequest {
        model: "gpt-5.5".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: std::sync::Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec::default(),
        tools: std::sync::Arc::from(Vec::new()),
        // Caller may have set `store=true`; the codex provider must
        // override it because the backend rejects `store=true`.
        store: true,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let body = OpenAiCodexProvider::build_codex_request_body(&request);
    assert_eq!(body["store"], serde_json::Value::Bool(false));
    let include = body
        .get("include")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("include array set");
    assert!(
        include
            .iter()
            .any(|v| v.as_str() == Some("reasoning.encrypted_content")),
        "include must request encrypted reasoning content for replay"
    );
}
