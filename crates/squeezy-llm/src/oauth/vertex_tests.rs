// Vertex OAuth tests stub `gcloud` via `/bin/sh -c`, which isn't
// available on Windows. Gate compilation to Unix so the Windows test
// run stays clean; the production path uses the real `gcloud` binary
// which Windows users invoke separately.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::*;
use crate::credentials::ApiKeySource;

static NONCE: AtomicU64 = AtomicU64::new(0);

fn unique_marker(tag: &str) -> String {
    format!(
        "vertex-{}-{}-{}",
        tag,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    )
}

/// Build a `sh -c` shell command that echoes a literal token. We use
/// `sh -c` instead of `echo` so we can control exactly what the
/// subprocess emits without relying on platform-specific `echo`
/// behavior (e.g. macOS `echo -n` vs `printf`).
fn shell_emits(token: &str) -> Vec<String> {
    vec!["-c".to_string(), format!("printf %s {token}")]
}

#[tokio::test]
async fn current_key_returns_subprocess_stdout() {
    let source = VertexOAuthSource::with_command("/bin/sh", shell_emits("ya29.test-token-stdout"));
    let token = source.current_key().await.expect("subprocess succeeds");
    assert_eq!(token, "ya29.test-token-stdout");
    assert_eq!(source.provider_label(), "vertex");
}

#[tokio::test]
async fn current_key_trims_trailing_whitespace() {
    // gcloud's print-access-token emits a trailing newline; we must
    // strip it so the token doesn't end up with stray bytes in the
    // Authorization header.
    let source = VertexOAuthSource::with_command(
        "/bin/sh",
        vec![
            "-c".to_string(),
            "printf 'ya29.with-newline\\n'".to_string(),
        ],
    );
    let token = source.current_key().await.expect("subprocess succeeds");
    assert_eq!(token, "ya29.with-newline");
}

#[tokio::test]
async fn current_key_caches_across_calls() {
    // The cache lives for the configured TTL. Use a marker so we can
    // verify the second call serves the same value when the script is
    // deterministic, then prove the cache key isn't time-derived.
    let marker = unique_marker("cache");
    let source = VertexOAuthSource::with_command("/bin/sh", shell_emits(&marker))
        .with_ttl(Duration::from_secs(10 * 60));
    let first = source.current_key().await.expect("first");
    let second = source.current_key().await.expect("second");
    assert_eq!(first, second);
    assert_eq!(first, marker);
    assert!(!source.needs_refresh().await);
}

#[tokio::test]
async fn invalidate_forces_next_call_to_refresh() {
    // After `invalidate()` the cache is dropped; the very next
    // `current_key()` must respawn the subprocess. We use a script
    // that bumps a counter via a temp file so we can prove the
    // refresh happened.
    let counter = std::env::temp_dir().join(unique_marker("invalidate-counter"));
    std::fs::write(&counter, "0").expect("seed counter");
    let counter_str = counter.display().to_string();
    let source = VertexOAuthSource::with_command(
        "/bin/sh",
        vec![
            "-c".to_string(),
            format!(
                "n=$(cat {p}); n=$((n+1)); printf %s $n > {p}; printf 'ya29.token-call-%s' $n",
                p = counter_str
            ),
        ],
    );

    let first = source.current_key().await.expect("first call");
    assert_eq!(first, "ya29.token-call-1");

    // Second call without invalidate should hit the cache.
    let cached = source.current_key().await.expect("cached call");
    assert_eq!(cached, "ya29.token-call-1");

    source.invalidate().await.expect("invalidate ok");
    assert!(source.needs_refresh().await);

    let refreshed = source.current_key().await.expect("post-invalidate call");
    assert_eq!(refreshed, "ya29.token-call-2");

    let _ = std::fs::remove_file(&counter);
}

#[tokio::test]
async fn missing_executable_surfaces_provider_not_configured() {
    // The subprocess failure path the doctor command will see when
    // `gcloud` isn't installed on the host. We use a deliberately
    // bogus binary name so the call always fails fast.
    let source = VertexOAuthSource::with_command(
        "/nonexistent/path/to/gcloud-binary-that-cannot-exist",
        vec!["auth".to_string()],
    );
    let err = source
        .current_key()
        .await
        .expect_err("missing binary must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("not found") || msg.contains("failed to spawn"),
        "expected install hint, got: {msg}"
    );
}

#[tokio::test]
async fn nonzero_exit_surfaces_stderr_in_error_message() {
    // gcloud refuses to print a token when ADC isn't logged in.
    // Simulate the bad-exit path: stderr carries the actionable
    // message and must survive to the caller.
    let source = VertexOAuthSource::with_command(
        "/bin/sh",
        vec![
            "-c".to_string(),
            "printf 'ERROR: gcloud auth application-default login required\\n' 1>&2; exit 1"
                .to_string(),
        ],
    );
    let err = source
        .current_key()
        .await
        .expect_err("nonzero exit must propagate");
    let msg = err.to_string();
    assert!(
        msg.contains("ERROR: gcloud auth application-default login"),
        "stderr must reach the user, got: {msg}"
    );
    assert!(msg.contains("exit 1"), "exit code surfaced: {msg}");
}

