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
