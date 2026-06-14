//! Caching and memory-bounding for the MAIN transcript render (Phase 8).
//!
//! The live main view (`render_transcript`) rebuilds and re-wraps the whole
//! transcript on *every* frame: even an idle, unchanged transcript pays the
//! cross-entry assembly loop and the per-logical-line `wrap_transcript_overlay_line`
//! pass. The Ctrl+T overlay already avoids this with a single-slot,
//! revision-keyed render cache (`with_transcript_overlay_rows`); this module is
//! the analogous layer for the main view, with two differences that the design
//! calls for:
//!
//! 1. **LRU, not single-slot.** A resize storm (drag-resizing the terminal)
//!    walks through many widths in quick succession. A single-slot cache would
//!    thrash — every frame a miss. An LRU keyed by `(width, detail, theme, …)`
//!    keeps a small working set of recent widths warm so the *next* frame at a
//!    width we just saw is a hit. Capacity is bounded so memory never grows
//!    unbounded across a long session. This mirrors `transcript_surface.rs`'s
//!    `row_cache()` LRU shape (cap 32).
//! 2. **Caches `(rows, entry_offsets)` together.** The main row builder
//!    (`transcript_lines_and_entry_offsets`) returns both the wrapped rows and a
//!    per-entry offset map used by jump-nav and card hit-testing. Both are
//!    produced by the same loop, so both are cached together under one key.
//!
//! ## What is cached, and what is NOT
//!
//! The cached value is the *pre-highlight* wrapped rows. Selection and search
//! highlight are layered onto a clone of the rows AFTER they are pulled from the
//! cache (exactly as the live `render_transcript` already does), so the cache
//! key does NOT carry the selection range or the search-match set: the highlight
//! stays a cheap per-frame clone-and-restyle on already-built rows, and the
//! cache survives a selection drag or a search-cursor move without
//! invalidating. The key still records `selected_entry` (which entry is
//! highlighted as the *card* selection, distinct from a text selection) because
//! that genuinely changes the styled card header in the cached rows.
//!
//! ## Correctness contract
//!
//! The key folds *everything* that can change the produced rows or offsets:
//! per-entry content revision, wrap width, the detail/verbosity policy, the
//! active theme (palette generation), the card-selected entry, the conversation
//! source (main vs. a subagent), the live pending stream, the turn divider, the
//! transcript shortcut rebind, and the startup-card toggle. When any of those
//! change the key changes and the entry is recomputed. The painted output is
//! therefore byte-for-byte identical with and without the cache for the same
//! state (proven by
//! `lib_tests::main_render_cached_matches_uncached_byte_for_byte`).
//!
//! ## Settle-fold bypass
//!
//! Entries mid settle-fold (`TranscriptEntry::settle == Some(..)`) render a
//! per-frame eased height that is *not* part of the key (the fold clock is an
//! absolute `Instant`, not `animation_tick`). Rather than fold the wall clock
//! into the key — which would force a miss on literally every frame and pollute
//! the LRU with single-use entries during the fold — the caller bypasses the
//! cache entirely while ANY active-conversation entry is settling (the cheap
//! `entry.settle.is_some()` scan runs over the whole active transcript, not a
//! visible subset). This matches `settle_folded_entry_lines`' existing "outside
//! the per-entry cache" contract.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use lru::LruCache;
use ratatui::text::Line;

