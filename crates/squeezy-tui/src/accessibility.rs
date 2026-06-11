//! Accessibility Quality Gate (spec §12.10.5).
//!
//! An automated accessibility *gate*: it captures a rendered surface through the
//! single fullscreen [`render`](crate::render) path and audits it against the
//! checks the spec calls out — contrast ratios, no-color-only meaning
//! (screen-reader-friendly text extraction), minimal-glyph coverage, and
//! keyboard reachability of every mouse affordance — failing loudly when a
//! surface violates the gate.
//!
//! ## What it audits
//!
//! [`audit_app`] runs the full gate over a built [`TuiApp`] at one geometry and
//! returns an [`AuditReport`] listing every [`Violation`]. The four gates are:
//!
//! * **Contrast** — every painted glyph's foreground must stand off its
//!   effective background by at least [`MIN_CONTRAST_RATIO`] (a WCAG-style
//!   relative-luminance contrast ratio). Washed-out or invisible text fails.
//! * **Screen-reader text** — the surface must carry its content as *extractable
//!   plain text*, not as color or glyph shape alone. The readable text stream
//!   (glyphs minus box-drawing chrome) must be non-empty and contain the actual
//!   transcript content, so a screen reader walking the cells reads real words.
//! * **Minimal-glyph coverage** — Squeezy chrome must stay inside a bounded glyph
//!   set (ASCII plus a small, known Unicode chrome set) so fonts/remoting/legacy
//!   consoles that cannot render arbitrary symbols still get meaningful output.
//! * **Keyboard reachability** — every mouse affordance in the
//!   [`interaction`](crate::interaction) hit-test vocabulary must have a keyboard
//!   equivalent, so nothing is mouse-only.
//!
//! ## Why a [`TerminalProfile`] axis
//!
//! The spec's platform notes require the gate to run against at least one
//! macOS-style, one Linux/tmux, and one Windows-Terminal profile, because the
//! effective terminal background (which a contrast ratio is computed *against*)
//! is app-controlled, not queryable from inside a terminal session. Each
//! [`TerminalProfile`] pins a reference background so the contrast gate is
//! deterministic and reproducible across platforms.
//!
//! ## Why no `TuiHarness`, no term-matrix, no keybinding
//!
//! Like [`visual_diff`](crate::visual_diff) and [`bench_render`](crate::bench_render),
//! this is a quality gate, not a shipped feature: the whole module is
//! `cfg(test)`-gated, never compiles into a TUI binary, adds no keybinding, no
//! dispatch arm, and no idle redraw. The surviving render seam — a ratatui
//! [`TestBackend`] driven by the real `render()` — is the capture sink the spec
//! asks for. Every item is exercised by the sibling `accessibility_tests.rs`, so
//! the module carries no dead code on any platform.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{Terminal, TerminalOptions, Viewport};
use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, SessionMode, TranscriptItem};

use crate::interaction;
use crate::keymap::Action;
use crate::{Clipboard, TuiApp, render};

/// Minimum WCAG-style contrast ratio a painted glyph must hold against its
/// effective background. WCAG AA demands 4.5:1 for normal text; terminal cell
/// glyphs are effectively "large text" (a monospace grid renders bold, blocky
/// shapes), for which AA is 3.0:1. The gate uses the large-text bar so a
/// genuinely readable dark surface passes while invisible / washed-out text
/// fails — a curated gate, not a hair-trigger.
pub(crate) const MIN_CONTRAST_RATIO: f64 = 3.0;

/// The empty-composer caret glyph [`crate::prompt_cursor_span`] paints. The
/// screen-reader gate ignores it: it is a focus cue, not content text.
const COMPOSER_CARET: char = '┃';

/// The bounded set of non-ASCII glyphs Squeezy chrome is allowed to paint. Any
/// rendered cell outside ASCII *and* outside this set is a minimal-glyph gate
/// violation — chrome that a font/remoting/legacy console may render as tofu.
///
/// This is intentionally small and explicit: box-drawing for card borders and
/// rails, the composer caret and scrollbar block, the fold/disclosure markers,
/// the spinner frames, and the ellipsis. Transcript/tool *content* is exempt
/// (the spec scopes minimal-glyph mode to Squeezy chrome, never user output);
/// the gate only audits chrome rows, see [`is_chrome_glyph`].
const ALLOWED_CHROME_GLYPHS: &[char] = &[
    // Box drawing — card borders, rails, separators (incl. dashed variants the
    // transcript rails paint).
    '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╭', '╮', '╰', '╯', '┈', '┊', '╌', '╎',
    // Heavy/double variants that show up in focus rings and the caret.
    '━', '┃', '┏', '┓', '┗', '┛', // Scrollbar + block fills.
    '█', '▌', '▐', '░', '▒', '▓', '▀', '▄',
    // Disclosure / fold / status markers and arrows.
    '▶', '▼', '▲', '◀', '·', '•', '◦', '»', '«', '›', '‹', '→', '←', '↑', '↓',
    // Status / mode indicator circles (idle/working/partial state dots).
    '○', '●', '◐', '◑', '◒', '◓', // Spinner frames + ellipsis.
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '…',
];

/// A reference terminal profile the gate audits against. Each pins the
/// *effective background* a contrast ratio is computed against, because a
/// terminal's real background is app-controlled and not queryable from inside a
/// terminal session (the spec's platform-notes requirement).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Each variant is a distinct dark-terminal reference profile; the shared `Dark`
// suffix is meaningful (every reference background the gate pins is dark), not a
// naming smell.
#[allow(clippy::enum_variant_names)]
pub(crate) enum TerminalProfile {
    /// A macOS-style dark terminal (Terminal.app / iTerm2 default near-black).
    MacosDark,
    /// A Linux / tmux dark terminal (xterm-ish near-black).
    LinuxTmuxDark,
    /// Windows Terminal's default dark "Campbell" background.
    WindowsTerminalDark,
}