#[tokio::test]
async fn nonzero_exit_truncates_oversized_stderr() {
    // A pathological gcloud could dump a huge stderr; the error message
    // bounds it so it stays diagnosable without blowing up. Emit far
    // more than the 512-byte cap and assert the truncation marker.
    let source = VertexOAuthSource::with_command(
        "/bin/sh",
        vec![
            "-c".to_string(),
            // 2000 'x' bytes to stderr, then a non-zero exit.
            "yes x | head -c 2000 | tr -d '\\n' 1>&2; exit 1".to_string(),
        ],
    );
    let err = source
        .current_key()
        .await
        .expect_err("nonzero exit must propagate");
    let msg = err.to_string();
    assert!(msg.contains("(truncated)"), "stderr must be truncated");
    // The full 2000-byte payload must not survive into the message.
    assert!(
        msg.len() < 1000,
        "truncated message should be bounded, got {} bytes",
        msg.len()
    );
}

#[tokio::test]
async fn empty_stdout_surfaces_provider_not_configured() {
    // A successful exit with an empty token usually means the user
    // ran `print-access-token` with no ADC at all. Surface the same
    // structured error the install path uses so the doctor command
    // can branch on it.
    let source =
        VertexOAuthSource::with_command("/bin/sh", vec!["-c".to_string(), "printf ''".to_string()]);
    let err = source
        .current_key()
        .await
        .expect_err("empty token must fail");
    let msg = err.to_string();
    assert!(msg.contains("empty token"), "{msg}");
    assert!(msg.contains("application-default login"), "{msg}");
}

#[tokio::test]
async fn timeout_surfaces_provider_not_configured() {
    // A wedged subprocess shouldn't hang the agent forever. We use a
    // sleep longer than the configured timeout and expect the
    // structured timeout error.
    let source =
        VertexOAuthSource::with_command("/bin/sh", vec!["-c".to_string(), "sleep 5".to_string()])
            .with_timeout(Duration::from_millis(100));
    let err = source
        .current_key()
        .await
        .expect_err("subprocess must time out");
    let msg = err.to_string();
    assert!(msg.contains("did not respond"), "{msg}");
}

#[tokio::test]
async fn cached_token_expires_after_ttl() {
    // Short TTL + deterministic script: cache returns the first
    // value, then after `sleep(ttl + epsilon)` the next call hits
    // the subprocess again.
    let counter = std::env::temp_dir().join(unique_marker("ttl-counter"));
    std::fs::write(&counter, "0").expect("seed counter");
    let counter_str = counter.display().to_string();
    let source = VertexOAuthSource::with_command(
        "/bin/sh",
        vec![
            "-c".to_string(),
            format!(
                "n=$(cat {p}); n=$((n+1)); printf %s $n > {p}; printf 'ya29.ttl-%s' $n",
                p = counter_str
            ),
        ],
    )
    .with_ttl(Duration::from_millis(50));

    let first = source.current_key().await.expect("first");
    assert_eq!(first, "ya29.ttl-1");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(source.needs_refresh().await);
    let second = source.current_key().await.expect("post-ttl");
    assert_eq!(second, "ya29.ttl-2");

    let _ = std::fs::remove_file(&counter);
}

#[tokio::test]
async fn debug_redacts_token_state() {
    // Provider client `Debug` output sometimes flows into bug
    // reports; the cached token must not leak. We stage the token in
    // a temp file so the script command line doesn't itself carry the
    // secret — that way a positive assertion proves the *cached*
    // value (not the args) was redacted.
    let token_file = std::env::temp_dir().join(unique_marker("debug-redact"));
    std::fs::write(&token_file, "ya29.secret-token-value").expect("seed token file");
    let source =
        VertexOAuthSource::with_command("/bin/cat", vec![token_file.display().to_string()]);
    let token = source.current_key().await.expect("warm cache");
    assert_eq!(token, "ya29.secret-token-value");
    let debug = format!("{source:?}");
    assert!(
        !debug.contains("ya29.secret-token-value"),
        "token leaked into Debug output: {debug}"
    );
    assert!(debug.contains("<redacted>"), "no redaction marker: {debug}");
    let _ = std::fs::remove_file(&token_file);
}

#[test]
fn command_path_returns_the_configured_command() {
    // Diagnostic surface for `auth status`/`doctor`: which `gcloud`
    // executable will the source actually spawn?
    let source = VertexOAuthSource::with_command(
        "/opt/google-cloud-sdk/bin/gcloud",
        vec!["auth".to_string()],
    );
    assert_eq!(
        source.command_path(),
        PathBuf::from("/opt/google-cloud-sdk/bin/gcloud")
    );
}

#[tokio::test]
#[ignore = "requires `gcloud` and a configured ADC; opt in by removing #[ignore]"]
async fn real_gcloud_prints_a_token() {
    // Opt-in smoke test for a developer with ADC already configured.
    // Marked `#[ignore]` so CI doesn't fail when `gcloud` is absent.
    let source = VertexOAuthSource::new();
    let token = source.current_key().await.expect("real gcloud call");
    assert!(
        token.starts_with("ya29.") || !token.is_empty(),
        "unexpected token shape: {}",
        &token[..token.len().min(8)]
    );
}
