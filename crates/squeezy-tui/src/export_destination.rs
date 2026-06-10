//! Export destinations (§12.6.4).
//!
//! The single `/export` flow renders the transcript through the shared copy
//! formatters (see [`crate::copy`]) and then hands the rendered payload to one
//! of several *destinations*: an explicit file path, the clipboard, a
//! "stdout"-style transcript echo, or a configured directory under the
//! workspace. This module owns the *pure* destination grammar — parsing the
//! `/export <format> [destination]` argument tail into a typed
//! [`ExportDestination`] plus the [`CopyFormat`] — so the wiring in `lib.rs`
//! stays a thin dispatch over an exhaustively-matched enum.
//!
//! Keeping the parser pure (no `TuiApp`, no filesystem) is what lets the
//! destination semantics — including path-traversal rejection — be unit-tested
//! in isolation, and lets `lib.rs` reuse the existing atomic-write /
//! `deliver_copy` pipelines unchanged.

use crate::copy::CopyFormat;

/// Where a `/export` payload should be delivered.
///
/// Variants mirror the destinations called out in the §12.6.4 spec that are
/// meaningful inside the alt-screen TUI: an explicit file, the clipboard, a
/// stdout-style transcript echo, and a configured workspace directory. The
/// session-storage default (no destination token) is [`ExportDestination::Default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExportDestination {
    /// No destination token: write under session storage
    /// (`<workspace>/.squeezy/exports/<session>/transcript-<ts>.<ext>`).
    Default,
    /// An explicit file path (relative paths resolve against the workspace
    /// root). Carries the verbatim user-supplied path so interior whitespace is
    /// preserved (`/export md ./my notes.md`).
    File(String),
    /// The clipboard, via the same provider chain semantic copies use.
    Clipboard,
    /// A stdout-style echo: the payload is pushed into the transcript as a
    /// system item so it lands in the terminal (and the clean-exit scrollback
    /// mirror) rather than being silently swallowed by the alt screen.
    Stdout,
    /// A configured directory under the workspace
    /// (`<workspace>/<dir>/transcript-<ts>.<ext>`). The directory name is
    /// validated (no traversal, no absolute escape) at parse time.
    ConfiguredDir(String),
}

impl ExportDestination {
    /// Short human label for status/toast/transcript lines. Currently exercised
    /// by tests (the production status strings are hand-written per arm); kept
    /// as the single source of truth for a destination's display name.
    #[cfg(test)]
    pub(crate) fn label(&self) -> &'static str {
        match self {
            ExportDestination::Default => "session storage",
            ExportDestination::File(_) => "file",
            ExportDestination::Clipboard => "clipboard",
            ExportDestination::Stdout => "stdout",
            ExportDestination::ConfiguredDir(_) => "configured directory",
        }
    }
}

/// A fully-parsed `/export` invocation: the render format plus the resolved
/// destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportRequest {
    pub(crate) format: CopyFormat,
    pub(crate) destination: ExportDestination,
}

/// Shared usage hint for `/export`. Lists every destination keyword so the
/// error path is self-documenting.
pub(crate) const EXPORT_USAGE: &str =
    "usage: /export <md|txt|json> [clipboard|stdout|dir:<name>|<path>]";

/// Parse `/export <md|txt|json> [destination]`.
///
/// The first token is the (required) format. Any remaining text is the
/// destination:
///
/// * empty                         → [`ExportDestination::Default`]
/// * `clipboard` / `clip`          → [`ExportDestination::Clipboard`]
/// * `stdout` / `-`                → [`ExportDestination::Stdout`]
/// * `dir:<name>`                  → [`ExportDestination::ConfiguredDir`]
/// * anything else (verbatim tail) → [`ExportDestination::File`]
///
/// The destination keywords are matched case-insensitively on the *whole*
/// remaining tail, so they never shadow a real file (`/export md clipboard.md`
/// is a file, `/export md clipboard` is the clipboard). A `dir:` name is
/// validated against path traversal so `/export` can never be coaxed into
/// writing outside the workspace via a configured directory; an explicit
/// `File` path is *not* rejected here (absolute paths are a deliberate,
/// documented capability of the path form), matching the pre-existing
/// `/export <fmt> <path>` behavior.
pub(crate) fn parse_export_request(rest: &str) -> Result<ExportRequest, String> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err(EXPORT_USAGE.to_string());
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let format_token = parts.next().unwrap_or_default();
    let format = CopyFormat::from_token(format_token)
        .ok_or_else(|| format!("unknown export format {format_token:?}. {EXPORT_USAGE}"))?;

    let tail = parts.next().map(str::trim).unwrap_or_default();
    let destination = parse_destination(tail)?;
    Ok(ExportRequest {
        format,
        destination,
    })
}

/// Resolve the destination tail (already format-stripped and trimmed) into an
/// [`ExportDestination`]. Pulled out so the keyword/traversal rules can be
/// exercised directly in unit tests.
fn parse_destination(tail: &str) -> Result<ExportDestination, String> {
    if tail.is_empty() {
        return Ok(ExportDestination::Default);
    }
    // Case-insensitive keyword match on the *whole* tail so `clipboard.md`
    // (a file) is never mistaken for the clipboard keyword.
    let lowered = tail.to_ascii_lowercase();
    match lowered.as_str() {
        "clipboard" | "clip" => return Ok(ExportDestination::Clipboard),
        "stdout" | "-" => return Ok(ExportDestination::Stdout),
        _ => {}
    }
    if let Some(name) = tail.strip_prefix("dir:") {
        let name = name.trim();
        validate_configured_dir(name)?;
        return Ok(ExportDestination::ConfiguredDir(name.to_string()));
    }
    // Everything else is a file path, preserved verbatim (interior whitespace
    // intact) for the existing workspace-relative atomic-write path.
    Ok(ExportDestination::File(tail.to_string()))
}

/// Reject a configured-directory name that is empty, absolute, or escapes the
/// workspace via `..`. Returns the validated trimmed name on success.
///
/// This is deliberately stricter than the `File` path form: `dir:` is sold as
/// a *workspace-relative configured directory*, so an absolute root or a
/// parent-escape would violate that contract (and the spec's path-traversal
/// rejection requirement). A bare `.` is also rejected because it implies the
/// caller meant the session-default destination, not a named directory.
fn validate_configured_dir(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("export dir name is empty. {EXPORT_USAGE}"));
    }
    let path = std::path::Path::new(name);
    if path.is_absolute() {
        return Err(format!(
            "export dir {name:?} must be a workspace-relative directory, not an absolute path"
        ));
    }
    // Windows drive-relative / UNC prefixes (`C:`, `\\server`) also count as
    // "not workspace-relative"; `Component::Prefix` covers them on Windows and
    // is simply never produced on Unix.
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(format!(
                    "export dir {name:?} must not escape the workspace with `..`"
                ));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!(
                    "export dir {name:?} must be a workspace-relative directory"
                ));
            }
            std::path::Component::CurDir | std::path::Component::Normal(_) => {}
        }
    }
    // A name that normalizes to nothing but `.` (e.g. `.` or `./.`) carries no
    // real directory segment; treat it as a usage error rather than silently
    // aliasing the workspace root.
    let has_real_segment = path
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(_)));
    if !has_real_segment {
        return Err(format!(
            "export dir {name:?} resolves to no directory. {EXPORT_USAGE}"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "export_destination_tests.rs"]
mod tests;