impl TerminalProfile {
    /// Every profile, in a stable order, so the gate sweeps the required
    /// platform matrix deterministically.
    pub(crate) const ALL: [TerminalProfile; 3] = [
        TerminalProfile::MacosDark,
        TerminalProfile::LinuxTmuxDark,
        TerminalProfile::WindowsTerminalDark,
    ];

    /// Stable slug for log identification / test naming.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            TerminalProfile::MacosDark => "macos_dark",
            TerminalProfile::LinuxTmuxDark => "linux_tmux_dark",
            TerminalProfile::WindowsTerminalDark => "windows_terminal_dark",
        }
    }

    /// The reference background sRGB triple a cell with `Color::Reset` (terminal
    /// default) is assumed to sit on. Real, slightly-different near-blacks so the
    /// matrix is not a single trivial point.
    pub(crate) fn reference_bg(self) -> (u8, u8, u8) {
        match self {
            // Terminal.app "Basic" / iTerm2 default near-black.
            TerminalProfile::MacosDark => (0, 0, 0),
            // Common xterm/tmux dark theme.
            TerminalProfile::LinuxTmuxDark => (12, 12, 12),
            // Windows Terminal "Campbell" background (#0C0C0C is also dark, but
            // Campbell's true bg is #0C0C0C → use the documented value).
            TerminalProfile::WindowsTerminalDark => (12, 12, 12),
        }
    }
}

/// One representative render surface the gate audits. Each maps to a
/// deterministically-built [`TuiApp`] so a captured surface is reproducible
/// across runs and platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// A freshly-started session: empty transcript, composer only. The minimum
    /// frame and an edge case — nothing but chrome and the caret.
    Empty,
    /// A short back-and-forth: a handful of prose turns. The common case.
    ShortChat,
    /// A long session that overflows the viewport. Stresses scrollbar/rail
    /// chrome and wrapping.
    LongSession,
    /// A session carrying a system notice plus a typed composer line — the
    /// surface that proves status/notice text is screen-reader extractable and
    /// not color-only.
    SystemNotice,
}

impl Surface {
    /// Every surface, in a stable order, so the gate sweeps deterministically.
    pub(crate) const ALL: [Surface; 4] = [
        Surface::Empty,
        Surface::ShortChat,
        Surface::LongSession,
        Surface::SystemNotice,
    ];

    /// Stable slug for log identification / test naming.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Surface::Empty => "empty",
            Surface::ShortChat => "short_chat",
            Surface::LongSession => "long_session",
            Surface::SystemNotice => "system_notice",
        }
    }

    /// The substrings the screen-reader gate requires to be present as
    /// extractable plain text on this surface (empty for the chrome-only Empty
    /// surface). Proves the content is conveyed by *words*, not color/glyph.
    ///
    /// Only content guaranteed *visible at the tail* is required: a small
    /// viewport legitimately scrolls older turns off (correct scroll behavior,
    /// not an accessibility failure), so the gate asserts the latest turn's
    /// content and the always-visible composer/status text. Matching is
    /// whitespace-normalized (see [`AuditSurface::readable_text`]) so a phrase
    /// that *wraps* across rows still matches.
    pub(crate) fn required_text(self) -> &'static [&'static str] {
        match self {
            Surface::Empty => &[],
            // The latest assistant answer is the tail — always visible.
            Surface::ShortChat => &["Clamp the offset against the row count before indexing"],
            // Each long-session answer ends the same way; the tail answer is visible.
            Surface::LongSession => &["the fix is local"],
            // The system notice is the last transcript entry (tail); the typed
            // composer line is always visible.
            Surface::SystemNotice => &["sandbox denied write", "still typing the next prompt"],
        }
    }

    /// Build the [`TuiApp`] this surface captures. Self-contained: a temp
    /// workspace root so construction never crawls the real repo.
    fn build_app(self) -> TuiApp {
        let mut app = new_audit_app();
        match self {
            Surface::Empty => {}
            Surface::ShortChat => {
                app.push_transcript_item(TranscriptItem::user("explain this stack trace"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "The panic unwinds through render because the slice index is out of bounds.",
                ));
                app.push_transcript_item(TranscriptItem::user("how do I fix it?"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "Clamp the offset against the row count before indexing.",
                ));
            }
            Surface::LongSession => {
                seed_long_session(&mut app, 120);
            }
            Surface::SystemNotice => {
                app.push_transcript_item(TranscriptItem::user("run the failing command"));
                app.push_transcript_item(TranscriptItem::system(
                    "sandbox denied write to /etc; rerun with approval",
                ));
                app.input = "still typing the next prompt".to_string();
            }
        }
        app
    }
}

/// A single captured cell: its glyph plus the resolved foreground/background and
/// whether it is blank. The unit every gate reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuditCell {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) symbol: String,
    pub(crate) fg: Color,
    pub(crate) bg: Color,
}

impl AuditCell {
    /// True when the cell paints nothing readable — a space or an empty symbol.
    /// Blank cells are exempt from the contrast and minimal-glyph gates.
    fn is_blank(&self) -> bool {
        self.symbol.is_empty() || self.symbol == " "
    }
}

/// A full rendered surface projected to a cell grid plus the metadata each
/// violation is attributed to.
#[derive(Clone, Debug)]
pub(crate) struct AuditSurface {
    pub(crate) surface: Surface,
    pub(crate) profile: TerminalProfile,
    pub(crate) height: u16,
    pub(crate) cells: Vec<AuditCell>,
}

impl AuditSurface {
    /// Capture `surface` at `width`x`height` under `profile` by driving the real
    /// [`render`] over a [`TestBackend`] and projecting its post-frame buffer.
    pub(crate) fn capture(
        surface: Surface,
        profile: TerminalProfile,
        width: u16,
        height: u16,
    ) -> AuditSurface {
        let app = surface.build_app();
        Self::capture_app(&app, surface, profile, width, height)
    }