/// Cached value: the wrapped rows painted by the main view, the per-entry offset
/// map (`entry_offsets[i]` = wrapped-row index where entry `i`'s block begins),
/// and a per-row [`crate::search::RowKind`] tag in lock-step with the rows so the
/// incremental-search toggles (Ctrl+O / Ctrl+R) can exclude tool-output /
/// reasoning rows on the main surface against the SAME rows that were painted.
/// Stored behind an `Arc` so a hit clones the pointer, not the vectors, then the
/// caller clones the inner `Vec`s once on the way out (the existing render APIs
/// own their `Vec`s).
pub(crate) type MainRows = Arc<(Vec<Line<'static>>, Vec<usize>, Vec<crate::search::RowKind>)>;

/// LRU capacity for the assembled main-render cache.
///
/// Keyed by `(width, detail, theme, …)`, the dominant churn axis is `width`
/// during a resize drag. 24 distinct keys comfortably covers a resize sweep
/// (a terminal rarely visits more than a handful of stable widths in a
/// session) plus the occasional detail/theme toggle, while bounding memory:
/// each value is one fully-wrapped transcript, so the cap is the ceiling on how
/// many full transcripts we hold. Smaller than the per-entry cap because these
/// are whole-transcript snapshots, not single entries.
const MAIN_RENDER_CAPACITY: usize = 24;

/// Per-entry wrapped-row cache capacity. One slot per `(entry, width, detail)`;
/// a long transcript at a stable width fills roughly one slot per entry, and a
/// resize visits each entry again at the new width. Sized to hold a long
/// conversation's worth of entries at one or two recent widths without
/// unbounded growth.
const ENTRY_WRAP_CAPACITY: usize = 2048;

/// Validity-keyed assembled-render LRU key. Derives `Hash`/`Eq` so the whole
/// key participates in the lookup (the row list is rebuilt wholesale on any
/// change, so there is no sub-key to preserve a slot across an input change —
/// same rationale as `transcript_surface::RowCacheKey`).
///
/// Selection/search highlight state is intentionally absent: highlight is a
/// post-pass on a clone of the cached rows, so it must not key the cache.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct MainRenderKey {
    /// Per-app isolation so two `TuiApp`s sharing a process (parallel tests, or
    /// a `/clear` that rotates the session) cannot serve each other's rows.
    pub(crate) render_cache_session: u64,
    /// Live painted text width (resize axis).
    pub(crate) width: u16,
    /// Card-selected entry (styles that entry's header), NOT a text selection.
    pub(crate) selected_entry: Option<usize>,
    /// Tool-output detail/verbosity (the main view's effective detail policy).
    pub(crate) tool_output_verbosity: u8,
    pub(crate) show_reasoning_usage: bool,
    pub(crate) coalesce_tool_runs: bool,
    /// Theme/palette generation — a `/theme` switch bumps it and invalidates.
    pub(crate) palette_generation: u64,
    /// Conversation source (main vs. a specific subagent pane).
    pub(crate) subagent_source_hash: u64,
    /// Fold over every visible `(entry.id, entry.revision)` — per-entry content
    /// revision is the primary invalidation trigger.
    pub(crate) transcript_revision_hash: u64,
    /// Live pending reasoning + assistant tail (uncommitted text has no entry
    /// id/revision, so it is content-hashed).
    pub(crate) pending_hash: u64,
    /// Turn-divider animation snapshot folded to a `u64`.
    pub(crate) turn_divider_hash: u64,
    /// Transcript shortcut rebind (a `[tui.keymap]` change restyles hints).
    pub(crate) shortcut_hash: u64,
    /// Animation phase of the pending-assistant tail's moon span, folded ONLY
    /// while a Running turn is actually showing that tail (see
    /// `main_render_key`). The tail's crescent is tinted by
    /// `TurnVisualState::Running.color(tick)`, which alternates every 4 ticks
    /// (mod 8). Without this dimension a cache hit on momentarily-stable pending
    /// text would freeze the moon's pulse. Held at a constant `0` on every
    /// non-animating frame so idle/settled transcripts still hit regardless of
    /// the live tick.
    pub(crate) tail_anim_phase: u64,
    /// Main-only input: the startup card flips the leading lines.
    pub(crate) include_startup_card: bool,
    /// Main-view Semantic Filter (§12.5.2) folded to a `u64`. A filter change
    /// hides/reveals entries, so it must invalidate the assembled-rows cache or a
    /// hit would serve the pre-filter row set. Held at a constant `0` while the
    /// filter is `All` (the resting state) so an unfiltered transcript still hits
    /// exactly as before this dimension existed.
    pub(crate) semantic_filter_hash: u64,
}

