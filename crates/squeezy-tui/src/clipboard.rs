//! Clipboard provider chain.
//!
//! Copying text to the system clipboard from a terminal app is annoyingly
//! non-uniform: the OSC 52 escape works over SSH/tmux with no local helper
//! but is size-limited and silently dropped by some emulators; the platform
//! clipboard commands (`pbcopy`, `wl-copy`, `xclip`/`xsel`, `clip.exe`/
//! PowerShell) are reliable but only when a local clipboard daemon and the
//! right binary are present; and when neither works we still owe the user a
//! durable artefact rather than a silent failure.
//!
//! This module models that as an ordered chain of [`ClipboardProvider`]s
//! walked by [`ClipboardChain::copy`]:
//!
//! 1. **OSC 52** — base64-encode the payload and write the escape to the
//!    terminal. Honours a configurable byte cap; an oversized payload is
//!    refused up front so the chain falls through to the next provider rather
//!    than emitting one escape the terminal would silently drop.
//! 2. **Platform command** — pipe the payload to the host's clipboard binary.
//! 3. **Temp file** — write the payload to a file under the temp dir and
//!    surface the path so the caller can tell the user where it landed.
//!
//! ## Trace-testability
//!
//! The chain never touches a real terminal, clipboard, or subprocess
//! directly. Every side effect goes through the [`ClipboardSink`] seam, whose
//! production impl is [`RealSink`] and whose test impl is [`RecordingSink`]
//! (mirroring the `size_source.rs` template and generalising the existing
//! `RecordingClipboard` test fake). A test drives the whole chain against a
//! `RecordingSink`, scripts each method to succeed or fail, and then asserts
//! *which* provider was attempted, *in what order*, and the *exact bytes*
//! produced — with no clipboard or process involved.
//!
//! ## Capability probe
//!
//! [`detect_clipboard_capabilities_from_env`] follows the DEC-2026 detection
//! shape in `lib.rs`: a pure function over an env-getter closure, never
//! reading real process env in the hot path. Production threads
//! [`std::env::var_os`]; tests pass a fixture closure.

#![allow(dead_code)]

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::base64_encode;
use crate::toast::ToastVariant;

/// Default OSC 52 payload cap, matching the historical
/// `OSC52_MAX_PAYLOAD_BYTES` constant: xterm's `selectToClipboard` buffer is
/// 8 KiB and many emulators silently drop sequences past their undocumented
/// limit, so we refuse oversized OSC 52 writes up front and fall through.
pub(crate) const DEFAULT_OSC52_MAX_BYTES: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Sink seam
// ---------------------------------------------------------------------------

/// Result of running a platform clipboard command.
///
/// Carries enough detail for the failure-reason status string to quote the
/// real tool error instead of an opaque "command failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutcome {
    /// Process exit code, if one was reported (`None` for signal kills).
    pub status_code: Option<i32>,
    /// Whether the process exited successfully (zero status).
    pub success: bool,
    /// Captured stderr, trimmed, for surfacing the tool's own error text.
    pub stderr: String,
}

/// The single side-effect seam for the clipboard chain.
///
/// Production = [`RealSink`] (stdout/flush, `std::process::Command`, temp dir).
/// Tests = [`RecordingSink`], which records every call and replays scripted
/// outcomes so the chain is fully trace-testable without real I/O.
pub(crate) trait ClipboardSink: Send {
    /// Write raw OSC 52 (or any) bytes to the terminal.
    fn write_terminal(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Run a platform clipboard command, piping `payload` to its stdin.
    ///
    /// `Ok(CommandOutcome)` is returned whenever the process was spawned and
    /// reaped (even on non-zero exit); `Err` carries a spawn/IO failure such
    /// as the binary being absent.
    fn run_command(
        &mut self,
        program: &str,
        args: &[&str],
        payload: &[u8],
    ) -> io::Result<CommandOutcome>;

    /// Write the payload to a temp file and return the path actually written.
    fn write_temp_file(&mut self, payload: &[u8], suggested_name: &str) -> io::Result<PathBuf>;
}

/// Forward through a boxed sink so a [`ClipboardChain<Box<dyn ClipboardSink + Send>>`]
/// can be stored on the app and injected with either [`RealSink`] (production)
/// or a [`RecordingSink`] (tests) without monomorphizing the field to one impl.
/// This is what keeps the chain's trace-test seam alive at the app layer.
/// `+ Send` keeps the boxed sink `Send` so the owning app/`Driver` future
/// stays `Send` for `tokio::spawn`.
impl ClipboardSink for Box<dyn ClipboardSink + Send> {
    fn write_terminal(&mut self, bytes: &[u8]) -> io::Result<()> {
        (**self).write_terminal(bytes)
    }

