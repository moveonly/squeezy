//! External Editor Handoff (§12.6.5).
//!
//! Lets the user edit the composer text in their own `$VISUAL`/`$EDITOR`, then
//! re-import the result with an accept / reopen / discard confirmation. The
//! composer is fine for short prompts but a poor fit for a multi-paragraph
//! prompt the user would rather shape in vim/helix/VS Code; this module hands a
//! temp file to that editor and reads the saved buffer back.
//!
//! ## What lives here (pure, cross-platform, fully testable)
//!
//!   - [`resolve_editor`] reads `$VISUAL` then `$EDITOR` (injected `env_get` so
//!     the tests never mutate process env) and splits the value into an
//!     [`EditorCommand`] (program + args). Returns `None` — the safe fallback —
//!     when neither variable is set or both are blank, so the caller can degrade
//!     to a status hint instead of spawning nothing.
//!   - [`EditorTarget`] names what is being edited and supplies the temp-file
//!     extension (`.md` for the composer prose) so the editor lights up syntax.
//!   - [`temp_file_name`] builds a collision-resistant temp filename from the
//!     target, the process id, and a per-app sequence number — no hardcoded
//!     path, just the leaf the caller joins onto [`std::env::temp_dir`].
//!   - [`run_handoff`] is the testable core: it writes the initial text to a
//!     file in the supplied directory, invokes an injected `run_editor` closure
//!     (the real spawn in `lib.rs`, a fake editor in the tests), reads the saved
//!     buffer back, classifies the result, and always deletes the temp file.
//!   - [`HandoffOutcome`] / [`classify_result`] decide whether the edit changed
//!     anything, so the caller only re-imports a real change.
//!   - [`EditorHandoffReview`] is the selectable confirmation overlay state
//!     (accept / reopen / discard) `lib.rs` renders after a successful edit.
//!
//! ## What stays in `lib.rs` (side effects)
//!
//! The keybinding, the terminal suspend/restore around the spawn, the actual
//! `std::process::Command` build, and applying the accepted text to the composer
//! all live in the crate root, which owns the terminal guard and the composer.
//! The real spawn is `#[cfg(unix)]`: leaving the alternate screen before running
//! a full-screen editor is the only safe option, and Windows process creation /
//! terminal restoration differ sharply (see the spec's platform notes), so the
//! non-Unix build degrades to a status hint rather than guessing.
//!
//! Everything in this module is IO-light and deterministic: [`run_handoff`] is
//! the only function that touches the filesystem, and it does so only inside a
//! caller-supplied directory through an injected editor closure, so the tests
//! drive the whole modify / unchanged / fail / slow-editor matrix without a real
//! terminal or a real editor.
//!
//! The runtime entrypoints ([`resolve_editor`], [`run_handoff`], …) are driven
//! only from the `#[cfg(unix)]` spawn path in `lib.rs`, so on a non-Unix build
//! they are exercised solely by this module's tests (which compile on every
//! platform). The module-level `cfg_attr(not(unix), allow(dead_code))` keeps the
//! Windows `-D warnings` clippy gate green without forking the logic — the same
//! pure code is type-checked and tested everywhere, it just is not *wired* to a
//! keybinding off Unix.
#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// What the user is handing off to the external editor. Only the composer is
/// wired today; the variant carries the syntax extension so a future queue-item
/// or export-buffer target slots in without touching the spawn plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorTarget {
    /// The main composer text. Edited as Markdown (`.md`) so an editor with
    /// filetype detection gives prose-friendly wrapping/highlighting.
    Composer,
}

impl EditorTarget {
    /// The temp-file extension (without the dot) for this target, chosen so the
    /// editor's filetype detection lights up. Composer prose is Markdown.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            EditorTarget::Composer => "md",
        }
    }

    /// A short human label used in the temp-file stem and the status/summary
    /// lines, so a stray temp file is self-describing.
    pub(crate) fn label(self) -> &'static str {
        match self {
            EditorTarget::Composer => "composer",
        }
    }
}

