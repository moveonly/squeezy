//! Gentle First-Run Interaction Hints (§12.1.8): a subtle, dismissible hint that
//! teaches the three least-discoverable interactions — the command-palette chord,
//! the hover reveal, and turn-to-turn jumping — and then *fades once used*, never
//! to return.
//!
//! **Pure model.** Like the other §12 leaf modules (`hover_intent`, `breadcrumbs`,
//! `toast`), this file owns only a tiny state machine: which hints the user has
//! already learned (seen), and which one — if any — should paint this frame. It
//! does NOT depend on `lib.rs`'s `TuiApp`. The caller feeds it interaction signals
//! ([`HintEngine::note_used`]) and a suppression flag (modals / overlays / search
//! input own the surface), then asks [`HintEngine::active_hint`] which single hint
//! to draw. Keyboard parity holds by construction: every hint is dismissed by the
//! `DismissFirstRunHint` keymap verb just as it is by a click on the hint strip,
//! and the interactions the hints *teach* are themselves keyboard verbs, so doing
//! the thing the hint describes ([`note_used`]) retires the hint with no mouse.
//!
//! **Shown at most once per hint, persisted as seen.** Each [`HintId`] carries a
//! one-way `seen` latch. A hint is retired the instant the user either performs
//! the interaction it teaches ([`note_used`]) or dismisses it ([`dismiss`]); once
//! latched it never paints again for the life of the session. The latch is the
//! "persisted as seen" durable state — it lives on the engine (held by `TuiApp`),
//! never recomputed from terminal cells, so a learned hint stays learned across
//! every redraw, resize, and scroll.
//!
//! **Never intrusive.** At most one hint shows at a time, chosen by a fixed
//! priority, and only after a short settle delay measured from the first frame the
//! engine was given a chance to show it — so a hint never flashes on the very
//! first paint, and a user who immediately does the thing never sees the hint at
//! all. The render path draws a single dim status-row line; it reserves no layout
//! rows and clips what it overlaps, exactly like the toast stack.
//!
//! **Zero idle cost after dismissal.** Once every hint is seen, [`is_quiet`]
//! returns `true`: [`active_hint`] short-circuits to `None`, the render path paints
//! nothing, and the redraw gate schedules no tick. A session whose user learns (or
//! dismisses) all three hints — or who disables the feature — pays a single
//! `all-seen` bool check per frame and nothing more.
//!
//! [`note_used`]: HintEngine::note_used
//! [`dismiss`]: HintEngine::dismiss
//! [`is_quiet`]: HintEngine::is_quiet
//! [`active_hint`]: HintEngine::active_hint

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The three first-run interactions the engine teaches, in *priority order* (the
/// declaration order is the tie-break the engine uses to pick the single hint to
/// show this frame). Each maps to one keyboard verb so doing the thing retires the
/// hint without a mouse.
///
/// The variant set is intentionally tiny and closed: the spec calls for hints
/// about "palette chord, hover, jump", and a longer list would risk the
/// noisy/patronizing failure mode the spec warns against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintId {
    /// "Press {chord} for the command palette." Teaches the Universal Command
    /// Palette (§12.1.1) chord, the single most discoverable entry point to every
    /// command — and the least guessable.
    PaletteChord,
    /// "Hover (or focus) a card to peek it." Teaches the Hover Preview / Hover
    /// Intent reveal (§12.1.3/§12.1.4): the quiet peek that degrades to keyboard
    /// focus when the terminal reports no mouse motion.
    Hover,
    /// "Use {chord} to jump between turns." Teaches turn-to-turn jump navigation,
    /// the fast way to move a long transcript without paging.
    Jump,
}

impl HintId {
    /// Every hint id in priority order. The index into this slice is the hint's
    /// priority (lower = higher priority); [`HintEngine::active_hint`] returns the
    /// first not-yet-seen hint in this order. Keeping the order here (not scattered
    /// across the engine) means a re-prioritization is a one-line edit.
    const ALL: [HintId; 3] = [HintId::PaletteChord, HintId::Hover, HintId::Jump];

