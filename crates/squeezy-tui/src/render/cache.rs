//! Read-through LRU caches for markdown / highlight / diff rendering,
//! plus a top-level per-entry cache that memoises the fully-composed
//! transcript-entry line list (header + body + separator).
//!
//! The ratatui redraw model fires on resize, scroll, focus, key, and
//! every async event. Without caching, a long transcript re-parses each
//! assistant message, re-tree-sits each fenced block, and re-parses each
//! patch on every frame. The three lower caches key on a content hash
//! (and, for diff/highlight, the discriminators the renderer actually
//! consumes) so an idle transcript pays the parse cost once per unique
//! block. The per-entry cache (`get_or_compute_entry`) sits one layer
//! up: it keys on the transcript entry's stable id and only recomputes
//! when either the entry itself bumps its content `revision` or a
//! global rendering knob (palette tone/accent or per-render context
//! such as `selected` / `width` / verbosity) changes.
//!
//! Cache values are [`Arc<Vec<Line<'static>>>`] so a hit clones the Arc
//! rather than reallocating the rendered lines; callers receive an owned
//! `Vec<Line<'static>>` for compatibility with the existing render APIs.
//!
//! Cap rationale:
//! - markdown: 256 distinct assistant messages — covers extended sessions
//!   without blowing the heap on a long conversation.
//! - highlight: 256 distinct `(content, language)` pairs — fenced blocks
//!   are usually small (10–500 lines), single-digit KB per entry.
//! - diff: 64 distinct `(path, patch)` pairs — diffs are the heaviest
//!   payload and rarely repeat per session.
//! - entry: 1024 distinct transcript entries — comfortably exceeds the
//!   "very long conversation" target while keeping the LRU bounded.
//!
//! Palette tone and color level are intentionally omitted from the
//! per-block (markdown/highlight/diff) cache keys; those caches operate
//! below the palette boundary. The per-entry cache instead reads
//! `palette::palette_generation()` at lookup time and folds it into the
//! validity tag, so a `/theme` switch transparently invalidates every
//! cached entry without forcing a manual `clear_all`.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use lru::LruCache;
use ratatui::text::Line;
use sha2::{Digest, Sha256};

const MARKDOWN_CAPACITY: usize = 256;
const HIGHLIGHT_CAPACITY: usize = 256;
const DIFF_CAPACITY: usize = 64;
/// Per-entry cap. Sized for "very long conversation" scenarios where
/// the transcript pushes well past a few hundred entries — at one card
/// per ~50 lines the cache still fits comfortably in memory, while the
/// LRU bound prevents an open session from accumulating cached entries
/// indefinitely as `/compact` rotations and pinned scrollback grow.
const ENTRY_CAPACITY: usize = 1024;

