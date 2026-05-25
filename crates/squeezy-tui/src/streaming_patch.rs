//! Streaming previewer for `apply_patch` tool arguments.
//!
//! Provider adapters do not yet emit `tool_call_arguments_delta` events to the
//! TUI, but the parser is built and unit-tested so wiring becomes a single
//! "call `push_delta` from the agent stream" change once that surface exists.
//! The contract is intentionally narrow: feed bytes via [`push_delta`], get a
//! cumulative list of [`PatchPreviewEvent`]s as patch objects close.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Incremental events the TUI can paint while patch JSON is still streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchPreviewEvent {
    /// A single patch object has finished parsing inside the `patches` array.
    Patch {
        index: usize,
        path: String,
        search_hash: String,
        replace_hash: String,
    },
    /// The `patches` array has closed; no further patch objects will appear.
    Complete { count: usize },
}

/// Push-driven JSON parser that recognises
/// `{"patches": [ { ... }, { ... }, ... ]}` shaped tool-call arguments and
/// emits a [`PatchPreviewEvent::Patch`] each time a patch object closes.
///
/// Escaped quotes inside string literals are handled, so a `search` or
/// `replace` body containing `\"` does not falsely close a sub-string.
#[derive(Debug, Default)]
pub struct JsonPatchPreviewParser {
    state: ParserState,
    buf: String,
    object_start: Option<usize>,
    in_string: bool,
    escape: bool,
    depth: usize,
    patches_array_depth: Option<usize>,
    emitted: usize,
    completed: bool,
}

#[derive(Debug, Default)]
enum ParserState {
    #[default]
    Searching,
    AwaitingPatchesArray,
    InsidePatchesArray,
    Done,
}

#[derive(Deserialize)]
struct PatchShape {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    replace: Option<String>,
}

impl JsonPatchPreviewParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of streamed tool-call arguments. Returns every
    /// event that became known after this delta — usually empty, occasionally
    /// one [`PatchPreviewEvent::Patch`] (a patch object just closed) or a
    /// [`PatchPreviewEvent::Complete`] (the `patches` array closed).
    pub fn push_delta(&mut self, delta: &str) -> Vec<PatchPreviewEvent> {
        let mut events = Vec::new();
        for byte in delta.bytes() {
            self.push_byte(byte, &mut events);
        }
        events
    }

    /// Signal end-of-stream. Emits a `Complete` event if the parser saw a
    /// `patches` array open but never saw the matching close (truncated JSON,
    /// canceled call, etc.) — the count reflects however many patch objects
    /// completed before truncation.
    pub fn finish(&mut self) -> Vec<PatchPreviewEvent> {
        let mut events = Vec::new();
        if !self.completed && self.patches_array_depth.is_some() {
            events.push(PatchPreviewEvent::Complete {
                count: self.emitted,
            });
            self.completed = true;
            self.state = ParserState::Done;
        }
        events
    }

    /// Number of patch events emitted so far.
    pub fn emitted_count(&self) -> usize {
        self.emitted
    }

    fn push_byte(&mut self, byte: u8, events: &mut Vec<PatchPreviewEvent>) {
        if matches!(self.state, ParserState::Done) {
            return;
        }
        let ch = byte as char;
        self.buf.push(ch);
        if self.in_string {
            if self.escape {
                self.escape = false;
            } else if ch == '\\' {
                self.escape = true;
            } else if ch == '"' {
                self.in_string = false;
            }
            return;
        }
        match ch {
            '"' => {
                self.in_string = true;
            }
            '{' | '[' => {
                self.depth += 1;
                if matches!(self.state, ParserState::AwaitingPatchesArray) && ch == '[' {
                    self.state = ParserState::InsidePatchesArray;
                    self.patches_array_depth = Some(self.depth);
                } else if matches!(self.state, ParserState::InsidePatchesArray)
                    && ch == '{'
                    && self.object_start.is_none()
                    && self.patches_array_depth == Some(self.depth - 1)
                {
                    // First byte inside a new top-level patch object — record
                    // the position so we can extract the full JSON when the
                    // matching '}' arrives.
                    self.object_start = Some(self.buf.len() - 1);
                }
            }
            '}' | ']' => {
                if matches!(self.state, ParserState::InsidePatchesArray)
                    && ch == '}'
                    && let Some(start) = self.object_start
                    && self.patches_array_depth == Some(self.depth - 1)
                {
                    let slice = &self.buf[start..self.buf.len()];
                    if let Ok(shape) = serde_json::from_str::<PatchShape>(slice) {
                        let path = shape.path.unwrap_or_default();
                        let search_hash = sha256_short(shape.search.as_deref().unwrap_or_default());
                        let replace_hash =
                            sha256_short(shape.replace.as_deref().unwrap_or_default());
                        events.push(PatchPreviewEvent::Patch {
                            index: self.emitted,
                            path,
                            search_hash,
                            replace_hash,
                        });
                        self.emitted += 1;
                    }
                    self.object_start = None;
                }
                if self.depth == 0 {
                    // Unbalanced — bail out.
                    self.state = ParserState::Done;
                    return;
                }
                self.depth -= 1;
                if ch == ']'
                    && matches!(self.state, ParserState::InsidePatchesArray)
                    && Some(self.depth) == self.patches_array_depth.map(|d| d - 1)
                {
                    events.push(PatchPreviewEvent::Complete {
                        count: self.emitted,
                    });
                    self.completed = true;
                    self.state = ParserState::Done;
                }
            }
            _ => {}
        }
        // Keyword detection: look for `"patches"` followed by a `:` before any
        // unrelated structural token. We do this by scanning the recent
        // suffix; cheap because the buffer is bounded by the streamed args.
        if matches!(self.state, ParserState::Searching) && ends_with_patches_key(&self.buf) {
            self.state = ParserState::AwaitingPatchesArray;
        }
    }
}

fn ends_with_patches_key(buf: &str) -> bool {
    // Match `"patches":` (optionally with whitespace before `:`). We scan from
    // the tail to keep this O(1) per byte.
    let trimmed = buf.trim_end();
    let trimmed = trimmed.trim_end_matches(':');
    let trimmed = trimmed.trim_end();
    trimmed.ends_with("\"patches\"")
}

fn sha256_short(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let bytes = hasher.finalize();
    // Short prefix: enough to distinguish previews, cheap to render.
    let mut out = String::with_capacity(16);
    for byte in bytes.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}
