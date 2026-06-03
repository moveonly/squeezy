//! Centralized [`reqwest::Client`] factory for the native Squeezy
//! providers.
//!
//! A single process-wide HTTP client lets every fetch share one TCP/TLS
//! pool and inherit the idle-timeout knob the user picked. Each Squeezy
//! provider previously called [`reqwest::Client::new`] in its
//! constructor, which meant:
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
//!   before reqwest drops it. Reqwest's request-body timeout would
//!   clip live streaming bodies and is intentionally left at
//!   "unbounded" here.
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

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lru::LruCache;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use squeezy_core::{
    ProviderTransportConfig, is_metadata_or_link_local_host, is_metadata_or_link_local_ip,
};

/// Upper bound on the number of distinct [`ProviderTransportConfig`]
/// values whose [`reqwest::Client`]s are kept warm in the shared cache.
/// A long session can rotate through cheap/main/judge profiles plus
/// any custom presets the user toggles, and the previous unbounded
/// `HashMap` would retain a `Client` (each holding its own connection
/// pool, TLS context, and DNS resolver) for every distinct config ever
/// seen — even ones the agent no longer uses. 32 covers every routing
/// permutation any real agent loadout reaches (currently <=10) while
/// still bounding worst-case memory in the face of a runaway config
/// generator (e.g. tests, fuzz harnesses) that would otherwise leak.
const SHARED_CLIENT_CACHE_CAPACITY: usize = 32;

type SharedClientCache = Mutex<LruCache<ProviderTransportConfig, reqwest::Client>>;

/// Returns the shared [`reqwest::Client`] for `config`. Two providers
/// carrying equal `ProviderTransportConfig` values share one pool for
/// the lifetime of the process, up to [`SHARED_CLIENT_CACHE_CAPACITY`]
/// distinct configs — past that, the least-recently-used entry is
/// evicted on insert and its `reqwest::Client` (and underlying pool)
/// is dropped once the last outstanding clone the providers hold goes
/// out of scope. Falls back to a freshly built client whenever the
/// cache mutex is poisoned (extremely rare; only if a builder panic
/// poisoned the lock) so callers never see a panic propagated from the
/// cache layer.
pub fn shared_client(config: &ProviderTransportConfig) -> reqwest::Client {
    static CACHE: OnceLock<SharedClientCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(SHARED_CLIENT_CACHE_CAPACITY)
                .expect("SHARED_CLIENT_CACHE_CAPACITY is a non-zero literal"),
        ))
    });
    match cache.lock() {
        Ok(mut guard) => {
            // `LruCache::get_or_insert` both bumps the recency of an
            // existing entry and inserts on miss, evicting the LRU
            // entry once the cache is at capacity. The returned `&V`
            // is cloned into a fresh `Arc` handle for the caller so
            // the provider can outlive any future eviction without
            // tearing down its in-flight requests.
            guard
                .get_or_insert(*config, || build_client(config))
                .clone()
        }
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
    build_client_with_resolver(config, MetadataBlockingResolver::system())
}

/// Variant of [`build_client`] that accepts an arbitrary DNS resolver
/// so unit tests can pin lookups to canned addresses without standing up
/// a real server. Production callers go through [`build_client`], which
/// installs the [`MetadataBlockingResolver`] wrapper around the system
/// resolver.
pub(crate) fn build_client_with_resolver(
    config: &ProviderTransportConfig,
    resolver: MetadataBlockingResolver,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(config.pool_max_idle_per_host as usize)
        // Identifies the agent to upstream providers and any in-path
        // proxy. Providers use the User-Agent for routing, rate-limit
        // bucketing, and abuse triage; emitting `squeezy-cli/<ver>`
        // lets the upstream attribute traffic to us instead of the
        // anonymous `reqwest/<ver>` default.
        .user_agent(concat!("squeezy-cli/", env!("CARGO_PKG_VERSION")))
        // Bounds the *connect* (DNS + TCP + TLS) handshake only.
        .connect_timeout(Duration::from_secs(30))
        // Probes idle sockets so a pool entry silently killed by a
        // NAT/firewall is detected on the next reuse.
        .tcp_keepalive(Duration::from_secs(60))
        // Re-validate every redirect hop. The custom DNS resolver only fires
        // when reqwest performs a DNS lookup, so a 30x `Location` pointing at
        // a literal metadata/link-local IP would otherwise be followed without
        // ever passing through the resolver. This closes that SSRF hop.
        .redirect(metadata_blocking_redirect_policy())
        .dns_resolver(Arc::new(resolver));
    // Some network paths (corporate proxies, certain middleboxes) reset the
    // HTTP/2 streams reqwest negotiates by default mid-response while leaving
    // HTTP/1.1 untouched, which surfaces as repeated `provider stream`
    // reconnects. `SQUEEZY_FORCE_HTTP1=1` pins the client to HTTP/1.1 as an
    // escape hatch for those environments.
    if std::env::var("SQUEEZY_FORCE_HTTP1")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
    {
        builder = builder.http1_only();
    }
    builder = if config.pool_idle_timeout_ms == 0 {
        // `None` keeps idle sockets parked indefinitely — a
        // `pool_idle_timeout_ms = 0` config explicitly disables
        // eviction.
        builder.pool_idle_timeout(None)
    } else {
        builder.pool_idle_timeout(Some(Duration::from_millis(config.pool_idle_timeout_ms)))
    };
    builder
        .build()
        .expect("reqwest::Client builder must succeed with default TLS backend")
}