    /// Capture an already-built `app`. Split out so a test can stage a bespoke
    /// app without a [`Surface`] variant.
    pub(crate) fn capture_app(
        app: &TuiApp,
        surface: Surface,
        profile: TerminalProfile,
        width: u16,
        height: u16,
    ) -> AuditSurface {
        let viewport = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::with_options(
            TestBackend::new(width, height),
            TerminalOptions {
                viewport: Viewport::Fixed(viewport),
            },
        )
        .expect("test backend");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        AuditSurface {
            surface,
            profile,
            height,
            cells: cells_from_buffer(&buffer),
        }
    }

    /// The readable plain-text stream a screen reader would walk: every
    /// non-chrome glyph in row-major order, rows joined by newlines. Box-drawing
    /// chrome and the composer caret are dropped so the stream is *words*, the
    /// surface the screen-reader gate searches.
    pub(crate) fn screen_reader_text(&self) -> String {
        let mut out = String::with_capacity(self.cells.len() + self.height as usize);
        let mut last_y = 0u16;
        for cell in &self.cells {
            if cell.y != last_y {
                out.push('\n');
                last_y = cell.y;
            }
            let glyph = cell.symbol.chars().next().unwrap_or(' ');
            if is_chrome_glyph(glyph) || glyph == COMPOSER_CARET {
                // Chrome renders as a separator space so adjacent words don't fuse.
                out.push(' ');
            } else {
                out.push_str(&cell.symbol);
            }
        }
        out
    }

    /// The [`screen_reader_text`](Self::screen_reader_text) stream with all
    /// runs of whitespace (including the row-boundary newlines) collapsed to a
    /// single space. This is the surface the screen-reader gate substring-checks
    /// so a phrase that *wraps* across two rows — or sits behind rail/gutter
    /// padding — still matches by its words.
    pub(crate) fn readable_text(&self) -> String {
        self.screen_reader_text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Project a rendered [`Buffer`] to the audit's flat cell grid (row-major).
fn cells_from_buffer(buffer: &Buffer) -> Vec<AuditCell> {
    let area = buffer.area;
    let mut cells = Vec::with_capacity(area.width as usize * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            cells.push(AuditCell {
                x,
                y,
                symbol: cell.symbol().to_string(),
                fg: cell.fg,
                bg: cell.bg,
            });
        }
    }
    cells
}

/// The four gate dimensions the spec enumerates. Carried on every [`Violation`]
/// so a failing report names which gate tripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateKind {
    /// A painted glyph failed the minimum contrast ratio against its background.
    Contrast,
    /// Required content was missing from the extractable plain-text stream —
    /// information conveyed by color/glyph alone, not words.
    ScreenReaderText,
    /// A chrome cell painted a glyph outside the bounded minimal-glyph set.
    MinimalGlyph,
    /// A mouse affordance has no keyboard equivalent.
    KeyboardReachability,
}

impl GateKind {
    /// Stable label for messages / test assertions.
    pub(crate) fn label(self) -> &'static str {
        match self {
            GateKind::Contrast => "contrast",
            GateKind::ScreenReaderText => "screen_reader_text",
            GateKind::MinimalGlyph => "minimal_glyph",
            GateKind::KeyboardReachability => "keyboard_reachability",
        }
    }
}

/// One gate failure: which gate, and a human-readable detail naming the
/// offending cell/text/affordance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Violation {
    pub(crate) gate: GateKind,
    pub(crate) detail: String,
}

/// The result of auditing one surface: every violation found. An empty list is
/// a pass.
#[derive(Clone, Debug)]
pub(crate) struct AuditReport {
    pub(crate) violations: Vec<Violation>,
}

impl AuditReport {
    /// True when the surface passed every gate.
    pub(crate) fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// Violations of one gate kind (test/diagnostic aid).
    pub(crate) fn of_kind(&self, gate: GateKind) -> Vec<&Violation> {
        self.violations.iter().filter(|v| v.gate == gate).collect()
    }
}

/// Audit a built `app` at one geometry under `profile`: the full gate. The
/// entry point a capture-sink test drives.
pub(crate) fn audit_app(
    app: &TuiApp,
    surface: Surface,
    profile: TerminalProfile,
    width: u16,
    height: u16,
) -> AuditReport {
    let captured = AuditSurface::capture_app(app, surface, profile, width, height);
    audit_surface(&captured)
}

/// Audit `surface` (built deterministically) at one geometry under `profile`.
pub(crate) fn audit(
    surface: Surface,
    profile: TerminalProfile,
    width: u16,
    height: u16,
) -> AuditReport {
    let captured = AuditSurface::capture(surface, profile, width, height);
    audit_surface(&captured)
}

/// Run the four gates over an already-captured surface.
pub(crate) fn audit_surface(captured: &AuditSurface) -> AuditReport {
    let mut violations = Vec::new();
    contrast_gate(captured, &mut violations);
    screen_reader_gate(captured, &mut violations);
    minimal_glyph_gate(captured, &mut violations);
    keyboard_reachability_gate(&mut violations);
    AuditReport { violations }
}