/// Hit/miss counters for the assembled main-render cache. Surfaced as plain
/// atomics so the later instrumentation step can read them without threading a
/// handle through the render path. Bumped on every `get_or_compute` call.
static MAIN_HITS: AtomicU64 = AtomicU64::new(0);
static MAIN_MISSES: AtomicU64 = AtomicU64::new(0);
/// Test-only counter of how many times the cached rows `Vec<Line>` is deep-cloned
/// out of the shared `Arc`. The rows-consuming accessor
/// (`transcript_lines_and_entry_offsets`) bumps it; the offsets-only accessor
/// (`transcript_entry_offsets`) must NOT, which is what the regression test for
/// deep-review #39 asserts.
#[cfg(test)]
static MAIN_ROWS_CLONES: AtomicU64 = AtomicU64::new(0);
/// Hit/miss counters for the per-entry wrapped-row cache.
static ENTRY_WRAP_HITS: AtomicU64 = AtomicU64::new(0);
static ENTRY_WRAP_MISSES: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the four cache counters: `(main_hits, main_misses,
/// entry_wrap_hits, entry_wrap_misses)`.
///
/// The render path bumps these on every lookup; `draw_app` reads this snapshot
/// before and after each painted frame to derive that frame's cache hit/miss
/// deltas for the render-budget metrics (surfaced when `show_render_metrics` is
/// on as the per-frame HUD / trace line).
pub(crate) fn cache_stats() -> (u64, u64, u64, u64) {
    (
        MAIN_HITS.load(Ordering::Relaxed),
        MAIN_MISSES.load(Ordering::Relaxed),
        ENTRY_WRAP_HITS.load(Ordering::Relaxed),
        ENTRY_WRAP_MISSES.load(Ordering::Relaxed),
    )
}

fn main_render_cache() -> &'static Mutex<LruCache<MainRenderKey, MainRows>> {
    static CACHE: OnceLock<Mutex<LruCache<MainRenderKey, MainRows>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MAIN_RENDER_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Read-through, LRU-bounded cache for the assembled main render.