    /// A short, stable, ASCII-only slug for the hint — the on-disk key for the
    /// persisted seen-set, so it never depends on the `Debug` spelling. Stable
    /// across releases: changing a slug silently resurrects a learned hint.
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            HintId::PaletteChord => "palette_chord",
            HintId::Hover => "hover",
            HintId::Jump => "jump",
        }
    }

    /// Parse a persisted [`slug`](HintId::slug) back into its id. Unknown slugs
    /// (an older/newer build, a hand-edited file) yield `None` and are ignored.
    fn from_slug(slug: &str) -> Option<HintId> {
        HintId::ALL.into_iter().find(|id| id.slug() == slug)
    }

    /// The hint body, with `{chord}` substituted for the live key binding the
    /// caller passes (so a rebound key shows the user's actual chord, never a
    /// stale default). `Hover` substitutes the live focus chord too.
    /// Kept terse — one short clause — so the strip never grows past a single dim
    /// line.
    pub(crate) fn message(self, chord: &str) -> String {
        match self {
            HintId::PaletteChord => format!("tip: press {chord} for the command palette"),
            HintId::Hover => format!("tip: focus a card ({chord}) to peek it"),
            HintId::Jump => format!("tip: {chord} jumps between turns"),
        }
    }
}

/// Per-hint durable state: the one-way `seen` latch plus the frame timestamp the
/// hint first became *eligible* to show (so the settle delay is measured from when
/// the engine was first asked, not from process start).
///
/// `first_eligible` is a [`Cell`] because the render path holds only `&TuiApp` (and
/// thus `&HintEngine`), yet the first frame a hint becomes the candidate must stamp
/// the settle clock. Stamping is a benign, monotonic, idempotent record of "first
/// asked at T" — it never changes a visible decision on its own — so interior
/// mutability here keeps the render path read-`&self` exactly like the frame-local
/// hit-test registry does.
#[derive(Debug, Clone, Default)]
struct HintState {
    /// Latched `true` the first time the user performs or dismisses the hint. A
    /// seen hint never paints again. One-way: nothing clears it for the session.
    seen: bool,
    /// When this hint first became the candidate to show (the first frame
    /// [`HintEngine::active_hint`] reached it while not seen). `None` until then.
    /// The settle delay is `now - first_eligible`, so a hint never flashes on the
    /// frame it becomes eligible.
    first_eligible: Cell<Option<Instant>>,
}

/// How long a hint must remain the top candidate before it actually paints. Short
/// enough to feel responsive, long enough that a user who immediately does the
/// thing (or who is mid-task) never sees a flash. Distinct from any animation
/// timing — this is purely "don't show instantly".
const HINT_SETTLE: Duration = Duration::from_millis(700);

/// The First-Run Interaction Hints engine (§12.1.8): the durable seen-set for the
/// three taught interactions plus the rule that picks the single hint to paint
/// this frame. Held by `TuiApp` directly (not behind an `Option`) because the
/// resting state — feature enabled, nothing seen yet — is itself cheap, and the
/// all-seen state collapses to a single bool check via [`is_quiet`].
#[derive(Debug, Clone)]
pub(crate) struct HintEngine {
    /// Whether the whole feature is enabled. Off ⇒ no hint ever shows and
    /// [`is_quiet`] is `true`, so a user who finds hints patronizing can silence
    /// them entirely with one verb.
    enabled: bool,
    /// Per-hint state, indexed in lockstep with [`HintId::ALL`].
    states: [HintState; HintId::ALL.len()],
    /// Which hint, if any, is currently *painted on screen*. A [`Cell`] for the same
    /// reason as `first_eligible`: the render-time [`active_hint`](HintEngine::
    /// active_hint) query records that a line is live (so [`dismiss`](HintEngine::
    /// dismiss) / [`note_used`](HintEngine::note_used) know whether to ask for an
    /// erase repaint) while holding only `&self`.
    showing: Cell<Option<HintId>>,
    /// File the seen-set is mirrored to so a learned hint stays learned across
    /// sessions. `None` keeps the engine purely in-memory (tests, and the
    /// disabled feature). Writes are best-effort: an I/O failure is logged and
    /// the in-memory latch keeps working.
    persist_path: Option<PathBuf>,
}