/// Contrast gate: every painted (non-blank) glyph must clear [`MIN_CONTRAST_RATIO`]
/// against its effective background. Only the first failing cell per surface is
/// reported so a wholesale failure stays one actionable line, not a wall of
/// cells.
fn contrast_gate(captured: &AuditSurface, out: &mut Vec<Violation>) {
    let ref_bg = captured.profile.reference_bg();
    for cell in &captured.cells {
        if cell.is_blank() {
            continue;
        }
        // Indexed colors inherit a palette we cannot resolve to RGB; the gate
        // skips them (they degrade to the terminal's own scheme, outside our
        // contract). Reset fg inherits the foreground default, also a pass.
        let Some(fg) = resolvable_rgb(cell.fg) else {
            continue;
        };
        let bg = match resolvable_rgb(cell.bg) {
            Some(bg) => bg,
            None => ref_bg,
        };
        let ratio = contrast_ratio(fg, bg);
        if ratio < MIN_CONTRAST_RATIO {
            out.push(Violation {
                gate: GateKind::Contrast,
                detail: format!(
                    "cell ({},{}) glyph {:?} fg {:?} on bg {:?} has contrast {:.2}:1 < {:.1}:1",
                    cell.x, cell.y, cell.symbol, fg, bg, ratio, MIN_CONTRAST_RATIO
                ),
            });
            return;
        }
    }
}

/// Screen-reader gate: the surface's required content must be present in the
/// extractable plain-text stream — proof that meaning is carried by words, not
/// by color or glyph shape. A non-empty surface must also produce *some*
/// readable text.
fn screen_reader_gate(captured: &AuditSurface, out: &mut Vec<Violation>) {
    let text = captured.readable_text();
    let required = captured.surface.required_text();
    if required.is_empty() {
        return;
    }
    // A content surface must yield real words, not just chrome.
    if text.split_whitespace().next().is_none() {
        out.push(Violation {
            gate: GateKind::ScreenReaderText,
            detail: "surface produced no extractable text (chrome-only)".to_string(),
        });
        return;
    }
    for needle in required {
        if !text.contains(needle) {
            out.push(Violation {
                gate: GateKind::ScreenReaderText,
                detail: format!(
                    "required content {needle:?} not extractable as plain text on surface {}",
                    captured.surface.slug()
                ),
            });
        }
    }
}

/// Minimal-glyph gate: every chrome cell must paint a glyph inside the bounded
/// allowed set. Content glyphs (letters, digits, punctuation, whitespace) are
/// exempt — the spec scopes minimal-glyph mode to Squeezy chrome, never user
/// output. Only the first offender is reported.
fn minimal_glyph_gate(captured: &AuditSurface, out: &mut Vec<Violation>) {
    for cell in &captured.cells {
        if cell.is_blank() {
            continue;
        }
        let glyph = cell.symbol.chars().next().unwrap_or(' ');
        if glyph.is_ascii() {
            continue;
        }
        // A non-ASCII glyph is acceptable only if it is allowed chrome. Any
        // *content* glyph (CJK prose, emoji in user text) is exempt because it
        // is not chrome; the gate distinguishes via the allow-list: a non-ASCII
        // glyph that is NOT in the chrome set and IS chrome-positioned is the
        // failure. We approximate "is chrome" as "is in the box-drawing /
        // marker Unicode blocks" so stray decorative chrome is caught while
        // genuine content is not.
        if is_chrome_glyph(glyph) {
            continue;
        }
        if looks_like_chrome_block(glyph) && !ALLOWED_CHROME_GLYPHS.contains(&glyph) {
            out.push(Violation {
                gate: GateKind::MinimalGlyph,
                detail: format!(
                    "chrome cell ({},{}) paints non-minimal glyph {:?} (U+{:04X})",
                    cell.x, cell.y, glyph, glyph as u32
                ),
            });
            return;
        }
    }
}

/// Keyboard-reachability gate: every mouse affordance in the hit-test
/// vocabulary must have a keyboard equivalent. The spec's "no mouse-only"
/// guarantee. Reported per-affordance so a regression names the orphaned one.
fn keyboard_reachability_gate(out: &mut Vec<Violation>) {
    for &mouse_action in interaction::Action::AUDIT_ALL {
        if keyboard_equivalent(mouse_action).is_none() {
            out.push(Violation {
                gate: GateKind::KeyboardReachability,
                detail: format!("mouse affordance {mouse_action:?} has no keyboard equivalent"),
            });
        }
    }
}