///
/// On a hit returns the stored `Arc` (cheap pointer clone) and bumps the hit
/// counter. On a miss runs `compute`, stores the result, and bumps the miss
/// counter. Eviction is plain LRU bounded by [`MAIN_RENDER_CAPACITY`]; the LRU
/// never *selectively* invalidates — a state change produces a different key,
/// and the stale slot ages out under the bound.
pub(crate) fn get_or_compute_main(
    key: MainRenderKey,
    compute: impl FnOnce() -> (Vec<Line<'static>>, Vec<usize>, Vec<crate::search::RowKind>),
) -> MainRows {
    if let Ok(mut cache) = main_render_cache().lock()
        && let Some(value) = cache.get(&key)
    {
        MAIN_HITS.fetch_add(1, Ordering::Relaxed);
        return Arc::clone(value);
    }
    MAIN_MISSES.fetch_add(1, Ordering::Relaxed);
    let computed: MainRows = Arc::new(compute());
    if let Ok(mut cache) = main_render_cache().lock() {
        cache.put(key, Arc::clone(&computed));
    }
    computed
}

/// Per-entry wrapped-row cache key: `(session_id, entry_id, width, detail_hash)`
/// (see the tuple ordering at the `get_or_compute_entry_wrap` call site). `width`
/// and `detail_hash` are part of the *key*, not validity tags, so a re-wrap of
/// the same entry at a *new* width or detail mode mints a SEPARATE slot rather
/// than replacing the old one — the cache holds one wrapped form per
/// `(entry, width, detail)` in the recent working set, all bounded by the
/// [`ENTRY_WRAP_CAPACITY`] LRU cap. (Only `entry_revision` and
/// `palette_generation` are validity tags, re-checked on a hit so a stale
/// content/theme variant recomputes in place.) This differs from
/// `render::cache`'s per-entry render cache, which keys on `(session_id,
/// entry_id)` ALONE and demotes its `context_hash` to a validity tag, so a
/// re-render at a new context overwrites that entry's single slot.
type EntryWrapKey = (u64, u64, u16, u64);

#[derive(Clone)]
struct CachedEntryWrap {
    entry_revision: u64,
    palette_generation: u64,
    /// Wrapped rows for this entry's block at the keyed width/detail.
    rows: Arc<Vec<Line<'static>>>,
}

fn entry_wrap_cache() -> &'static Mutex<LruCache<EntryWrapKey, CachedEntryWrap>> {
    static CACHE: OnceLock<Mutex<LruCache<EntryWrapKey, CachedEntryWrap>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(ENTRY_WRAP_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Incremental per-entry wrapped-row cache.
///
/// Memoises the *wrapped* rows for a single entry's block keyed by
/// `(session_id, entry_id, width, detail_hash)` and validated by
/// `(entry_revision, palette_generation)`. So re-wrapping happens only for
/// entries whose width, detail, content (`entry_revision`), or theme actually
/// changed — not the whole transcript every frame. A hit clones the stored
/// `Arc<Vec<Line>>` once.
///
/// Correctness: callers MUST fold every input that affects the *unwrapped*
/// lines that are not already covered by `entry_revision` (selected flag,
/// outcome, verbosity, shortcut) into `detail_hash`, and bump `entry_revision`
/// on every content mutation. `width` and `palette_generation` are explicit.
pub(crate) fn get_or_compute_entry_wrap(
    session_id: u64,
    entry_id: u64,
    entry_revision: u64,
    width: u16,
    detail_hash: u64,
    palette_generation: u64,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Arc<Vec<Line<'static>>> {
    let key = (session_id, entry_id, width, detail_hash);
    if let Ok(mut cache) = entry_wrap_cache().lock()
        && let Some(cached) = cache.get(&key)
        && cached.entry_revision == entry_revision
        && cached.palette_generation == palette_generation
    {
        ENTRY_WRAP_HITS.fetch_add(1, Ordering::Relaxed);
        return Arc::clone(&cached.rows);
    }
    ENTRY_WRAP_MISSES.fetch_add(1, Ordering::Relaxed);
    // Time the wrap so the render-budget HUD can report the slowest single
    // entry this frame. A miss is the only branch that actually wraps; a hit
    // costs nothing, so this never times a no-op. `Instant` is cheap relative
    // to a markdown/tree-sitter wrap, so it does not dominate the work it
    // measures.
    let started = std::time::Instant::now();
    let rows = Arc::new(compute());
    crate::metrics::record_entry_wrap(started.elapsed());
    if let Ok(mut cache) = entry_wrap_cache().lock() {
        cache.put(
            key,
            CachedEntryWrap {
                entry_revision,
                palette_generation,
                rows: Arc::clone(&rows),
            },
        );
    }
    rows
}

/// Convenience: hash anything `Hash` to a `u64` with the std default hasher,
/// matching the hashing style the overlay/row caches use for their composite
/// sub-fields (turn divider, etc.).
pub(crate) fn hash_u64(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Process-wide serialization lock for cache-behaviour tests.
///
/// The assembled-render LRU is a small (cap 24) process-wide singleton, so a
/// test that asserts on cache *presence* or *eviction order* must not run while
/// another test floods that shared cache — an unrelated insert could evict the
/// slot under test. Both the unit tests (`main_render_cache_tests.rs`) and the
/// integration tests (`lib_tests.rs`) acquire this single lock so they never
/// flood concurrently. Tests that also touch the global theme generation
/// acquire it BEFORE the theme lock, a consistent order that cannot deadlock.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
pub(crate) fn main_render_len() -> usize {
    main_render_cache().lock().map(|c| c.len()).unwrap_or(0)
}

/// Test-only: record one deep-clone of the cached rows `Vec<Line>`. Called by
/// the rows-consuming front door so a test can prove the offsets-only accessor
/// avoids that clone.
#[cfg(test)]
pub(crate) fn note_main_rows_clone() {
    MAIN_ROWS_CLONES.fetch_add(1, Ordering::Relaxed);
}

/// Test-only: snapshot of the rows-clone counter (see [`note_main_rows_clone`]).
#[cfg(test)]
pub(crate) fn main_rows_clone_count() -> u64 {
    MAIN_ROWS_CLONES.load(Ordering::Relaxed)
}

/// Test-only: is `key` currently resident in the assembled-render LRU? Used by
/// integration tests to assert hit/miss deterministically (the global hit/miss
/// counters race under parallel `cargo test`, since the cache is process-wide;
/// presence of a specific key is race-free because each test mints a private
/// `render_cache_session`). A `peek` so it does not perturb LRU order.
#[cfg(test)]
pub(crate) fn contains_main_key(key: &MainRenderKey) -> bool {
    main_render_cache()
        .lock()
        .map(|c| c.peek(key).is_some())
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn entry_wrap_len() -> usize {
    entry_wrap_cache().lock().map(|c| c.len()).unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn main_render_capacity() -> usize {
    MAIN_RENDER_CAPACITY
}

#[cfg(test)]
pub(crate) fn entry_wrap_capacity() -> usize {
    ENTRY_WRAP_CAPACITY
}

#[cfg(test)]
#[path = "main_render_cache_tests.rs"]
mod tests;