    fn run_command(
        &mut self,
        program: &str,
        args: &[&str],
        payload: &[u8],
    ) -> io::Result<CommandOutcome> {
        (**self).run_command(program, args, payload)
    }

    fn write_temp_file(&mut self, payload: &[u8], suggested_name: &str) -> io::Result<PathBuf> {
        (**self).write_temp_file(payload, suggested_name)
    }
}

/// Production [`ClipboardSink`]: real stdout, real subprocesses, real files.
#[derive(Debug, Default)]
pub(crate) struct RealSink;

/// Monotonic counter so concurrent temp-file writes don't collide on name.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ClipboardSink for RealSink {
    fn write_terminal(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut stdout = io::stdout();
        stdout.write_all(bytes)?;
        stdout.flush()
    }

    fn run_command(
        &mut self,
        program: &str,
        args: &[&str],
        payload: &[u8],
    ) -> io::Result<CommandOutcome> {
        use std::process::{Command, Stdio};

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        // Feed stdin on a SEPARATE thread so the payload write and the
        // stderr/stdout drain run concurrently. Writing the whole payload on
        // this thread before `wait_with_output` would deadlock on a large copy:
        // a child that emits to stderr before it finishes draining stdin fills
        // its (fixed-size) stderr pipe, blocks on the write, and never reads the
        // rest of stdin — while we block writing stdin and never drain stderr.
        // The writer thread owns the moved `stdin`; dropping it at the end of
        // the closure signals EOF. `wait_with_output` on this thread drains
        // stderr concurrently, then we join the writer and surface its error.
        let writer = child.stdin.take().map(|mut stdin| {
            let payload = payload.to_vec();
            std::thread::spawn(move || {
                let result = stdin.write_all(&payload);
                drop(stdin);
                result
            })
        });

        // Always reap the child first (this drains stderr), so a write error
        // never leaks the process as a zombie.
        let output = child.wait_with_output()?;
        // Join the writer and propagate any write error. A panicked writer
        // thread surfaces as a generic broken-pipe-style IO error.
        if let Some(writer) = writer {
            match writer.join() {
                Ok(write_result) => write_result?,
                Err(_) => {
                    return Err(io::Error::other("clipboard stdin writer thread panicked"));
                }
            }
        }
        Ok(CommandOutcome {
            status_code: output.status.code(),
            success: output.status.success(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }

    fn write_temp_file(&mut self, payload: &[u8], suggested_name: &str) -> io::Result<PathBuf> {
        let dir = std::env::temp_dir();
        // Create the file atomically and privately. `create_new(true)` maps to
        // O_CREAT|O_EXCL, so a pre-planted file or symlink at the predictable
        // leaf name causes EEXIST instead of being followed/truncated (no
        // symlink / CWE-59 attack, no clobber of another user's file). On Unix
        // we also force mode 0o600 so the copied payload is not world-readable
        // (CWE-377). We retry on AlreadyExists with a fresh counter so a single
        // pre-planted name does not wedge the chain.
        let mut last_err: Option<io::Error> = None;
        for _ in 0..16 {
            let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = dir.clone();
            path.push(format!(
                "squeezy-copy-{}-{}-{}",
                std::process::id(),
                counter,
                suggested_name,
            ));
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(mut file) => {
                    file.write_all(payload)?;
                    file.flush()?;
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a fresh clipboard temp file",
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// Capability probe
// ---------------------------------------------------------------------------

/// Terminal clipboard capabilities resolved from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ClipboardCapabilities {
    /// Whether the host terminal is believed to honour OSC 52 clipboard
    /// writes. When `false` the chain skips OSC 52 entirely and goes straight
    /// to the platform command.
    pub osc52: bool,
}

/// Pure capability heuristic for OSC 52 clipboard support based on
/// environment-variable signals exposed by the host terminal.
///
/// Mirrors `detect_synchronized_output_support_from_env` in `lib.rs`:
/// factored out so production threads [`std::env::var_os`] while tests pass a
/// fixture-backed closure, exercising the resolver without mutating real env.
pub(crate) fn detect_clipboard_capabilities_from_env<F>(env_get: F) -> ClipboardCapabilities
where
    F: Fn(&str) -> Option<OsString>,
{
    ClipboardCapabilities {
        osc52: detect_osc52_from_env(&env_get),
    }
}

fn detect_osc52_from_env<F>(env_get: &F) -> bool
where
    F: Fn(&str) -> Option<OsString>,
{
    // Same heuristic family as DEC-2026 detection: emulators known to
    // implement OSC 52, plus tmux which proxies it through to the outer
    // terminal when `set-clipboard on`.
    if env_get("KITTY_WINDOW_ID").is_some()
        || env_get("WEZTERM_PANE").is_some()
        || env_get("WEZTERM_EXECUTABLE").is_some()
        || env_get("GHOSTTY_RESOURCES_DIR").is_some()
        || env_get("ITERM_SESSION_ID").is_some()
        || env_get("TMUX").is_some()
    {
        return true;
    }
    if let Some(prog) = env_get("TERM_PROGRAM") {
        let prog = prog.to_string_lossy().to_ascii_lowercase();
        if matches!(
            prog.as_str(),
            "iterm.app" | "iterm2" | "wezterm" | "ghostty" | "kitty" | "vscode" | "tmux"
        ) {
            return true;
        }
    }
    if let Some(term) = env_get("TERM") {
        let term = term.to_string_lossy().to_ascii_lowercase();
        if term.contains("kitty")
            || term.contains("wezterm")
            || term.contains("ghostty")
            || term.contains("tmux")
            || term.contains("screen")
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Platform command selection
// ---------------------------------------------------------------------------

/// A single platform clipboard command candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// Resolve the ordered list of platform clipboard command candidates for the
/// host, consulting `env_get` only where the choice is env-driven (Linux
/// Wayland vs X11). Each candidate is attempted in turn; a spawn error or
/// non-zero exit falls through to the next, then to the temp-file fallback.
///
/// Kept `pub(crate)` and env-closure-driven (rather than reading real env) so
/// the selection is testable, exactly like the capability probe.
pub(crate) fn platform_commands<F>(env_get: F) -> Vec<PlatformCommand>
where
    F: Fn(&str) -> Option<OsString>,
{
    #[cfg(target_os = "macos")]
    {
        let _ = env_get;
        vec![PlatformCommand {
            program: "pbcopy",
            args: &[],
        }]
    }
    #[cfg(target_os = "windows")]
    {
        let _ = env_get;
        vec![
            PlatformCommand {
                program: "clip.exe",
                args: &[],
            },
            PlatformCommand {
                program: "powershell",
                args: &["-NoProfile", "-Command", "Set-Clipboard"],
            },
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux / other unix: prefer Wayland when `WAYLAND_DISPLAY` is set,
        // otherwise the X11 helpers. The order between wl-copy and the X11
        // tools is env-driven through the closure, mirroring the DEC-2026
        // detection style.
        let wayland = env_get("WAYLAND_DISPLAY").is_some();
        let mut cmds = Vec::new();
        if wayland {
            cmds.push(PlatformCommand {
                program: "wl-copy",
                args: &[],
            });
        }
        cmds.push(PlatformCommand {
            program: "xclip",
            args: &["-selection", "clipboard"],
        });
        cmds.push(PlatformCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
        });
        if !wayland {
            // Still offer wl-copy last in case the user is on Wayland without
            // the env var exported (rare, but cheap to try before temp-file).
            cmds.push(PlatformCommand {
                program: "wl-copy",
                args: &[],
            });
        }
        cmds
    }
}

// ---------------------------------------------------------------------------
// Providers + chain
// ---------------------------------------------------------------------------

/// One step in the clipboard provider chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClipboardProvider {
    /// OSC 52 terminal escape (base64), honouring the configured byte cap.
    Osc52,
    /// A platform clipboard command, piping the payload to its stdin.
    PlatformCommand(PlatformCommand),
    /// Durable temp-file fallback; always last.
    TempFile,
}

/// Which provider actually serviced a copy. A small `Copy` enum so a trace
/// test can assert exactly which provider won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardProviderKind {
    Osc52,
    Platform(&'static str),
    TempFile,
}

/// A copy request from a call site.
#[derive(Debug, Clone)]
pub(crate) struct CopyRequest<'a> {
    /// The text to place on the clipboard.
    pub payload: &'a str,
    /// Number of logical lines, for the "copied N lines" status.
    pub lines: usize,
    /// Whether the user has already confirmed a large write (privacy gate).
    pub confirmed: bool,
    /// Human label for the copied content, e.g. "assistant message".
    pub label: &'a str,
}

impl<'a> CopyRequest<'a> {
    /// Convenience constructor that derives `lines` from the payload and
    /// leaves the write unconfirmed.
    pub(crate) fn new(payload: &'a str, label: &'a str) -> Self {
        let lines = payload.lines().count().max(1);
        Self {
            payload,
            lines,
            confirmed: false,
            label,
        }
    }
}

/// The outcome of a copy attempt, ready to drive both the status line and a
/// toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    /// A provider accepted the payload.
    Copied {
        provider: ClipboardProviderKind,
        lines: usize,
        bytes: usize,
    },
    /// All clipboard providers failed; the payload was written to disk.
    WroteTempFile { path: PathBuf, bytes: usize },
    /// The payload exceeds `confirm_threshold` and the caller has not yet
    /// confirmed. No provider was attempted (privacy control).
    NeedsConfirmation { bytes: usize },
    /// Every provider, including the temp-file fallback, failed. Carries the
    /// exact reason from the last provider tried — never silent.
    Failed { reason: String },
}

impl CopyOutcome {
    /// Status-line text for this outcome.
    pub(crate) fn status_message(&self) -> String {
        match self {
            CopyOutcome::Copied { lines, .. } => {
                if *lines == 1 {
                    "copied 1 line".to_string()
                } else {
                    format!("copied {lines} lines")
                }
            }
            CopyOutcome::WroteTempFile { path, .. } => {
                format!("wrote {}", path.display())
            }
            CopyOutcome::NeedsConfirmation { bytes } => {
                format!("copy of {bytes} bytes needs confirmation")
            }
            CopyOutcome::Failed { reason } => format!("copy failed: {reason}"),
        }
    }

    /// Toast message and variant for this outcome.
    ///
    /// - `Copied` → success
    /// - `WroteTempFile` → warning (a fallback fired)
    /// - `Failed` → error (never silent)
    /// - `NeedsConfirmation` → info (the caller should prompt)
    pub(crate) fn toast(&self) -> (String, ToastVariant) {
        match self {
            CopyOutcome::Copied { .. } => (self.status_message(), ToastVariant::Success),
            CopyOutcome::WroteTempFile { .. } => (self.status_message(), ToastVariant::Warning),
            CopyOutcome::NeedsConfirmation { .. } => (self.status_message(), ToastVariant::Info),
            CopyOutcome::Failed { .. } => (self.status_message(), ToastVariant::Error),
        }
    }
}

/// Ordered clipboard provider chain over an injectable [`ClipboardSink`].
pub(crate) struct ClipboardChain<S: ClipboardSink> {
    sink: S,
    /// Ordered providers: OSC 52 (if capable) → platform commands → temp file.
    providers: Vec<ClipboardProvider>,
    /// Configurable OSC 52 payload cap (defaults to [`DEFAULT_OSC52_MAX_BYTES`]).
    osc52_max_bytes: usize,
    /// Retained, INERT toggle. It once enabled a wire-level "chunked" OSC 52
    /// write for oversized payloads, but that emitted one escape the terminal
    /// still dropped while falsely reporting success. `try_osc52` now ignores
    /// it: an over-cap payload always fails over to the next provider whether
    /// this is set or not. Kept only so callers (and the regression test) can
    /// still toggle it without changing the now-correct fall-through behavior.
    osc52_chunk: bool,
    /// When set, any payload larger than this requires `request.confirmed`.
    confirm_threshold: Option<usize>,
}

impl<S: ClipboardSink> ClipboardChain<S> {
    /// Build a chain with an explicit provider list (mainly for tests).
    pub(crate) fn with_providers(sink: S, providers: Vec<ClipboardProvider>) -> Self {
        Self {
            sink,
            providers,
            osc52_max_bytes: DEFAULT_OSC52_MAX_BYTES,
            osc52_chunk: false,
            confirm_threshold: None,
        }
    }

    /// Build the default chain from resolved capabilities and platform
    /// command candidates: OSC 52 first (only if `caps.osc52`), then each
    /// platform command, then the temp-file fallback (always last).
    pub(crate) fn default_chain(
        sink: S,
        caps: ClipboardCapabilities,
        platform: Vec<PlatformCommand>,
    ) -> Self {
        let mut providers = Vec::new();
        if caps.osc52 {
            providers.push(ClipboardProvider::Osc52);
        }
        for cmd in platform {
            providers.push(ClipboardProvider::PlatformCommand(cmd));
        }
        providers.push(ClipboardProvider::TempFile);
        Self::with_providers(sink, providers)
    }

    pub(crate) fn set_osc52_max_bytes(&mut self, max: usize) -> &mut Self {
        self.osc52_max_bytes = max;
        self
    }

    /// INERT toggle, retained for API/test compatibility. See the `osc52_chunk`
    /// field doc: an over-cap OSC 52 payload always falls through to the next
    /// provider now, so this no longer changes behavior.
    pub(crate) fn set_osc52_chunk(&mut self, chunk: bool) -> &mut Self {
        self.osc52_chunk = chunk;
        self
    }

    pub(crate) fn set_confirm_threshold(&mut self, threshold: Option<usize>) -> &mut Self {
        self.confirm_threshold = threshold;
        self
    }

    /// Borrow the underlying sink (test introspection).
    pub(crate) fn sink(&self) -> &S {
        &self.sink
    }

    /// The single entry point. Walks the provider chain in order, returning on
    /// the first success; on each failure it records the reason and falls
    /// through. Enforces the confirmation gate before touching any provider.
    pub(crate) fn copy(&mut self, request: &CopyRequest<'_>) -> CopyOutcome {
        let bytes = request.payload.len();

        // Privacy gate: never write a large payload to the clipboard without
        // an explicit confirmation. Checked before any provider is attempted.
        if let Some(threshold) = self.confirm_threshold
            && bytes > threshold
            && !request.confirmed
        {
            return CopyOutcome::NeedsConfirmation { bytes };
        }

        let providers = self.providers.clone();
        let mut last_reason = "no clipboard provider available (chain was empty)".to_string();

        for provider in &providers {
            match provider {
                ClipboardProvider::Osc52 => match self.try_osc52(request.payload) {
                    Ok(()) => {
                        return CopyOutcome::Copied {
                            provider: ClipboardProviderKind::Osc52,
                            lines: request.lines,
                            bytes,
                        };
                    }
                    Err(reason) => last_reason = reason,
                },
                ClipboardProvider::PlatformCommand(cmd) => {
                    match self.try_platform(*cmd, request.payload.as_bytes()) {
                        Ok(()) => {
                            return CopyOutcome::Copied {
                                provider: ClipboardProviderKind::Platform(cmd.program),
                                lines: request.lines,
                                bytes,
                            };
                        }
                        Err(reason) => last_reason = reason,
                    }
                }
                ClipboardProvider::TempFile => match self.try_temp_file(request) {
                    Ok(path) => return CopyOutcome::WroteTempFile { path, bytes },
                    Err(reason) => last_reason = reason,
                },
            }
        }

        CopyOutcome::Failed {
            reason: last_reason,
        }
    }

    /// Attempt the OSC 52 provider. Reuses the existing pure base64 encoder.
    fn try_osc52(&mut self, payload: &str) -> Result<(), String> {
        let encoded = base64_encode(payload.as_bytes());

        if encoded.len() <= self.osc52_max_bytes {
            // Fits the cap: a single classic `ESC ] 52 ; c ; <b64> BEL`.
            let seq = format!("\x1b]52;c;{encoded}\x07");
            return self
                .sink
                .write_terminal(seq.as_bytes())
                .map_err(|err| format!("terminal clipboard write failed: {err}"));
        }

        // Over the cap: never claim success. A wire-level split would still emit
        // one oversized `ESC ] 52 ; c ; <b64> ST` sequence that the terminal (or
        // tmux/SSH `set-clipboard` buffer) drops, so chunking is not a real
        // protocol here. Fail so the chain falls through to the platform-command
        // / temp-file providers instead of silently dropping the copy.
        Err(format!(
            "payload {} bytes exceeds terminal clipboard cap of {} bytes",
            encoded.len(),
            self.osc52_max_bytes,
        ))
    }

    /// Attempt one platform clipboard command.
    fn try_platform(&mut self, cmd: PlatformCommand, payload: &[u8]) -> Result<(), String> {
        match self.sink.run_command(cmd.program, cmd.args, payload) {
            Ok(outcome) if outcome.success => Ok(()),
            Ok(outcome) => {
                let code = outcome
                    .status_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let stderr = if outcome.stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", outcome.stderr)
                };
                Err(format!("{} exited with status {code}{stderr}", cmd.program))
            }
            Err(err) => Err(format!("{} failed to run: {err}", cmd.program)),
        }
    }

    /// Attempt the temp-file fallback.
    fn try_temp_file(&mut self, request: &CopyRequest<'_>) -> Result<PathBuf, String> {
        let name = sanitize_label(request.label);
        self.sink
            .write_temp_file(request.payload.as_bytes(), &name)
            .map_err(|err| format!("temp-file fallback failed: {err}"))
    }
}

/// Turn a free-form label into a filesystem-safe temp-file suffix.
fn sanitize_label(label: &str) -> String {
    let mut out: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if out.is_empty() {
        out.push_str("copy");
    }
    out.push_str(".txt");
    out
}

// ---------------------------------------------------------------------------
// Recording sink (test seam)
// ---------------------------------------------------------------------------

// Re-exported for both the in-crate `#[cfg(test)]` suite and any
// `feature = "testing"` consumer. Under `testing` alone (no `test`) the
// in-crate tests that consume these are not compiled, so allow the otherwise
// "unused" re-export rather than splitting the cfg.
#[cfg(any(test, feature = "testing"))]
#[allow(unused_imports)]
pub(crate) use recording::{CommandScript, RecordingSink, SinkCall, SinkScript};

#[cfg(any(test, feature = "testing"))]
mod recording {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A recorded sink invocation, capturing which method ran and its bytes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum SinkCall {
        Terminal {
            bytes: Vec<u8>,
        },
        Command {
            program: String,
            args: Vec<String>,
            payload: Vec<u8>,
        },
        TempFile {
            payload: Vec<u8>,
            suggested_name: String,
        },
    }

    /// Scripts how a [`RecordingSink`] answers each method. By default all
    /// three succeed; flip a field to make that method fail so a test can
    /// drive any fall-through path.
    #[derive(Debug, Clone)]
    pub(crate) struct SinkScript {
        /// `Err` message for `write_terminal`, or `None` to succeed.
        pub terminal_error: Option<String>,
        /// Outcome for `run_command`: `Ok(false)` = non-zero exit,
        /// `Err` = spawn failure, `None` = success.
        pub command_outcome: CommandScript,
        /// `Err` message for `write_temp_file`, or `None` to succeed and
        /// return a synthetic path under this directory stem.
        pub temp_file_error: Option<String>,
        /// The directory the synthetic temp path is rooted at.
        pub temp_dir: PathBuf,
    }

    /// How the recording sink should answer `run_command`.
    #[derive(Debug, Clone)]
    pub(crate) enum CommandScript {
        /// Succeed (zero exit).
        Success,
        /// Non-zero exit with this status code and stderr.
        Exit { code: i32, stderr: String },
        /// Spawn/IO failure with this message.
        SpawnError(String),
    }

    impl Default for SinkScript {
        fn default() -> Self {
            Self {
                terminal_error: None,
                command_outcome: CommandScript::Success,
                temp_file_error: None,
                temp_dir: PathBuf::from("/tmp/squeezy-test"),
            }
        }
    }

    /// Test [`ClipboardSink`] that records every call and replays scripted
    /// outcomes. Records into an `Arc<Mutex<Vec<SinkCall>>>` so a test can
    /// assert which provider was attempted, in what order, and the exact
    /// bytes — with no real clipboard, terminal, or subprocess.
    #[derive(Debug, Clone)]
    pub(crate) struct RecordingSink {
        calls: Arc<Mutex<Vec<SinkCall>>>,
        script: SinkScript,
    }

    impl RecordingSink {
        pub(crate) fn new() -> Self {
            Self::with_script(SinkScript::default())
        }

        pub(crate) fn with_script(script: SinkScript) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                script,
            }
        }

        /// Snapshot of every recorded call, in order.
        pub(crate) fn calls(&self) -> Vec<SinkCall> {
            self.calls.lock().unwrap().clone()
        }

        /// Shared handle to the recorded calls, so a chain can be moved while
        /// the test retains visibility.
        pub(crate) fn handle(&self) -> Arc<Mutex<Vec<SinkCall>>> {
            Arc::clone(&self.calls)
        }
    }

    impl Default for RecordingSink {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ClipboardSink for RecordingSink {
        fn write_terminal(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.calls.lock().unwrap().push(SinkCall::Terminal {
                bytes: bytes.to_vec(),
            });
            match &self.script.terminal_error {
                None => Ok(()),
                Some(msg) => Err(io::Error::other(msg.clone())),
            }
        }

        fn run_command(
            &mut self,
            program: &str,
            args: &[&str],
            payload: &[u8],
        ) -> io::Result<CommandOutcome> {
            self.calls.lock().unwrap().push(SinkCall::Command {
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                payload: payload.to_vec(),
            });
            match &self.script.command_outcome {
                CommandScript::Success => Ok(CommandOutcome {
                    status_code: Some(0),
                    success: true,
                    stderr: String::new(),
                }),
                CommandScript::Exit { code, stderr } => Ok(CommandOutcome {
                    status_code: Some(*code),
                    success: false,
                    stderr: stderr.clone(),
                }),
                CommandScript::SpawnError(msg) => {
                    Err(io::Error::new(io::ErrorKind::NotFound, msg.clone()))
                }
            }
        }

        fn write_temp_file(&mut self, payload: &[u8], suggested_name: &str) -> io::Result<PathBuf> {
            self.calls.lock().unwrap().push(SinkCall::TempFile {
                payload: payload.to_vec(),
                suggested_name: suggested_name.to_string(),
            });
            match &self.script.temp_file_error {
                None => {
                    let mut path = self.script.temp_dir.clone();
                    path.push(suggested_name);
                    Ok(path)
                }
                Some(msg) => Err(io::Error::other(msg.clone())),
            }
        }
    }
}

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod tests;