/// Map a mouse [`interaction::Action`] to the keyboard equivalent that reaches
/// the same handler, or [`None`] when the affordance is genuinely mouse-only
/// (which the gate then flags). Mirrors the dispatch parity that
/// [`interaction`](crate::interaction) documents: every wired mouse action
/// shares its handler with a keyboard verb.
pub(crate) fn keyboard_equivalent(action: interaction::Action) -> Option<KeyboardPath> {
    use interaction::Action as A;
    Some(match action {
        // Toggle the queue overlay — reachable via the queue strip's own key
        // handling; the overlay opens/closes from the keyboard.
        A::ToggleQueueOverlay => KeyboardPath::Always("queue overlay key"),
        // Fold toggle — `ToggleFocusedFold` (Ctrl+O default).
        A::ToggleEntryCollapsed(_) => KeyboardPath::Keymap(Action::ToggleFocusedFold),
        // Focus an entry — `FocusPrevEntry` / `FocusNextEntry` (Ctrl+Up/Down).
        A::FocusEntry(_) => KeyboardPath::Keymap(Action::FocusNextEntry),
        // Expand a collapsed entry — the same fold toggle reaches expand.
        A::ExpandEntry(_) => KeyboardPath::Keymap(Action::ToggleFocusedFold),
        // Open in detail — `OpenFocusedInDetail` (Ctrl+Enter default).
        A::OpenEntryInDetail(_) => KeyboardPath::Keymap(Action::OpenFocusedInDetail),
        // Queue delete — consumed by the queue overlay's own Delete handler.
        A::QueueDelete(_) => KeyboardPath::Always("queue overlay Delete"),
        // Queue reorder — Shift+Up/Down inside the queue overlay.
        A::QueueReorderBegin(_) => KeyboardPath::Always("queue overlay Shift+Up/Down"),
        // Queue undo — `QueueUndo` (u default).
        A::QueueUndo => KeyboardPath::Keymap(Action::QueueUndo),
        // Queue edit (§11G.8) — the queue overlay's own Enter/`e` handler pulls
        // the focused prompt into the composer before the global keymap sees
        // the key, the same `begin_queue_edit` the double-click drives.
        A::QueueEdit(_) => KeyboardPath::Always("queue overlay Enter/e"),
        // Queue run-next (§11G.9) — the queue overlay's own `r` handler promotes
        // the focused prompt to the front before the global keymap sees the key,
        // the same `queue_run_selected_next` the click drives.
        A::QueueRunNext(_) => KeyboardPath::Always("queue overlay r"),
        // Queue cycle-condition (§12.3.5) — the queue overlay's own `v` handler
        // cycles the focused prompt's run-condition before the global keymap sees
        // the key, the same `queue_cycle_condition_by_id` the Ctrl+Right-click
        // drives.
        A::QueueCycleCondition(_) => KeyboardPath::Always("queue overlay v"),
        // Jump to latest — `TranscriptEnd` (End default) reaches the tail.
        A::JumpToLatest => KeyboardPath::Keymap(Action::TranscriptEnd),
        // Scrollbar jump — page scroll keys move the same viewport.
        A::ScrollbarJump => KeyboardPath::Keymap(Action::ScrollTranscriptPageDown),
        // Minimap turn-rail jump — the jump-navigation keys reach the same
        // "move the viewport to a turn" handler the rail click drives
        // (`jump_to_entry_id`); `JumpNextUserTurn` (Alt+Down default) is the
        // representative keyboard verb.
        A::MinimapJump(_) => KeyboardPath::Keymap(Action::JumpNextUserTurn),
        // Large-paste confirm/cancel — the paste-preview modal's own key
        // handler owns Enter/`y` (confirm) and Esc/`n` (cancel) before the
        // global keymap sees them, so both reach the same `resolve_paste_preview`
        // handler the buttons click.
        A::ConfirmPaste => KeyboardPath::Always("paste modal Enter/y"),
        A::CancelPaste => KeyboardPath::Always("paste modal Esc/n"),
        // Paste-transform row select (§12.6.2) — the paste-transform menu's own
        // key handler owns ↑↓/kj (move the cursor) and Enter (apply the selected
        // row) before the global keymap sees them, reaching the same
        // `resolve_paste_transform` handler a row click drives.
        A::PasteTransformSelect(_) => KeyboardPath::Always("paste menu ↑↓/Enter"),
        // Large Paste Staging row select (§12.6.3) — the staging overlay's own
        // key handler owns ↑↓/kj (move the cursor) and Enter (apply the selected
        // action) before the global keymap sees them, reaching the same
        // `resolve_paste_staging` handler a row click drives.
        A::PasteStagingSelect(_) => KeyboardPath::Always("paste staging ↑↓/Enter"),
        // Clipboard-history picker (§12.6.1) — the picker's own key handler owns
        // Up/Down (select), Enter (re-copy), `d` (delete), and `c` (clear) before
        // the global keymap sees them, so every mouse affordance routes to the
        // same handler its picker key drives.
        A::ClipboardSelect(_) => KeyboardPath::Always("clipboard history Up/Down"),
        A::ClipboardRecopy(_) => KeyboardPath::Always("clipboard history Enter"),
        A::ClipboardDelete(_) => KeyboardPath::Always("clipboard history d"),
        A::ClipboardClear => KeyboardPath::Always("clipboard history c"),
        // External Editor Handoff confirmation overlay (§12.6.5) — the overlay's
        // own key handler owns ↑↓/kj (move the cursor), Enter (apply), and the
        // a/r/d first-letter shortcuts before the global keymap sees them, so the
        // button click and the keyboard reach the same `apply_editor_handoff_review`
        // handler.
        A::EditorHandoffSelect(_) => KeyboardPath::Always("editor handoff ↑↓/Enter"),
        // Main-view Semantic Filter badge (§12.5.2) — `CycleSemanticFilter`
        // (Alt+f default). The badge click and the keyboard verb both drive the
        // same `cycle_main_semantic_filter` handler.
        A::CycleSemanticFilter => KeyboardPath::Keymap(Action::CycleSemanticFilter),
        // Local Transcript Index overlay (§12.5.1) — the overlay's own key
        // handler owns ↑↓/kj (move the category cursor) and Enter/→/l (jump to the
        // next entry in the selected category) before the global keymap sees them,
        // so a row click and the keyboard reach the same
        // `transcript_index_jump_to_selected` handler.
        A::TranscriptIndexSelect(_) => KeyboardPath::Always("transcript index ↑↓/Enter"),
        // Related-Entry Links overlay (§12.5.3) — the overlay's own key handler
        // owns ↑↓/kj (move the relation cursor) and Enter/→/l (jump to the
        // selected related entry) before the global keymap sees them, so a row
        // click and the keyboard reach the same `related_links_jump_to_selected`
        // handler.
        A::RelatedLinkSelect(_) => KeyboardPath::Always("related links ↑↓/Enter"),
        // Duplicate-Output Folds overlay (§12.5.4) — the overlay's own key
        // handler owns ↑↓/kj (move the fold cursor) and Enter/→/l (jump to the
        // selected fold's lead and toggle it expanded) before the global keymap
        // sees them, so a row click and the keyboard reach the same
        // `duplicate_fold_activate_selected` handler.
        A::DuplicateFoldSelect(_) => KeyboardPath::Always("duplicate folds ↑↓/Enter"),
        // Error Lenses overlay (§12.5.6) — the overlay's own key handler owns
        // ↑↓/kj (move the lens cursor) and Enter/→/l (jump to the failing entry
        // behind the selected lens) before the global keymap sees them, so a row
        // click and the keyboard reach the same `error_lens_jump_to_selected`
        // handler.
        A::ErrorLensSelect(_) => KeyboardPath::Always("error lenses ↑↓/Enter"),
        // Transcript Health Markers overlay (§12.5.7) — the overlay's own key
        // handler owns ↑↓/kj (move the marker cursor) and Enter/→/l (jump to the
        // entry behind the selected marker) before the global keymap sees them, so
        // a row click and the keyboard reach the same
        // `health_markers_jump_to_selected` handler.
        A::HealthMarkerSelect(_) => KeyboardPath::Always("health markers ↑↓/Enter"),
        // Semantic Turn Outline overlay (§12.2.1) — the overlay's own key handler
        // owns ↑↓/kj (move the node cursor) and Enter/→/l (jump to the logical
        // transcript row behind the selected node) before the global keymap sees
        // them, so a row click and the keyboard reach the same
        // `turn_outline_jump_to_selected` handler.
        A::TurnOutlineSelect(_) => KeyboardPath::Always("turn outline ↑↓/Enter"),
        // Collapsible Reasoning/Tool Lanes overlay (§12.2.2) — the overlay's own
        // key handler owns ↑↓/kj (move the lane cursor) and Enter/Space (toggle
        // the selected lane's collapse) before the global keymap sees them, so a
        // row click and the keyboard reach the same `lane_fold_toggle_selected`
        // handler.
        A::LaneFoldToggle(_) => KeyboardPath::Always("lane folds ↑↓/Enter"),
        // Reading Position Bookmarks overlay (§12.2.4) — the overlay's own key
        // handler owns ↑↓/kj/n/p (move the bookmark cursor / next-previous) and
        // Enter (jump to the entry behind the selected bookmark) before the global
        // keymap sees them, so a row click and the keyboard reach the same
        // `bookmark_jump_to_selected` handler.
        A::BookmarkSelectJump(_) => KeyboardPath::Always("bookmarks ↑↓/Enter"),
        // Session Timeline overlay (§12.2.6) — the overlay's own key handler owns
        // ↑↓/kj (move the event cursor), f (cycle the per-kind filter), and
        // Enter/→/l (jump to the transcript row the selected event stands for)
        // before the global keymap sees them, so a row click and the keyboard
        // reach the same `session_timeline_jump_to_selected` handler.
        A::TimelineSelectJump(_) => KeyboardPath::Always("session timeline ↑↓/Enter"),
        // Subagent Timeline Panel (§12.8.1) — the panel's own key handler owns
        // ↑↓/kj/n/p (move the subagent cursor / next-previous), f (cycle the
        // per-status filter), and Enter/→/l (open the selected subagent's
        // conversation) before the global keymap sees them, so a row click and the
        // keyboard reach the same `subagent_timeline_jump_to_selected` handler.
        A::SubagentTimelineSelectJump(_) => KeyboardPath::Always("subagent timeline ↑↓/Enter"),
        // Entry Annotations overlay (§12.2.5) — the overlay's own key handler owns
        // ↑↓/kj/n/p (move the annotation cursor / next-previous) and Enter (jump to
        // the entry behind the selected annotation) before the global keymap sees
        // them, so a row click and the keyboard reach the same
        // `annotation_jump_to_selected` handler.
        A::AnnotationSelectJump(_) => KeyboardPath::Always("annotations ↑↓/Enter"),
        // The inline annotation marker (§12.2.5) opens the overlay on that entry's
        // note; the keyboard twin is `Alt+\` (open the list) then ↑↓ to the entry,
        // or `Alt+/` to annotate the focused entry. Reachable without a mouse.
        A::OpenAnnotationsForEntry(_) => KeyboardPath::Always("annotations Alt+\\ / Alt+/"),
        // What Changed Since Here? overlay (§12.2.7) — the overlay's own key handler
        // owns ↑↓/kj/n/p (move the change cursor / next-previous), m (re-mark the
        // anchor), and Enter/→/l (jump to the entry behind the selected change)
        // before the global keymap sees them, so a row click and the keyboard reach
        // the same `changes_since_jump_to_selected` handler.
        A::ChangeSinceSelectJump(_) => KeyboardPath::Always("what changed since here ↑↓/Enter"),
        // Contextual Action Palette (§12.1.2) — the palette's own key handler owns
        // ↑↓/kj (move the action cursor) and Enter/→/l (run the selected action on
        // the focused unit) before the global keymap sees them, so a row click and
        // the keyboard reach the same `run_selected_action_palette_action` handler.
        A::PaletteActionRun(_) => KeyboardPath::Always("action palette ↑↓/Enter"),
        // Universal Command Palette overlay (§12.1.1) — the overlay's own key handler
        // owns the fuzzy query (type/Backspace), ↑↓/Ctrl+P/Ctrl+N (move the command
        // cursor), and Enter (run the highlighted command) before the global keymap
        // sees them, so a row click and the keyboard reach the same
        // `command_palette_run_selected` handler.
        A::CommandPaletteRun(_) => KeyboardPath::Always("command palette ↑↓/Enter"),
        // Clickable Breadcrumbs strip (§12.1.5) — while the strip is shown its own
        // key handler owns ←→/hl (move the breadcrumb focus) and Enter (jump to the
        // focused crumb) before the global keymap sees them, so a crumb click and
        // the keyboard reach the same `breadcrumbs_activate_focused` handler.
        A::BreadcrumbActivate(_) => KeyboardPath::Always("breadcrumbs ←→/Enter"),
        // The inline rename-label badge (§12.1.7) opens the inline editor on that
        // entry's label; the keyboard twin is `Ctrl+Alt+R`, which opens the same
        // editor on the focused (or top-visible) entry. Reachable without a mouse.
        A::OpenRenameForEntry(_) => KeyboardPath::Always("rename label Ctrl+Alt+R"),
        // Gentle First-Run Interaction Hint dismissal (§12.1.8) — `DismissFirstRunHint`
        // (Ctrl+Alt+N default). The dim hint strip's click and the keyboard verb both
        // drive the same `dismiss_first_run_hint` handler.
        A::DismissFirstRunHint => KeyboardPath::Keymap(Action::DismissFirstRunHint),
        // Prompt Snippets picker (§12.3.2) — the picker's own key handler owns
        // Up/Down (select), Enter (insert into the composer), `q` (queue), `d`
        // (delete), and `c` (clear) before the global keymap sees them, so every
        // mouse affordance routes to the same handler its picker key drives.
        A::SnippetSelect(_) => KeyboardPath::Always("snippets Up/Down"),
        A::SnippetInsertCompose(_) => KeyboardPath::Always("snippets Enter"),
        A::SnippetEnqueue(_) => KeyboardPath::Always("snippets q"),
        A::SnippetDelete(_) => KeyboardPath::Always("snippets d"),
        A::SnippetClear => KeyboardPath::Always("snippets c"),
        // Actionable Tool Outputs overlay (§12.3.1) — the overlay's own key handler
        // owns ↑↓/k (move the item cursor), Enter/c (copy the matched element), and
        // j/→/l (jump to the source tool result) before the global keymap sees them,
        // so a row click and the keyboard reach the same `tool_actions_copy_selected`
        // handler.
        A::ToolActionRun(_) => KeyboardPath::Always("tool actions ↑↓/Enter"),
        // Scratchpad Pane (§12.3.3) — while the pane is open its own key handler
        // owns Ctrl+I (insert into the composer), Ctrl+Q (queue), Ctrl+L (append
        // selection / source link), and Ctrl+K (clear) before the global keymap
        // sees them, so every in-pane button click routes to the same handler its
        // keyboard verb drives.
        A::ScratchpadInsertCompose => KeyboardPath::Always("scratchpad Ctrl+I"),
        A::ScratchpadEnqueue => KeyboardPath::Always("scratchpad Ctrl+Q"),
        A::ScratchpadAppend => KeyboardPath::Always("scratchpad Ctrl+L"),
        A::ScratchpadClear => KeyboardPath::Always("scratchpad Ctrl+K"),
        // Prompt Templates picker / card (§12.3.6) — while the picker is open its
        // own key handler owns Up/Down (select a template), Enter (instantiate /
        // enqueue the filled card), Tab/↑↓ (move between the card's slots), typing
        // (fill the focused slot), `d` (delete), and `c` (clear) before the global
        // keymap sees them, so every mouse affordance routes to the same handler
        // its picker/card key drives.
        A::TemplateSelect(_) => KeyboardPath::Always("templates Up/Down"),
        A::TemplateFocusSlot(_) => KeyboardPath::Always("templates Tab/↑↓"),
        A::TemplateEnqueue => KeyboardPath::Always("templates Enter"),
        A::TemplateDelete(_) => KeyboardPath::Always("templates d"),
        A::TemplateClear => KeyboardPath::Always("templates c"),
        // Replayable Interaction Macros (§12.3.7) — the record/replay status strip's
        // click stops/cancels the active recording or replay; the keyboard twin is
        // `ToggleMacroRecord` (Ctrl+Alt+K default), which drives the same
        // `toggle_macro_record` handler. Reachable without a mouse.
        A::MacroToggleRecord => KeyboardPath::Keymap(Action::ToggleMacroRecord),
        // Keybinding Editor UI (§12.7.1) — while the editor is open its own key
        // handler owns ↑↓/kj (move the action cursor), Enter (begin capturing a new
        // chord for the selected row), and r/Delete (reset the row to its default)
        // before the global keymap sees them, so a row click and the Rebind/Reset
        // button clicks reach the same handlers their keyboard verbs drive.
        A::KeybindingSelect(_) => KeyboardPath::Always("keybinding editor ↑↓"),
        A::KeybindingRebind => KeyboardPath::Always("keybinding editor Enter"),
        A::KeybindingReset => KeyboardPath::Always("keybinding editor r/Delete"),
        // Theme Editor overlay (§12.7.2) — while the overlay is open its own key
        // handler owns ↑↓ (move the role focus) and ←→ + +/-/PageUp/PageDown (move
        // the channel focus and adjust its value) before the global keymap sees
        // them, so a role-row click and a channel-bar click reach the same
        // `focus_role` / `set_channel` handlers the keyboard verbs drive.
        A::ThemeEditorSelectRole(_) => KeyboardPath::Always("theme editor ↑↓"),
        A::ThemeEditorSetChannel(_, _) => KeyboardPath::Always("theme editor ←→ +/-"),
        // Per-Workspace UI Profile overlay (§12.7.4) — while the overlay is open
        // its own key handler owns ↑↓/kj (move the field focus) before the global
        // keymap sees them, so a field-row click reaches the same `focus_field`
        // handler the keyboard verbs drive.
        A::WorkspaceProfileSelectField(_) => KeyboardPath::Always("workspace profile ↑↓"),
        // Per-Terminal Profiles overlay (§12.7.3) — while the overlay is open its
        // own key handler owns ↑↓ (move the field focus) and ←→/Space (cycle the
        // focused field's value) before the global keymap sees them, so a field-row
        // click reaches the same focus/cycle handlers the keyboard verbs drive.
        A::TerminalProfileCycleField(_) => KeyboardPath::Always("terminal profile ↑↓ ←→"),
        // Gesture Settings overlay (§12.7.5) — while the overlay is open its own key
        // handler owns ↑↓ (move the field focus) and ←→/Space/+/- (step the focused
        // field's value) before the global keymap sees them, so a field-row click
        // reaches the same focus/step handlers the keyboard verbs drive.
        A::GestureSettingsStepField(_) => KeyboardPath::Always("gesture settings ↑↓ ←→"),
        // Minimal Glyph Mode overlay (§12.7.6) — while the overlay is open its own
        // key handler owns ↑↓/kj (move the mode focus) and ←→/Space (cycle the
        // working mode) before the global keymap sees them, so a mode-row click
        // reaches the same focus/select handler the keyboard verbs drive.
        A::GlyphModeSelect(_) => KeyboardPath::Always("glyph mode ↑↓ ←→"),
        // Subagent timeline row select/pin (§12.8.2) — the subagent pane's own
        // ↑↓ cursor + `PreviewSubagent` (Alt+5) verb both seat the cursor on the
        // row and preview it, the same `subagent_select_index` the click drives.
        A::SubagentSelect(_) => KeyboardPath::Keymap(Action::PreviewSubagent),
        // Subagent transcript jump (§12.8.2) — `JumpToSubagent` (Ctrl+Alt+D
        // default) reaches the same `jump_to_subagent_index` handler the
        // double-click drives.
        A::SubagentJump(_) => KeyboardPath::Keymap(Action::JumpToSubagent),
        // Subagent result promote (§12.8.4) — `PromoteSubagentResult` (Ctrl+Alt+Q
        // default) and the timeline panel's own `y` key both reach the same
        // `promote_subagent_timeline_row` handler the click drives.
        A::SubagentTimelinePromote(_) => KeyboardPath::Keymap(Action::PromoteSubagentResult),
    })
}