/// Redirect policy that refuses any hop whose host is a cloud-metadata
/// sentinel or link-local IP literal, while preserving reqwest's default
/// 10-hop cap. Complements [`MetadataBlockingResolver`]: the resolver guards
/// DNS-resolved hosts, this guards literal-IP `Location` targets the resolver
/// never sees.
fn metadata_blocking_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // `host_str()` keeps the brackets on an IPv6 literal; strip them so the
        // address parses the same way the config-layer check does. Own the
        // string so the borrow on `attempt` ends before `attempt.error(..)`
        // moves it.
        let host = attempt
            .url()
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        if is_metadata_or_link_local_host(&host) {
            return attempt.error(format!(
                "refusing redirect to metadata/link-local host: {host}"
            ));
        }
        if attempt.previous().len() >= 10 {
            return attempt.stop();
        }
        attempt.follow()
    })
}

/// reqwest [`Resolve`] adapter that refuses any DNS lookup resolving to
/// a cloud-metadata sentinel or IPv4/IPv6 link-local address.
///
/// String-level URL allow-listing (see `squeezy_core::check_base_url_scheme`)
/// is insufficient on its own: an attacker who controls the DNS for a
/// benign-looking hostname can answer with a TTL=0 record pointing at
/// `169.254.169.254`. Without a resolver-level guard the validated
/// hostname slips through at config-load and the first request stream
/// connects to AWS IMDS over TLS, shipping the user's Bearer token.
/// Installing this wrapper as the [`reqwest::ClientBuilder::dns_resolver`]
/// re-checks every resolved `IpAddr` on every refresh of the connection
/// pool, so DNS rebinding cannot bypass the literal-host filter.
///
/// Uses the same `is_metadata_or_link_local_ip` predicate the config
/// layer applies to literal IPs so the two block-lists cannot drift.
pub(crate) struct MetadataBlockingResolver {
    inner: Arc<dyn Resolve>,
}

impl MetadataBlockingResolver {
    /// Wrap the platform default (getaddrinfo via `tokio::net::lookup_host`).
    pub(crate) fn system() -> Self {
        Self {
            inner: Arc::new(SystemResolver),
        }
    }

    /// Wrap an arbitrary inner resolver. Used by unit tests so a canned
    /// resolver can simulate DNS rebinding without touching live DNS.
    #[cfg(test)]
    pub(crate) fn wrapping(inner: Arc<dyn Resolve>) -> Self {
        Self { inner }
    }
}

impl Resolve for MetadataBlockingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = self.inner.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = inner.resolve(name).await?;
            let collected: Vec<SocketAddr> = addrs.collect();
            for addr in &collected {
                if is_metadata_or_link_local_ip(&addr.ip()) {
                    let msg = format!(
                        "DNS resolution for host {host:?} produced cloud-metadata or \
                         link-local address {ip}; refusing to connect (possible DNS \
                         rebinding attempt)",
                        ip = addr.ip(),
                    );
                    return Err(BlockedAddressError(msg).into());
                }
            }
            Ok(Box::new(collected.into_iter()) as Addrs)
        })
    }
}

/// Error type emitted when the [`MetadataBlockingResolver`] refuses an
/// address. Wrapped in [`reqwest::Error`] by the connect path; surfaces
/// in upstream errors via `BoxError`.
#[derive(Debug)]
struct BlockedAddressError(String);

impl std::fmt::Display for BlockedAddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BlockedAddressError {}

/// Tokio-backed system resolver invoked by [`MetadataBlockingResolver`].
/// reqwest's bundled `GaiResolver` is `pub(crate)`, so we re-implement the
/// minimal contract: call `tokio::net::lookup_host` and stream the
/// results back.
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Append a sentinel port; reqwest overrides the port using
            // the URL's authority before connecting, so any non-zero
            // value works. Using 0 would make `lookup_host` reject the
            // input outright.
            let target = format!("{host}:443");
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(target)
                .await
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?
                .map(|mut sa| {
                    sa.set_port(0);
                    sa
                })
                .collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Test-only [`Resolve`] that returns the configured `IpAddr`s on every
/// lookup. Used by the DNS-rebinding regression test to swap a benign
/// address for a metadata-sentinel between calls without touching real
/// DNS.
#[cfg(test)]
pub(crate) struct StaticResolver(pub(crate) std::sync::Mutex<Vec<std::net::IpAddr>>);

#[cfg(test)]
impl Resolve for StaticResolver {
    fn resolve(&self, _name: Name) -> Resolving {
        let addrs = self.0.lock().expect("static resolver mutex poisoned");
        let collected: Vec<SocketAddr> = addrs.iter().map(|ip| SocketAddr::new(*ip, 0)).collect();
        Box::pin(std::future::ready(Ok(
            Box::new(collected.into_iter()) as Addrs
        )))
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
