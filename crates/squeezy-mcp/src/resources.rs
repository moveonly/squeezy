use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use serde_json::Value;

pub(crate) const RESOURCE_READ_CACHE_TTL: Duration = Duration::from_secs(300);
pub(crate) const RESOURCE_DECLARATION_CACHE_TTL: Duration = Duration::from_secs(30);
pub(crate) const RESOURCE_READ_CACHE_CAPACITY: usize = 256;

#[derive(Debug)]
pub(crate) struct CachedResourceRead {
    pub(crate) value: Value,
    pub(crate) fetched_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedResourceDeclarations {
    pub(crate) resource_uris: BTreeSet<String>,
    pub(crate) resource_templates: Vec<String>,
    pub(crate) resource_uris_complete: bool,
    pub(crate) resource_templates_complete: bool,
    pub(crate) fetched_at: Instant,
}

pub(crate) fn insert_resource_read(
    cache: &mut BTreeMap<(String, String), CachedResourceRead>,
    key: (String, String),
    entry: CachedResourceRead,
) {
    cache.retain(|_, v| v.fetched_at.elapsed() <= RESOURCE_READ_CACHE_TTL);
    while cache.len() >= RESOURCE_READ_CACHE_CAPACITY
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, v)| v.fetched_at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, entry);
}