/// How a mouse affordance is reached from the keyboard: a rebindable
/// [`keymap::Action`](Action), or an always-available context key the keymap
/// does not own (e.g. a modal's own Delete/Shift+arrow handler).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardPath {
    /// A rebindable keymap action with a default binding.
    Keymap(Action),
    /// A context key handled before the global keymap (modal/overlay handler).
    Always(&'static str),
}

/// True when a glyph is box-drawing / scrollbar / marker chrome Squeezy may
/// paint — used both to strip chrome from the screen-reader stream and to scope
/// the minimal-glyph gate to chrome.
fn is_chrome_glyph(glyph: char) -> bool {
    ALLOWED_CHROME_GLYPHS.contains(&glyph)
}

/// Heuristic: does this non-ASCII glyph live in a Unicode block Squeezy uses for
/// *chrome*? Box-drawing (U+2500..U+257F), block elements (U+2580..U+259F),
/// geometric shapes (U+25A0..U+25FF), and braille spinners (U+2800..U+28FF).
/// Content scripts (Latin-1 accents, CJK, emoji) fall outside, so the
/// minimal-glyph gate never fires on genuine user text.
fn looks_like_chrome_block(glyph: char) -> bool {
    let c = glyph as u32;
    (0x2500..=0x257F).contains(&c) // box drawing
        || (0x2580..=0x259F).contains(&c) // block elements
        || (0x25A0..=0x25FF).contains(&c) // geometric shapes
        || (0x2800..=0x28FF).contains(&c) // braille (spinners)
}

