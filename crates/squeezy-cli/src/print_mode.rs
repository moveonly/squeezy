//! Helpers that desugar `--prompt` arguments into the ordered sequence of
//! user prompts run by print mode.
//!
//! Print mode accepts three layered input shapes — repeated `--prompt`
//! arguments, piped stdin, and `@path` file mentions — and this module
//! resolves them into a stable list of prompt strings. The resolver is
//! split out so it can be tested without spawning an LLM provider or a
//! sub-process.

use std::{
    fs,
    io::{self, IsTerminal, Read},
    path::Path,
};

use squeezy_core::SqueezyError;

/// Resolved print-mode prompt body plus the per-prompt exclude-from-context
/// flag derived from the `!!` prefix.
///
/// The double-bang escape gives print mode an "execute but do not feed
/// the output back into the LLM transcript" semantic, matching the TUI
/// bang-bang shortcut. The user can run `squeezy --prompt "!!cmd"` to
/// run a side check without polluting the conversation.
///
/// Today the runtime that actually executes `cmd` lives behind the
/// F01-exclude-from-context-bang-bang sibling work; until that lands, the
/// parser still recognises the prefix here and the print-mode pump
/// announces the deferral instead of forwarding `!!cmd` verbatim into the
/// agent transcript. That keeps the flag's user-visible promise — "this
/// prompt does not pollute the conversation context" — intact even on the
/// minimum surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptInput {
    pub content: String,
    pub exclude_from_context: bool,
}

/// Strip the optional `!!` exclude-from-context prefix from a resolved
/// prompt body and return a structured input that captures both the
/// content and the flag.
///
/// Detection is intentionally narrow: only a literal `"!!"` at the very
/// start of the resolved prompt counts. A single leading `!` is **not**
/// recognised — Squeezy does not yet expose an "include-in-context"
/// bash escape as a top-level shortcut, and recognising it here would
/// otherwise collide with prompts that legitimately begin with an
/// exclamation mark (e.g. "!important: …"). Whitespace before the
/// double-bang is preserved as content so callers can't accidentally
/// hide the prefix behind a leading newline.
pub(crate) fn classify_prompt(content: String) -> PromptInput {
    if let Some(rest) = content.strip_prefix("!!") {
        PromptInput {
            content: rest.to_string(),
            exclude_from_context: true,
        }
    } else {
        PromptInput {
            content,
            exclude_from_context: false,
        }
    }
}

/// Resolve repeated `--prompt` values plus any piped stdin into the
/// ordered list of prompts that print mode should dispatch.
///
/// Rules:
/// - Plain `--prompt <text>` is appended verbatim.
/// - `--prompt @path` reads `path` as utf-8 text via `read_file`; binary
///   or image-typed files are rejected by the caller-provided reader.
/// - `--prompt -` consumes piped stdin once at that position. The reader
///   is invoked at most one time across the whole resolution and the
///   cached value is reused for any later `-` placement.
/// - When stdin is piped and is *not* consumed explicitly via `-`, its
///   contents are prepended to the first prompt with a blank-line
///   separator; if no `--prompt` values were supplied, the stdin content
///   becomes the sole prompt.
///
/// `read_stdin` and `read_file` are taken as closures so the unit tests
/// can drive the resolver with deterministic inputs without touching the
/// real filesystem or the real stdin.
pub(crate) fn resolve_prompt_inputs<R, F>(
    values: &[String],
    stdin_is_tty: bool,
    mut read_stdin: R,
    mut read_file: F,
) -> Result<Vec<String>, SqueezyError>
where
    R: FnMut() -> Result<String, SqueezyError>,
    F: FnMut(&Path) -> Result<String, SqueezyError>,
{
    let stdin_piped = !stdin_is_tty;
    let mut prompts: Vec<String> = Vec::with_capacity(values.len() + 1);
    let mut stdin_cache: Option<String> = None;
    let mut stdin_consumed_explicitly = false;

    for value in values {
        if value == "-" {
            if !stdin_piped {
                return Err(SqueezyError::Config(
                    "--prompt -: stdin is a TTY; pipe content into squeezy to use stdin as a prompt"
                        .to_string(),
                ));
            }
            if stdin_cache.is_none() {
                stdin_cache = Some(read_stdin()?);
            }
            stdin_consumed_explicitly = true;
            prompts.push(stdin_cache.clone().expect("stdin cache populated above"));
        } else if let Some(rest) = value.strip_prefix('@') {
            if rest.is_empty() {
                return Err(SqueezyError::Config(
                    "--prompt @: missing file path after '@'".to_string(),
                ));
            }
            prompts.push(read_file(Path::new(rest))?);
        } else {
            prompts.push(value.clone());
        }
    }

    if stdin_piped && !stdin_consumed_explicitly {
        if stdin_cache.is_none() {
            stdin_cache = Some(read_stdin()?);
        }
        let content = stdin_cache.expect("stdin cache populated above");
        if !content.trim().is_empty() {
            if let Some(first) = prompts.first_mut() {
                if first.is_empty() {
                    *first = content;
                } else {
                    *first = format!("{content}\n\n{first}");
                }
            } else {
                prompts.push(content);
            }
        }
    }

    Ok(prompts)
}

/// Read a `--prompt @path` target as utf-8 text wrapped in a `<file>`
/// envelope so the model sees the source path alongside the content.
/// Image-typed paths are rejected because Squeezy does not yet support
/// the multimodal envelope pi attaches for them.
pub(crate) fn read_prompt_file(path: &Path) -> Result<String, SqueezyError> {
    if let Some(ext) = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        && is_image_extension(&ext)
    {
        return Err(SqueezyError::Config(format!(
            "--prompt @{}: image attachments are not yet supported; Squeezy has no multimodal envelope yet",
            path.display()
        )));
    }
    let content = fs::read_to_string(path).map_err(|err| {
        SqueezyError::Config(format!(
            "--prompt @{}: failed to read file: {err}",
            path.display()
        ))
    })?;
    Ok(format!(
        "<file path=\"{}\">\n{}\n</file>",
        path.display(),
        content
    ))
}

/// Extensions that pi treats as multimodal image attachments. Squeezy
/// rejects these in `--prompt @path` until the LLM input envelope can
/// carry an image part — see `LlmInputItem` in `crates/squeezy-llm/src/lib.rs`.
pub(crate) fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "tif"
            | "tiff"
            | "ico"
            | "heic"
            | "heif"
            | "avif"
            | "svg"
    )
}

/// Drain piped stdin into a `String`. Callers should only invoke this
/// when [`stdin_is_tty`] reports `false`; otherwise it will block waiting
/// for an EOF that an interactive terminal never sends.
pub(crate) fn read_stdin_to_string() -> Result<String, SqueezyError> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|err| SqueezyError::Config(format!("failed to read stdin: {err}")))?;
    Ok(buf)
}

pub(crate) fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

#[cfg(test)]
#[path = "print_mode_tests.rs"]
mod tests;
