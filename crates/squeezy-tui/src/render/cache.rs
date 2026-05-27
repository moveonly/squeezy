//! Read-through LRU caches for markdown / highlight / diff rendering.
//!
//! The ratatui redraw model fires on resize, scroll, focus, key, and
//! every async event. Without caching, a long transcript re-parses each
//! assistant message, re-tree-sits each fenced block, and re-parses each
//! patch on every frame. These three caches key on a content hash (and,
//! for diff/highlight, the discriminators the renderer actually consumes)
//! so an idle transcript pays the parse cost once per unique block.
//!
//! Cache values are [`Arc<Vec<Line<'static>>>`] so a hit clones the Arc
//! rather than reallocating the rendered lines; callers receive an owned
//! `Vec<Line<'static>>` for compatibility with the existing render APIs.
//!
//! Cap rationale:
//! - markdown: 256 distinct assistant messages — covers extended sessions
//!   (clear-code uses 500, sized for a busier Node host).
//! - highlight: 256 distinct `(content, language)` pairs — fenced blocks
//!   are usually small (10–500 lines), single-digit KB per entry.
//! - diff: 64 distinct `(path, patch)` pairs — diffs are the heaviest
//!   payload and rarely repeat per session.
//!
//! Palette tone and color level are intentionally omitted from cache
//! keys. Both are [`OnceLock`'d at startup in `render::palette`]; a
//! runtime palette swap would require a manual `clear_all` flush, which
//! is acceptable for a debug-only feature.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use lru::LruCache;
use ratatui::text::Line;
use sha2::{Digest, Sha256};

const MARKDOWN_CAPACITY: usize = 256;
const HIGHLIGHT_CAPACITY: usize = 256;
const DIFF_CAPACITY: usize = 64;

type LineVec = Arc<Vec<Line<'static>>>;

fn markdown_cache() -> &'static Mutex<LruCache<u64, LineVec>> {
    static CACHE: OnceLock<Mutex<LruCache<u64, LineVec>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MARKDOWN_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

fn highlight_cache() -> &'static Mutex<LruCache<(u64, &'static str), LineVec>> {
    static CACHE: OnceLock<Mutex<LruCache<(u64, &'static str), LineVec>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(HIGHLIGHT_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

fn diff_cache() -> &'static Mutex<LruCache<(PathBuf, u64), LineVec>> {
    static CACHE: OnceLock<Mutex<LruCache<(PathBuf, u64), LineVec>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(DIFF_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Stable content fingerprint. First 8 bytes of SHA-256 reused so that
/// every call site in the crate hashes the same way (sha2 is already a
/// workspace dependency for plan / streaming-patch identifiers).
pub(crate) fn hash_content(bytes: &[u8]) -> u64 {
    let digest = Sha256::digest(bytes);
    u64::from_le_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

#[cfg(test)]
pub(crate) fn markdown_len() -> usize {
    markdown_cache().lock().map(|c| c.len()).unwrap_or(0)
}

pub(crate) fn get_or_compute_markdown(
    content: &str,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let key = hash_content(content.as_bytes());
    if let Ok(mut cache) = markdown_cache().lock()
        && let Some(value) = cache.get(&key)
    {
        return (**value).clone();
    }
    let computed = Arc::new(compute());
    let out = (*computed).clone();
    if let Ok(mut cache) = markdown_cache().lock() {
        cache.put(key, computed);
    }
    out
}

pub(crate) fn get_or_compute_highlight(
    content: &str,
    language: &'static str,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let key = (hash_content(content.as_bytes()), language);
    if let Ok(mut cache) = highlight_cache().lock()
        && let Some(value) = cache.get(&key)
    {
        return (**value).clone();
    }
    let computed = Arc::new(compute());
    let out = (*computed).clone();
    if let Ok(mut cache) = highlight_cache().lock() {
        cache.put(key, computed);
    }
    out
}

pub(crate) fn get_or_compute_diff(
    path: &str,
    patch: &str,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let key = (PathBuf::from(path), hash_content(patch.as_bytes()));
    if let Ok(mut cache) = diff_cache().lock()
        && let Some(value) = cache.get(&key)
    {
        return (**value).clone();
    }
    let computed = Arc::new(compute());
    let out = (*computed).clone();
    if let Ok(mut cache) = diff_cache().lock() {
        cache.put(key, computed);
    }
    out
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