/// A queued request to hand a buffer off to the external editor, stamped by the
/// `Alt+e` dispatch and consumed by the run loop (which owns the terminal guard
/// and so can suspend the alt-screen around the spawn). Keeping the request as a
/// small value on the app lets the keymap arm stay side-effect-light: it only
/// resolves the editor and records *what* to edit; the loop does the heavy
/// suspend/spawn/restore on its next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorHandoffRequest {
    /// What is being edited (drives the temp-file extension and the labels).
    pub(crate) target: EditorTarget,
    /// The resolved editor invocation (program + fixed args).
    pub(crate) command: EditorCommand,
    /// The text to seed the temp file with.
    pub(crate) initial_text: String,
}

/// A resolved editor invocation: the program to run plus any fixed leading
/// arguments parsed out of the `$VISUAL`/`$EDITOR` value (e.g. `"code --wait"`
/// becomes `program = "code"`, `args = ["--wait"]`). The temp-file path is
/// appended by the caller as the final argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorCommand {
    /// The editor binary (first whitespace-delimited token of the env value).
    pub(crate) program: String,
    /// Fixed arguments that preceded the file path in the env value.
    pub(crate) args: Vec<String>,
}

impl EditorCommand {
    /// Human-facing reconstruction (`"code --wait"`) for the status line. Not a
    /// shell-quoting round-trip — just a readable echo of what will run.
    pub(crate) fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// Resolve the editor from the environment: `$VISUAL` wins over `$EDITOR` (the
/// long-standing Unix convention — `$VISUAL` is the full-screen editor, `$EDITOR`
/// the line editor), and a blank/whitespace-only value is treated as unset so a
/// stray `EDITOR=` does not spawn an empty command. Returns `None` when no usable
/// editor is configured, which the caller surfaces as a safe "set $EDITOR" hint.
///
/// Parsing is a deliberately conservative whitespace split, NOT a shell parse:
/// the spec warns that shell-string parsing is a portability trap (quoting rules
/// differ across shells and Windows), so a value with quoted spaces in the path
/// is not supported here — the common `"vim"`, `"nvim"`, `"code --wait"`,
/// `"emacs -nw"` forms all resolve correctly, and anything more exotic falls back
/// to treating the whole first token as the program.
pub(crate) fn resolve_editor<F>(env_get: F) -> Option<EditorCommand>
where
    F: Fn(&str) -> Option<OsString>,
{
    for key in ["VISUAL", "EDITOR"] {
        let Some(raw) = env_get(key) else {
            continue;
        };
        let Some(value) = raw.to_str() else {
            // Non-UTF-8 editor spec: skip rather than guess at bytes.
            continue;
        };
        if let Some(command) = parse_editor_value(value) {
            return Some(command);
        }
    }
    None
}

/// Split a raw `$VISUAL`/`$EDITOR` value into program + leading args. Returns
/// `None` for a blank value so an empty `EDITOR=` does not resolve to an empty
/// program. Whitespace-split only (see [`resolve_editor`]'s parsing note).
fn parse_editor_value(value: &str) -> Option<EditorCommand> {
    let mut tokens = value.split_whitespace();
    let program = tokens.next()?.to_string();
    let args: Vec<String> = tokens.map(str::to_string).collect();
    Some(EditorCommand { program, args })
}

/// Build the leaf temp-file name for a handoff. Collision-resistant without
/// needing a real mkstemp: the process id plus a caller-supplied monotonic
/// sequence number make concurrent or repeated handoffs land on distinct files,
/// and the target label + extension keep a stray file self-describing. The
/// caller joins this onto [`std::env::temp_dir`] (never a hardcoded `/tmp`).
pub(crate) fn temp_file_name(target: EditorTarget, pid: u32, seq: u64) -> String {
    format!(
        "squeezy_edit_{}_{}_{}.{}",
        target.label(),
        pid,
        seq,
        target.extension(),
    )
}

/// The outcome of a completed editor session, after the saved buffer is read
/// back and compared to what was handed off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandoffOutcome {
    /// The editor saved a buffer that differs from the original. Carries the new
    /// text for the caller to re-import (after the accept confirmation).
    Changed(String),
    /// The editor exited without changing the text (saved the same bytes, or
    /// quit without saving). The caller leaves the composer untouched.
    Unchanged,
}