type LineVec = Arc<Vec<Line<'static>>>;
/// `(content_hash, theme_generation, render_mode)`. The `render_mode`
/// discriminant separates Compact and Full renders of the same source +
/// theme, which produce different line output and would otherwise collide
/// on a shared slot.
type MarkdownKey = (u64, u64, u8);
type HighlightKey = (u64, &'static str, u64);
type DiffKey = (PathBuf, u64, u64);
type MarkdownCache = Mutex<LruCache<MarkdownKey, LineVec>>;
type HighlightCache = Mutex<LruCache<HighlightKey, LineVec>>;
type DiffCache = Mutex<LruCache<DiffKey, LineVec>>;

/// Validity tag for one cached entry render. Held alongside the rendered
/// lines so a lookup can verify that none of the inputs that drove the
/// rendering have shifted since insertion. Mismatch on any field forces
/// a recompute, which is the only mechanism we use to invalidate — the
/// LRU itself never selectively evicts on state change, it only bounds
/// the total entry count.
#[derive(Clone)]
struct CachedEntryRender {
    /// Per-entry monotonic content revision. Bumps on every mutation of
    /// the entry's payload (streaming chunks landing, tool-call coalesce
    /// rolling another retry in, collapse toggle, etc.).
    entry_revision: u64,
    /// Snapshot of `palette::palette_generation()` at insertion time.
    /// Lets a theme switch invalidate every cached entry implicitly,
    /// since the live counter will have moved past the snapshot.
    palette_generation: u64,
    /// Per-render context fingerprint hashed by the caller: bundles the
    /// inputs that vary between draws *within* a stable palette
    /// (selected flag, terminal width, verbosity, outcome, etc.). Kept
    /// as an opaque `u64` so this module stays agnostic of the caller's
    /// context shape — only equality matters.
    context_hash: u64,
    lines: LineVec,
}

fn markdown_cache() -> &'static MarkdownCache {
    static CACHE: OnceLock<MarkdownCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MARKDOWN_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

fn highlight_cache() -> &'static HighlightCache {
    static CACHE: OnceLock<HighlightCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(HIGHLIGHT_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

fn diff_cache() -> &'static DiffCache {
    static CACHE: OnceLock<DiffCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(DIFF_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Entry cache key. Includes a `session` discriminator so multiple
/// `TuiApp` instances (notably parallel unit tests sharing the test
/// binary, but also a long-running squeezy process that creates a
/// second app for any reason) cannot collide on the per-app entry ids,
/// which always restart from `0` per session.
type EntryKey = (u64, u64);

fn entry_cache() -> &'static Mutex<LruCache<EntryKey, CachedEntryRender>> {
    static CACHE: OnceLock<Mutex<LruCache<EntryKey, CachedEntryRender>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(ENTRY_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Allocate a fresh session id. Each `TuiApp` calls this once at
/// construction and uses the returned value as the first half of every
/// per-entry cache key for the lifetime of the app, isolating that
/// session's cached lines from any other session that might share the
/// same process (most relevantly: parallel `cargo test` invocations).
///
/// Returns u64::MAX of distinct values before wrapping, which is
/// effectively unbounded for any realistic process lifetime.
pub(crate) fn next_session_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
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

#[cfg(test)]
pub(crate) fn entry_len() -> usize {
    entry_cache().lock().map(|c| c.len()).unwrap_or(0)
}

pub(crate) fn get_or_compute_markdown(
    content: &str,
    render_mode: u8,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let key = (
        hash_content(content.as_bytes()),
        crate::render::theme::theme_generation(),
        render_mode,
    );
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
    let key = (
        hash_content(content.as_bytes()),
        language,
        crate::render::theme::theme_generation(),
    );
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
    let key = (
        PathBuf::from(path),
        hash_content(patch.as_bytes()),
        crate::render::theme::theme_generation(),
    );
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

/// Memoise the fully-composed line list for one transcript entry.
///
/// The cache is keyed by `(session_id, entry_id)` and validated against
/// the triple `(entry_revision, palette_generation, context_hash)`. A
/// hit returns the cached lines without invoking `compute`. A miss (or
/// a validity mismatch on any of the three tags) runs `compute`, stores
/// the result under the new tags, and returns the freshly rendered
/// lines. Eviction is plain LRU bounded by [`ENTRY_CAPACITY`].
///
/// Correctness contract: callers must bump `entry_revision` whenever
/// the entry's payload changes (streaming, coalesce, collapse toggle),
/// and must fold any per-render context that affects line content into
/// `context_hash`. The palette generation is sampled by the caller from
/// [`palette::palette_generation`](crate::render::palette::palette_generation)
/// so a theme switch invalidates every cached entry implicitly.
///
/// The `session_id` discriminator isolates apps that share a process —
/// without it, two `TuiApp` instances each starting `entry_id` at `0`
/// would clobber each other through the shared LRU.
pub(crate) fn get_or_compute_entry(
    session_id: u64,
    entry_id: u64,
    entry_revision: u64,
    palette_generation: u64,
    context_hash: u64,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let key = (session_id, entry_id);
    if let Ok(mut cache) = entry_cache().lock()
        && let Some(cached) = cache.get(&key)
        && cached.entry_revision == entry_revision
        && cached.palette_generation == palette_generation
        && cached.context_hash == context_hash
    {
        return (*cached.lines).clone();
    }
    let computed = Arc::new(compute());
    let out = (*computed).clone();
    if let Ok(mut cache) = entry_cache().lock() {
        cache.put(
            key,
            CachedEntryRender {
                entry_revision,
                palette_generation,
                context_hash,
                lines: computed,
            },
        );
    }
    out
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