impl Default for HintEngine {
    fn default() -> Self {
        // In-memory only: tests and any caller that wants the historical
        // behaviour without touching disk. Enabled by default — the hints are
        // restrained and fade the instant the user learns the interaction.
        Self {
            enabled: true,
            states: Default::default(),
            showing: Cell::new(None),
            persist_path: None,
        }
    }
}

impl HintEngine {
    /// In-memory engine with the feature `enabled` flag set explicitly and no
    /// disk mirror. Used when the seen-set should not be persisted (the feature
    /// is disabled, or no state path is available).
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            states: Default::default(),
            showing: Cell::new(None),
            persist_path: None,
        }
    }

    /// Disk-backed engine: pre-seeds the seen-set from `path` (so a hint learned
    /// in a prior session stays learned) and mirrors every newly-latched hint
    /// back to it. When `enabled` is `false` the feature is silent regardless of
    /// what is on disk, but the path is still honored so a later re-enable keeps
    /// the persisted seen-set.
    pub(crate) fn with_persistence(enabled: bool, path: PathBuf) -> Self {
        let mut engine = Self {
            enabled,
            states: Default::default(),
            showing: Cell::new(None),
            persist_path: Some(path),
        };
        engine.load_seen_from_disk();
        engine
    }

    /// Mark every hint whose slug appears in `slugs` as already seen. Unknown
    /// slugs are ignored. Used by [`load_seen_from_disk`](HintEngine::
    /// load_seen_from_disk) and the seeding unit tests.
    fn seed_seen(&mut self, slugs: impl IntoIterator<Item = String>) {
        for slug in slugs {
            if let Some(id) = HintId::from_slug(slug.trim()) {
                self.states[Self::index(id)].seen = true;
            }
        }
    }

    /// The slugs of every hint currently latched seen, in priority order — the
    /// on-disk representation of the seen-set.
    fn seen_slugs(&self) -> Vec<&'static str> {
        HintId::ALL
            .into_iter()
            .filter(|&id| self.states[Self::index(id)].seen)
            .map(HintId::slug)
            .collect()
    }

    fn load_seen_from_disk(&mut self) {
        let Some(path) = self.persist_path.clone() else {
            return;
        };
        match read_slugs(&path) {
            Ok(slugs) => self.seed_seen(slugs),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    target: "squeezy_tui::first_run_hints",
                    error = %err,
                    path = %path.display(),
                    "failed to load first-run hint state",
                );
            }
        }
    }

    /// Best-effort write of the current seen-set to disk. A failure is logged
    /// and otherwise ignored so a borked state file never breaks the session.
    fn persist_seen(&self) {
        let Some(path) = self.persist_path.as_deref() else {
            return;
        };
        if let Err(err) = write_slugs(path, &self.seen_slugs()) {
            tracing::warn!(
                target: "squeezy_tui::first_run_hints",
                error = %err,
                path = %path.display(),
                "failed to persist first-run hint state",
            );
        }
    }

    /// The stable array index for a hint id (its position in [`HintId::ALL`],
    /// which is also its priority). Total over the closed variant set.
    fn index(id: HintId) -> usize {
        HintId::ALL
            .iter()
            .position(|&h| h == id)
            .expect("HintId::ALL is exhaustive over HintId")
    }

    /// Whether the feature is enabled. `cfg(test)`-only: production reads the flag
    /// through `toggle`'s return and the internal checks in `active_hint`.
    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Toggle the whole feature on/off, returning the new state. Turning it off
    /// also clears any hint currently being shown so a stale line can't linger
    /// painted; the seen-set is preserved (a re-enable does not resurrect learned
    /// hints). `cfg(test)`-only today: production keeps the feature enabled and
    /// reaches the quiet resting state through per-hint dismissals; the disable path
    /// is exercised by the "disabled config" unit test the spec calls for.
    #[cfg(test)]
    pub(crate) fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.showing.set(None);
        }
        self.enabled
    }

    /// Whether a given hint has been seen (used or dismissed). `cfg(test)`-only:
    /// production decides what to paint through [`active_hint`], never by reading
    /// the latch directly.
    #[cfg(test)]
    pub(crate) fn is_seen(&self, id: HintId) -> bool {
        self.states[Self::index(id)].seen
    }

    /// True once every hint is seen (or the feature is disabled): the render path
    /// paints nothing and the redraw gate schedules no tick. The cheap idle check
    /// the spec's "zero idle cost after dismissal" requires.
    pub(crate) fn is_quiet(&self) -> bool {
        !self.enabled || self.states.iter().all(|s| s.seen)
    }

    /// Record that the user performed the interaction `id` teaches — so the hint
    /// *fades once used*. Latches the hint seen and, if it was the one being shown,
    /// stops showing it. Idempotent and cheap: a no-op once already seen. Returns
    /// `true` only when this call actually retired a still-pending hint (so the
    /// caller can request one final redraw to erase a painted line).
    pub(crate) fn note_used(&mut self, id: HintId) -> bool {
        let idx = Self::index(id);
        let was_visible = self.showing.get() == Some(id);
        let newly_seen = !self.states[idx].seen;
        self.states[idx].seen = true;
        if was_visible {
            self.showing.set(None);
        }
        if newly_seen {
            self.persist_seen();
        }
        // A redraw is only needed if a line was actually painted for this hint.
        was_visible && newly_seen
    }

    /// Dismiss the hint currently being shown — the keyboard `DismissFirstRunHint`
    /// verb and the mouse click on the hint strip both call this. Latches that hint
    /// seen so it never returns. Returns the dismissed [`HintId`] (so the caller
    /// can name it in a status line) or `None` when nothing was showing.
    pub(crate) fn dismiss(&mut self) -> Option<HintId> {
        let id = self.showing.take()?;
        let idx = Self::index(id);
        let newly_seen = !self.states[idx].seen;
        self.states[idx].seen = true;
        if newly_seen {
            self.persist_seen();
        }
        Some(id)
    }

    /// The hint currently painted on screen, if any. `cfg(test)`-only: production
    /// reads the paint decision through [`active_hint`], never the raw flag.
    #[cfg(test)]
    pub(crate) fn showing(&self) -> Option<HintId> {
        self.showing.get()
    }

    /// Force the highest-priority not-yet-seen hint to be *already settled* (stamp
    /// its eligibility window well in the past), so a subsequent [`active_hint`] at
    /// "now" paints it without a real-time wait. `cfg(test)`-only: it exists purely
    /// so the integration tests can drive a settled hint through the real `render()`
    /// deterministically, never depending on wall-clock sleeps.
    #[cfg(test)]
    pub(crate) fn force_settle_for_test(&self) {
        if let Some(&candidate) = HintId::ALL
            .iter()
            .find(|&&id| !self.states[Self::index(id)].seen)
        {
            let idx = Self::index(candidate);
            // Stamp the eligibility clock far enough in the past that the candidate
            // reads as long-settled at any plausible test "now". An hour dwarfs
            // HINT_SETTLE, but `Instant - Duration` panics on platforms whose
            // monotonic clock is younger than the amount we subtract (Windows CI
            // runners boot with a tiny QPC value), so subtract via `checked_sub`
            // and fall back to the largest clock-safe offset that still clears the
            // settle window with margin.
            let now = Instant::now();
            let settled = now
                .checked_sub(Duration::from_secs(3600))
                .or_else(|| now.checked_sub(HINT_SETTLE * 4))
                .unwrap_or(now);
            self.states[idx].first_eligible.set(Some(settled));
        }
    }

    /// The single hint to paint this frame, or `None` for nothing. Encodes the
    /// spec's rules:
    ///
    /// - feature off, or every hint seen ⇒ `None` (the quiet resting state),
    /// - while `suppressed` (a modal / overlay / search input owns the surface) ⇒
    ///   `None`, and the settle clock does not advance, so a hint never burns its
    ///   one showing behind a modal,
    /// - otherwise the highest-priority not-yet-seen hint, but only after it has
    ///   been the top candidate for at least [`HINT_SETTLE`] — so it never flashes
    ///   instantly and a user who immediately acts never sees it.
    ///
    /// Takes `&self` (not `&mut`) because the only mutation is stamping the settle
    /// clock and recording which hint is on screen — both through [`Cell`]s — so the
    /// render path can call it while holding `&TuiApp`, exactly like the frame-local
    /// hit-test registry. The stamp is monotonic and idempotent; it never flips a
    /// visible decision on its own.
    pub(crate) fn active_hint(&self, now: Instant, suppressed: bool) -> Option<HintId> {
        if self.is_quiet() || suppressed {
            // Suppression does not clear `showing`: a hint hidden behind a
            // transient modal reappears (still un-seen) when the modal closes,
            // rather than silently burning its single showing.
            return None;
        }
        // The first not-yet-seen hint in priority order is the candidate.
        let &candidate = HintId::ALL
            .iter()
            .find(|&&id| !self.states[Self::index(id)].seen)?;
        let idx = Self::index(candidate);
        // Stamp (or read) the settle clock for this candidate. We only ever stamp
        // the *current* candidate; a higher-priority hint that gets retired hands
        // off to the next, which then starts its own settle window fresh.
        let first = match self.states[idx].first_eligible.get() {
            Some(first) => first,
            None => {
                self.states[idx].first_eligible.set(Some(now));
                now
            }
        };
        if now.duration_since(first) < HINT_SETTLE {
            // Still settling: nothing paints yet, and we are not "showing" it.
            if self.showing.get() == Some(candidate) {
                self.showing.set(None);
            }
            return None;
        }
        self.showing.set(Some(candidate));
        Some(candidate)
    }

    /// Whether a hint's settle window is *in flight* — the candidate has been
    /// stamped (a render has happened) and is still inside [`HINT_SETTLE`], or is
    /// already showing — so the caller schedules exactly one follow-up redraw to
    /// paint it once the settle elapses, then goes quiet. Returns `false` the moment
    /// the engine is quiet or suppressed, AND — critically — when the candidate has
    /// not yet been stamped (`first_eligible == None`): the *first* stamp happens
    /// inside [`active_hint`] on a render the caller is already doing (when focused),
    /// so an idle, unfocused, never-rendered session schedules NO tick of its own.
    /// This preserves the zero-idle-cost invariant: the hint engine never spins up
    /// the animation loop from cold; it only keeps an already-running settle alive.
    /// Read-only: never advances the settle clock.
    pub(crate) fn reveal_pending(&self, now: Instant, suppressed: bool) -> bool {
        if self.is_quiet() || suppressed {
            return false;
        }
        let Some(&candidate) = HintId::ALL
            .iter()
            .find(|&&id| !self.states[Self::index(id)].seen)
        else {
            return false;
        };
        match self.states[Self::index(candidate)].first_eligible.get() {
            // Not yet stamped: do NOT schedule a tick from cold. The first render
            // (which a focused session is already doing) stamps the clock; only then
            // does this start keeping the loop alive through the settle window.
            None => false,
            // Stamped and still inside the settle window, or already showing: a paint
            // is imminent, so the caller should schedule the wake-up.
            Some(first) => {
                now.duration_since(first) < HINT_SETTLE || self.showing.get() == Some(candidate)
            }
        }
    }
}

/// Read the persisted seen-set: one slug per line, blank lines ignored. A
/// missing file surfaces as `io::ErrorKind::NotFound` so the caller can treat a
/// first run as "nothing seen yet".
fn read_slugs(path: &Path) -> io::Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Rewrite the seen-set file with `slugs`, one per line, creating the parent
/// directory if needed. A whole rewrite (not append) keeps the tiny file
/// canonical and free of stale/duplicate entries.
fn write_slugs(path: &Path, slugs: &[&str]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for slug in slugs {
        body.push_str(slug);
        body.push('\n');
    }
    fs::write(path, body)
}

#[cfg(test)]
#[path = "first_run_hints_tests.rs"]
mod tests;