/// Compare the edited buffer against the original and classify the result. The
/// only normalization is a trailing-newline trim on the *edited* side: most
/// editors append a final newline on save, so `"hello"` handed off and saved as
/// `"hello\n"` reads as unchanged rather than a spurious edit. Interior content
/// is compared verbatim.
pub(crate) fn classify_result(original: &str, edited: &str) -> HandoffOutcome {
    let normalized = edited.strip_suffix('\n').unwrap_or(edited);
    if normalized == original {
        HandoffOutcome::Unchanged
    } else {
        HandoffOutcome::Changed(normalized.to_string())
    }
}

/// Create `path` exclusively and write `bytes` to it.
///
/// Uses `create_new(true)` (O_CREAT|O_EXCL) so a pre-existing file or symlink
/// already sitting at the predictable temp path causes the write to fail with
/// `AlreadyExists` instead of being followed/truncated — closing the symlink
/// (CWE-59) and clobber holes the old `fs::write` left open. On Unix the file is
/// forced to mode 0o600 so a composer/paste buffer is not world-readable
/// (CWE-377).
fn write_new_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Run a full editor handoff against a real (or fake) editor.
///
/// Writes `initial_text` to `<dir>/<temp_file_name>`, invokes `run_editor` with
/// the resolved [`EditorCommand`] and the temp path (the real spawn in `lib.rs`
/// suspends the terminal around this; the tests pass a closure that mutates the
/// file in place), reads the saved buffer back, classifies it against
/// `initial_text`, and ALWAYS deletes the temp file before returning — even when
/// the editor errors — so a handoff never litters the temp directory.
///
/// Returns the classified [`HandoffOutcome`] on success, or the editor's
/// `io::Error` on failure (the caller restores the terminal and reports it). The
/// temp file is cleaned up in both paths.
pub(crate) fn run_handoff<R>(
    command: &EditorCommand,
    target: EditorTarget,
    initial_text: &str,
    dir: &Path,
    pid: u32,
    seq: u64,
    run_editor: R,
) -> io::Result<HandoffOutcome>
where
    R: FnOnce(&EditorCommand, &Path) -> io::Result<()>,
{
    let path: PathBuf = dir.join(temp_file_name(target, pid, seq));
    write_new_private(&path, initial_text.as_bytes())?;

    // Run the editor with the SIGTSTP (Ctrl+Z) disposition reset to SIG_DFL for
    // the duration of the (blocking) spawn. Without this, a Ctrl+Z INSIDE the
    // editor wedges the session: the editor child stops, but the spawn's
    // `waitpid` (no `WUNTRACED`) never returns for a stopped child, and squeezy's
    // tokio notify-only SIGTSTP listener only flips a flag the blocked main loop
    // cannot act on. With SIG_DFL the editor job is stopped/continued by the
    // shell's job control normally. The previous disposition is restored when the
    // editor returns. (deep-review #5)
    //
    // On failure still clean up the temp file before bubbling the error so a
    // failed spawn never leaves a stray file behind.
    let run_result = crate::signal_teardown::with_default_sigtstp(|| run_editor(command, &path));
    if let Err(error) = run_result {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }

    // Only delete the temp file when the read-back SUCCEEDS — the buffer is then
    // safely in memory. A read failure (e.g. the editor saved non-UTF-8 bytes)
    // must PRESERVE the user's edits on disk and surface the path, rather than
    // silently destroying the session by deleting an unreadable buffer.
    let edited = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let _ = std::fs::remove_file(&path);
            text
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "editor handoff read failed; your edits are preserved at {}: {error}",
                    path.display()
                ),
            ));
        }
    };
    Ok(classify_result(initial_text, &edited))
}

