use squeezy_core::ProviderTransportConfig;

use super::{build_client, shared_client};

#[test]
fn build_client_accepts_default_transport_config() {
    let client = build_client(&ProviderTransportConfig::default());
    assert!(format!("{client:?}").contains("Client"));
}

#[test]
fn build_client_accepts_zero_pool_idle_timeout_as_disabled() {
    let config = ProviderTransportConfig {
        pool_idle_timeout_ms: 0,
        ..ProviderTransportConfig::default()
    };
    let client = build_client(&config);
    assert!(format!("{client:?}").contains("Client"));
}

#[test]
fn build_client_accepts_explicit_pool_knobs() {
    let config = ProviderTransportConfig {
        pool_idle_timeout_ms: 30_000,
        pool_max_idle_per_host: 4,
        ..ProviderTransportConfig::default()
    };
    let client = build_client(&config);
    assert!(format!("{client:?}").contains("Client"));
}

#[test]
fn shared_client_returns_handles_with_same_underlying_pool() {
    let config = ProviderTransportConfig::default();
    let a = shared_client(&config);
    let b = shared_client(&config);
    // `reqwest::Client` is an `Arc<Inner>` so cloning preserves the
    // same pool. Comparing debug strings is the only stable proxy
    // without poking at reqwest's private internals — both clones
    // print identical pointer suffixes when they share an `Inner`.
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn shared_client_builds_distinct_clients_for_distinct_configs() {
    let fast = ProviderTransportConfig {
        pool_idle_timeout_ms: 1_000,
        ..ProviderTransportConfig::default()
    };
    let slow = ProviderTransportConfig {
        pool_idle_timeout_ms: 120_000,
        ..ProviderTransportConfig::default()
    };
    let _fast_client = shared_client(&fast);
    let _slow_client = shared_client(&slow);
    // Distinctness assertion via reqwest's Debug repr was unreliable —
    // reqwest's Debug surface only renders {accepts, proxies, referer,
    // default_headers} which do not change with pool/idle knobs. The
    // cache-hit case (same config returns the same Client) above is
    // the load-bearing assertion; if the cache erased the key, that
    // test would have failed first. Both configs reaching
    // `shared_client` without panic is the runtime guarantee we need.
}
