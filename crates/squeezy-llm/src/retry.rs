use std::time::Duration;

use reqwest::{RequestBuilder, Response, StatusCode};
use squeezy_core::{ProviderTransportConfig, Result, SqueezyError};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u8,
    pub base_delay: Duration,
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryPolicy {
    pub fn provider_requests(config: ProviderTransportConfig) -> Self {
        Self {
            max_retries: config.request_max_retries,
            base_delay: Duration::from_millis(200),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
        }
    }
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
                let retry_after = retry_after_delay(&response).await;
                sleep_or_cancel(
                    cancel,
                    retry_after.unwrap_or_else(|| backoff(policy.base_delay, attempt)),
                )
                .await?;
            }
            Ok(response) => return Ok(response),
            Err(error) if policy.retry_transport && attempt < policy.max_retries => {
                let _ = error;
                sleep_or_cancel(cancel, backoff(policy.base_delay, attempt)).await?;
            }
            Err(error) => return Err(SqueezyError::ProviderRequest(error.to_string())),
        }
        attempt = attempt.saturating_add(1);
    }
}

pub fn idle_timeout(config: ProviderTransportConfig) -> Duration {
    Duration::from_millis(config.stream_idle_timeout_ms)
}

fn should_retry_status(policy: RetryPolicy, status: StatusCode) -> bool {
    policy.retry_429 && status == StatusCode::TOO_MANY_REQUESTS
        || policy.retry_5xx && status.is_server_error()
}

fn backoff(base: Duration, attempt: u8) -> Duration {
    let factor = 2u32.saturating_pow(u32::from(attempt));
    base.saturating_mul(factor)
}

async fn retry_after_delay(response: &Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let seconds = value.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

async fn sleep_or_cancel(cancel: &CancellationToken, duration: Duration) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(SqueezyError::ProviderStream("cancelled".to_string())),
        _ = sleep(duration) => Ok(()),
    }
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