/// The action a user can take on the post-edit confirmation overlay. Ordered as
/// rendered (accept first, the safe default; discard last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewAction {
    /// Re-import the edited text into the composer.
    Accept,
    /// Reopen the editor on the edited text for another pass.
    Reopen,
    /// Throw the edit away and keep the composer as it was.
    Discard,
}

impl ReviewAction {
    /// The actions offered, in render order. Accept is first so Enter on a fresh
    /// overlay re-imports the edit — the expected default after saving.
    pub(crate) const ALL: [ReviewAction; 3] = [
        ReviewAction::Accept,
        ReviewAction::Reopen,
        ReviewAction::Discard,
    ];

    /// The button label shown in the overlay (also the mouse target's glyph).
    pub(crate) fn label(self) -> &'static str {
        match self {
            ReviewAction::Accept => "Accept",
            ReviewAction::Reopen => "Reopen",
            ReviewAction::Discard => "Discard",
        }
    }
}

/// The post-edit confirmation overlay state. Holds what was edited (so Accept
/// re-imports it and Reopen hands it back to the editor), a short diff summary
/// for the header, and the selectable action cursor. Built only after a
/// successful edit that actually changed the text, so the overlay never opens on
/// an unchanged buffer.
#[derive(Debug, Clone)]
pub(crate) struct EditorHandoffReview {
    /// What is being edited (drives the header label and a Reopen round-trip).
    pub(crate) target: EditorTarget,
    /// The edited text to re-import on Accept (or re-edit on Reopen).
    pub(crate) edited_text: String,
    /// Lines in the original buffer (for the "N → M lines" summary).
    pub(crate) original_lines: usize,
    /// Lines in the edited buffer.
    pub(crate) edited_lines: usize,
    /// Cursor into [`ReviewAction::ALL`]; the highlighted/applied action.
    selected: usize,
}

impl EditorHandoffReview {
    /// Build the review overlay from the original and edited text. Precomputes
    /// the line counts shown in the summary so the render path stays allocation-
    /// light. Starts with the Accept action selected.
    pub(crate) fn new(target: EditorTarget, original: &str, edited: String) -> Self {
        Self {
            target,
            original_lines: line_count(original),
            edited_lines: line_count(&edited),
            edited_text: edited,
            selected: 0,
        }
    }

    /// The currently highlighted action.
    pub(crate) fn selected_action(&self) -> ReviewAction {
        ReviewAction::ALL[self.selected]
    }

    /// 0-based index of the highlighted action (for render highlighting and the
    /// hit-test parity check).
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// Move the cursor to the previous action, saturating at the top so a held
    /// key never wraps past Accept.
    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor to the next action, saturating at the bottom so a held
    /// key never wraps past Discard.
    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < ReviewAction::ALL.len() {
            self.selected += 1;
        }
    }

    /// Move the cursor directly to `index` (a mouse click on a button row),
    /// clamped into range so an out-of-bounds click is a no-op rather than a
    /// panic.
    pub(crate) fn select(&mut self, index: usize) {
        if index < ReviewAction::ALL.len() {
            self.selected = index;
        }
    }

    /// A one-line summary of the change for the overlay header, e.g.
    /// `"composer · 3 → 5 lines"`. Honest about the unit (lines) since this is a
    /// coarse summary, not a real diff.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · {} → {} {}",
            self.target.label(),
            self.original_lines,
            self.edited_lines,
            if self.edited_lines == 1 {
                "line"
            } else {
                "lines"
            },
        )
    }
}

/// Count lines the way the summary reports them: `\n`-separated segments, with a
/// trailing newline NOT adding a phantom empty line (so `"a\nb"` and `"a\nb\n"`
/// are both 2), and the empty string counted as 0 lines.
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    trimmed.split('\n').count()
}

#[cfg(test)]
#[path = "editor_handoff_tests.rs"]
mod tests;
