//! Centralized [`reqwest::Client`] factory for the native Squeezy
//! providers.
//!
//! Mirrors pi's `configureHttpDispatcher`
//! (`packages/coding-agent/src/core/http-dispatcher.ts`) — pi installs a
//! single global undici dispatcher with a configurable
//! `bodyTimeout`/`headersTimeout` so every fetch in the process shares the
//! same connection pool and inherits the idle-timeout knob the user
//! picked. Each Squeezy provider previously called
//! [`reqwest::Client::new`] in its constructor, which meant:
//!
//! - Multi-provider routing (e.g. small-fast vs main, or model
//!   switches mid-session) paid the DNS/TLS handshake cost on every
//!   provider swap because the pools were separate.
//! - The user had no `[transport]` lever for tuning the HTTP pool;
//!   only `stream_idle_timeout_ms` (per-event SSE gating) was wired
//!   through.
//!
//! This module exposes [`shared_client`], a process-wide memoized
//! factory keyed on [`ProviderTransportConfig`]. Two providers carrying
//! the same transport config now share one [`reqwest::Client`] — and
//! therefore one TCP/TLS pool — for the lifetime of the process. The
//! returned `Client` is itself an `Arc` handle so cloning into a
//! provider field is cheap and the underlying pool stays alive as long
//! as any caller holds a reference.
//!
//! ## Tuning knobs surfaced via `ProviderTransportConfig`
//!
//! - `pool_idle_timeout_ms`: how long an idle socket sits in the pool
//!   before reqwest drops it. Mirrors pi's `HTTP_IDLE_TIMEOUT_MS`,
//!   though semantically distinct (pi controls bodyTimeout; reqwest's
//!   request-body timeout would clip live streaming bodies and is
//!   intentionally left at "unbounded" here).
//! - `pool_max_idle_per_host`: cap on idle connections kept per
//!   origin. `u32::MAX` is treated as "unbounded" (reqwest default).
//!
//! ## Why no `Client::timeout(...)`
//!
//! Setting `timeout` would cap *total* request lifetime — fine for a
//! REST POST but catastrophic for a long reasoning stream, which can
//! legitimately run for minutes between completion. Per-event SSE
//! idle timing is enforced inside each provider's stream loop via
//! `tokio::time::timeout(idle_timeout(transport), bytes.next())`,
//! governed by `stream_idle_timeout_ms`. That is the long-stream
//! hang defense; the pool knobs here are about socket reuse, not
//! stream lifetime.
//!
//! ## Bedrock gap
//!
//! [`crate::BedrockProvider`] does not flow through this factory.
//! Bedrock requests go through `aws-sdk-bedrockruntime`, which builds
//! and manages its own HTTP transport (`aws-smithy-runtime`'s hyper
//! adapter). Centralizing it would require swapping the smithy
//! `HttpConnector`, which is out of scope for F08 and would interfere
//! with the AWS SDK's retry/credential refresh story. The transport
//! knobs that already do reach Bedrock (`stream_idle_timeout_ms`)
//! continue to be honored in [`crate::bedrock`] via the same
//! `tokio::time::timeout` pattern other providers use.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use squeezy_core::ProviderTransportConfig;

/// Returns the shared [`reqwest::Client`] for `config`. Two providers
/// carrying equal `ProviderTransportConfig` values share one pool for
/// the lifetime of the process. Falls back to a freshly built client
/// whenever the cache mutex is poisoned (extremely rare; only if a
/// builder panic poisoned the lock) so callers never see a panic
/// propagated from the cache layer.
pub fn shared_client(config: &ProviderTransportConfig) -> reqwest::Client {
    static CACHE: OnceLock<Mutex<HashMap<ProviderTransportConfig, reqwest::Client>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    match cache.lock() {
        Ok(mut guard) => guard
            .entry(*config)
            .or_insert_with(|| build_client(config))
            .clone(),
        Err(_) => {
            // Lock poisoned (a previous builder panicked under the
            // lock). Fall back to a non-cached client so the agent
            // can still issue requests. Logged at `warn` so the next
            // run can be diagnosed without halting the current one.
            tracing::warn!(
                target: "squeezy_llm::transport",
                "shared HTTP client cache poisoned; returning uncached client",
            );
            build_client(config)
        }
    }
}

/// Build a fresh [`reqwest::Client`] honoring the pool knobs in
/// `config`. Extracted from [`shared_client`] so unit tests can
/// exercise the builder without populating the process-wide cache
/// (and so the fallback path on a poisoned lock has a single code
/// path to maintain).
pub(crate) fn build_client(config: &ProviderTransportConfig) -> reqwest::Client {
    let mut builder =
        reqwest::Client::builder().pool_max_idle_per_host(config.pool_max_idle_per_host as usize);
    builder = if config.pool_idle_timeout_ms == 0 {
        // `None` keeps idle sockets parked indefinitely (pi's
        // "disabled" choice). Matches the user-visible semantic of
        // `HTTP_IDLE_TIMEOUT_MS=0` in pi's selector UI.
        builder.pool_idle_timeout(None)
    } else {
        builder.pool_idle_timeout(Some(Duration::from_millis(config.pool_idle_timeout_ms)))
    };
    builder
        .build()
        .expect("reqwest::Client builder must succeed with default TLS backend")
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
