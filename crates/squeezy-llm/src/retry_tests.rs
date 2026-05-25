use std::time::Duration;

use squeezy_core::ProviderTransportConfig;

use super::{RetryPolicy, idle_timeout};

#[test]
fn provider_requests_policy_inherits_transport_max_retries() {
    let transport = ProviderTransportConfig {
        request_max_retries: 7,
        stream_max_retries: 3,
        stream_idle_timeout_ms: 1_000,
    };
    let policy = RetryPolicy::provider_requests(transport);
    assert_eq!(policy.max_retries, 7);
    assert!(policy.retry_429);
    assert!(policy.retry_5xx);
    assert!(policy.retry_transport);
    assert_eq!(policy.base_delay, Duration::from_millis(200));
}

#[test]
fn idle_timeout_reflects_transport_setting() {
    let transport = ProviderTransportConfig {
        request_max_retries: 0,
        stream_max_retries: 0,
        stream_idle_timeout_ms: 250,
    };
    assert_eq!(idle_timeout(transport), Duration::from_millis(250));
}
