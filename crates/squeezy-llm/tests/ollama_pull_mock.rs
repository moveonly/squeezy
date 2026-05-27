//! End-to-end test for `pull_model` using a loopback TCP server that
//! emulates Ollama's `POST /api/pull` NDJSON stream.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_llm::{PullEvent, pull_model};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const NDJSON_BODY: &str = concat!(
    r#"{"status":"pulling manifest"}"#,
    "\n",
    r#"{"status":"downloading","digest":"sha256:abc","total":1024,"completed":256}"#,
    "\n",
    r#"{"status":"downloading","digest":"sha256:abc","total":1024,"completed":1024}"#,
    "\n",
    r#"{"status":"verifying sha256 digest"}"#,
    "\n",
    r#"{"status":"success"}"#,
    "\n",
);

const NDJSON_ERROR_BODY: &str = concat!(
    r#"{"status":"pulling manifest"}"#,
    "\n",
    r#"{"error":"pull model manifest: file does not exist"}"#,
    "\n",
);

async fn spawn_pull_server(body: &'static str, status_code: u16) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            // Drain request headers + (small) JSON body so the POST completes
            // before we stream the response.
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            let body_bytes = body.as_bytes();
            let headers = format!(
                "HTTP/1.1 {status_code} OK\r\n\
                 Content-Type: application/x-ndjson\r\n\
                 Content-Length: {}\r\n\
                 \r\n",
                body_bytes.len()
            );
            if stream.write_all(headers.as_bytes()).await.is_err() {
                continue;
            }
            let _ = stream.write_all(body_bytes).await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

#[tokio::test]
async fn pull_model_streams_progress_and_terminates_on_success() {
    let addr = spawn_pull_server(NDJSON_BODY, 200).await;
    let base_url = format!("http://{addr}");

    let stream = pull_model(&base_url, "qwen3-coder", CancellationToken::new());
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("pull stream completes within timeout")
        .into_iter()
        .map(|res| res.expect("pull stream must not error"))
        .collect();

    let statuses: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            PullEvent::Status(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        statuses.contains(&"pulling manifest"),
        "saw statuses: {statuses:?}"
    );
    assert!(
        statuses.contains(&"verifying sha256 digest"),
        "saw statuses: {statuses:?}"
    );

    let progress_completed: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            PullEvent::Progress {
                completed: Some(c), ..
            } => Some(*c),
            _ => None,
        })
        .collect();
    assert_eq!(progress_completed, vec![256, 1024]);

    assert!(
        matches!(events.last(), Some(PullEvent::Success)),
        "final event must be Success, got: {events:?}"
    );
}

#[tokio::test]
async fn pull_model_surfaces_inline_error_as_stream_failure() {
    let addr = spawn_pull_server(NDJSON_ERROR_BODY, 200).await;
    let base_url = format!("http://{addr}");

    let mut stream = pull_model(&base_url, "missing-model", CancellationToken::new());
    let mut had_error = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(PullEvent::Success) => panic!("expected error, got Success"),
            Ok(_) => continue,
            Err(err) => {
                had_error = true;
                let message = err.to_string();
                assert!(
                    message.contains("file does not exist"),
                    "expected error message to mention missing file, got: {message}"
                );
                break;
            }
        }
    }
    assert!(had_error, "stream must surface server-side error");
}
