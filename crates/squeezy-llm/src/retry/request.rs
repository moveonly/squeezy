use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::{RequestBuilder, Response, StatusCode};
use squeezy_core::{Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use super::classifier::is_terminal_quota_error;
use super::policy::{RetryPolicy, capped_backoff, should_retry_status, sleep_or_cancel};
use crate::credentials::ApiKeySource;

/// Run [`send_with_retry`] under an outer auth-refresh layer.
///
/// Resolves the API key from `source` once, dispatches the request
/// through the existing transport/throttle retry path, and - only if
/// the upstream comes back with `401`/`403` - calls
/// [`ApiKeySource::invalidate`] and retries the request a single
/// time with a freshly fetched key. A still-`401`/`403` on the
/// second attempt is returned to the caller for the provider's
/// existing error handler to surface.
pub async fn send_with_auth_retry<F>(
    source: &Arc<dyn ApiKeySource>,
    policy: RetryPolicy,
    cancel: &CancellationToken,
    mut make_request: F,
) -> Result<Response>
where
    F: FnMut(&str) -> RequestBuilder,
{
    let key = source.current_key().await?;
    let response = send_with_retry(policy, cancel, || make_request(&key)).await?;
    if !is_auth_failure(response.status()) {
        return Ok(response);
    }
    if !source.can_rotate() {
        tracing::debug!(
            target: "squeezy_llm::auth_retry",
            provider = source.provider_label(),
            status = response.status().as_u16(),
            "skipping auth retry: source cannot rotate credentials",
        );
        return Ok(response);
    }
    tracing::warn!(
        target: "squeezy_llm::auth_retry",
        provider = source.provider_label(),
        status = response.status().as_u16(),
        "upstream rejected api key; invalidating source and retrying once",
    );
    source.invalidate().await?;
    let refreshed = source.current_key().await?;
    send_with_retry(policy, cancel, || make_request(&refreshed)).await
}

fn is_auth_failure(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

pub async fn send_with_retry(
    policy: RetryPolicy,
    cancel: &CancellationToken,
    mut make_request: impl FnMut() -> RequestBuilder,
) -> Result<Response> {
    let mut attempt = 0u8;
    loop {
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(SqueezyError::ProviderStream("cancelled".to_string())),
            response = make_request().send() => response,
        };
        match response {
            Ok(response)
                if should_retry_status(policy, response.status())
                    && attempt < policy.max_retries =>
            {
                let status = response.status();
                let headers = response.headers().clone();
                let retry_after = parse_retry_after(&headers);
                let body_bytes = response.bytes().await.unwrap_or_default();
                if is_terminal_quota_error(&body_bytes) {
                    tracing::warn!(
                        target: "squeezy_llm::retry",
                        status = status.as_u16(),
                        attempt,
                        "terminal quota error detected on retryable status; skipping retry",
                    );
                    return Ok(reconstruct_response(status, headers, body_bytes));
                }
                let delay = retry_after
                    .map(|hint| hint.min(policy.max_retry_delay))
                    .unwrap_or_else(|| capped_backoff(policy, attempt));
                sleep_or_cancel(cancel, delay).await?;
            }
            Ok(response) => return Ok(response),
            Err(error) if policy.retry_transport && attempt < policy.max_retries => {
                let _ = error;
                sleep_or_cancel(cancel, capped_backoff(policy, attempt)).await?;
            }
            Err(error) => return Err(SqueezyError::ProviderRequest(error.to_string())),
        }
        attempt = attempt.saturating_add(1);
    }
}

fn reconstruct_response<B>(
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: B,
) -> Response
where
    B: Into<reqwest::Body>,
{
    let mut http_response = http::Response::new(body);
    *http_response.status_mut() = status;
    *http_response.headers_mut() = headers;
    Response::from(http_response)
}

pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(value) = headers.get("retry-after-ms")
        && let Some(millis) = value.to_str().ok().and_then(|s| s.parse::<u64>().ok())
    {
        return Some(Duration::from_millis(millis));
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let text = value.to_str().ok()?.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(seconds) = text.parse::<f64>()
        && seconds.is_finite()
        && seconds >= 0.0
    {
        const MAX_RETRY_AFTER_MS: f64 = 24.0 * 60.0 * 60.0 * 1_000.0;
        let millis = (seconds * 1_000.0).clamp(0.0, MAX_RETRY_AFTER_MS) as u64;
        return Some(Duration::from_millis(millis));
    }
    let target = httpdate::parse_http_date(text).ok()?;
    Some(target.duration_since(SystemTime::now()).unwrap_or_default())
}