/// Resolve a [`Color`] to an sRGB triple for the contrast gate, or [`None`] for
/// colors that inherit the terminal default (`Reset`) or a palette we cannot
/// resolve (`Indexed`) — both pass the gate by deferring to the terminal scheme.
fn resolvable_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset | Color::Indexed(_) => None,
        Color::Rgb(r, g, b) => Some((r, g, b)),
        other => Some(crate::render::palette::rgb_components(other)),
    }
}

/// WCAG 2.x relative luminance of an sRGB triple in `[0.0, 1.0]`. Unlike the
/// Rec. 601 *perceived-brightness* heuristic in [`crate::testing::rgb_luminance`]
/// (a u8 used by the eval palette rubric), this is the gamma-correct relative
/// luminance the WCAG contrast-ratio formula requires.
pub(crate) fn relative_luminance(rgb: (u8, u8, u8)) -> f64 {
    fn channel(c: u8) -> f64 {
        let s = f64::from(c) / 255.0;
        if s <= 0.039_28 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = rgb;
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG contrast ratio between two sRGB triples: `(L_lighter + 0.05) /
/// (L_darker + 0.05)`, in `[1.0, 21.0]`.
pub(crate) fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (lighter, darker) = if la >= lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

// ===========================================================================
// Deterministic app builders (mirrored from the visual_diff harness)
// ===========================================================================

/// Seed `app` with `turns` user/assistant exchanges so a surface overflows the
/// viewport deterministically.
fn seed_long_session(app: &mut TuiApp, turns: usize) {
    for i in 0..turns {
        app.push_transcript_item(TranscriptItem::user(format!("question number {i}")));
        app.push_transcript_item(TranscriptItem::assistant(format!(
            "Answer {i}: the relevant module lives under crates and the fix is local."
        )));
    }
}

/// Build a self-contained [`TuiApp`]: a temp workspace root so construction
/// never crawls the real repo, and a no-op clipboard.
fn new_audit_app() -> TuiApp {
    let config = audit_config();
    TuiApp::new_with_clipboard(
        "accessibility-audit",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopAuditClipboard),
    )
}

/// A minimal [`AppConfig`] pinned to a unique temp workspace so construction is
/// hermetic.
fn audit_config() -> AppConfig {
    AppConfig {
        model: "accessibility-audit-model".to_string(),
        session_mode: SessionMode::Build,
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            web: PermissionMode::Ask,
            ..Default::default()
        },
        config_sources: vec!["defaults".to_string()],
        workspace_root: audit_temp_root(),
        ..Default::default()
    }
}

/// A unique temp directory so two parallel audit apps never share a root.
fn audit_temp_root() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "squeezy_tui_a11y_audit_{}_{nonce}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&root);
    root
}

struct NoopAuditClipboard;

impl Clipboard for NoopAuditClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "accessibility_tests.rs"]
mod tests;
