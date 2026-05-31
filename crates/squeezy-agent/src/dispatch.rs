//! Typed slash-command dispatch.
//!
//! Every slash command surfaced by the TUI is also reachable via the
//! typed [`DispatchCommand`] enum so non-TUI drivers (RPC, eval) can
//! invoke the same set of commands without re-implementing string
//! parsing. The TUI's runtime handler is the parser + outcome
//! renderer; the structured dispatch lives on [`crate::Agent`] (see
//! `Agent::dispatch_command`).
//!
//! Goals:
//!
//! - Lossless round-trip: every slash command in
//!   `squeezy-tui/src/input.rs` (`SLASH_COMMANDS`) maps to exactly one
//!   `DispatchCommand` variant and back.
//! - Agent-only behaviour: variants whose action lives wholly inside
//!   `Agent` perform the work in `Agent::dispatch_command` and return a
//!   structured outcome. Variants whose action requires TUI state
//!   (overlays, transcript pushes, etc.) round-trip through
//!   `DispatchOutcome::TuiOnly { kind }` so the TUI can run its
//!   existing helper while RPC drivers see a structured value.
//! - Stable identifiers: variant kinds (`DispatchCommandKind`) are
//!   `serde(rename_all = "kebab-case")` so they look like the slash
//!   commands they represent (`compact`, `task-cancel`, `session-export-html`).
//!
//! Parsing semantics:
//!
//! - Input must start with a `/` head; otherwise [`DispatchCommandParseError::NotASlashCommand`].
//! - Unknown heads produce [`DispatchCommandParseError::Unknown`].
//! - Missing-required-arg errors surface as
//!   [`DispatchCommandParseError::Usage`] with a one-line hint mirroring
//!   the TUI's current `usage:` strings so behaviour is unchanged.

use std::fmt;

use serde::{Deserialize, Serialize};
use squeezy_vcs::{DiffSnapshot as VcsDiffSnapshot, RollbackResult as VcsRollbackResult};

/// Typed slash command parsed from a slash-prefixed input string. Each
/// variant matches exactly one entry in `SLASH_COMMANDS` (with `/jobs`,
/// `/job`, `/job-cancel` kept as documented aliases of `/tasks`,
/// `/task`, `/task-cancel`).
///
/// String-only payloads are intentional: UI-specific types like
/// `ConfigSectionId` live in higher crates and are only meaningful to the
/// TUI renderer. Keeping the payloads as `String` lets the dispatch layer
/// stay in `squeezy-agent` without pulling in TUI types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "args", rename_all = "kebab-case")]
pub enum DispatchCommand {
    Help {
        topic: Option<String>,
    },
    Config {
        section: Option<String>,
    },
    Model,
    Permissions,
    Plan {
        prompt: Option<String>,
    },
    Build {
        prompt: Option<String>,
    },
    Plans {
        args: String,
    },
    Cost,
    Context,
    Reviewer,
    Attach {
        path: String,
    },
    Attachments,
    Compact {
        undo: bool,
    },
    Diff,
    Tasks,
    Task {
        id: String,
    },
    TaskCancel {
        id: String,
    },
    Pin {
        target: Option<String>,
    },
    Pins,
    Unpin {
        id: String,
    },
    Feedback {
        args: String,
    },
    Report {
        args: String,
    },
    Sessions,
    Session {
        id: String,
    },
    /// `/session rename <name>` — set the active session's
    /// `display_name`. An empty `name` clears the field so the picker
    /// falls back to the inferred title.
    SessionRename {
        name: String,
    },
    /// `/session label <name>` — append a free-form label to the
    /// active session's `labels` list. Duplicates are suppressed by the
    /// agent so the wire shape stays small.
    SessionLabel {
        name: String,
    },
    Resume {
        id: String,
    },
    Fork,
    SessionExport {
        id: String,
    },
    SessionExportHtml {
        id: String,
        path: Option<String>,
    },
    SessionCleanup {
        args: String,
    },
    Checkpoints,
    Checkpoint {
        id: String,
    },
    Undo,
    RevertTurn {
        group_id: String,
    },
    Effort {
        value: Option<String>,
    },
    Verbosity {
        value: Option<String>,
    },
    ToolVerbosity {
        value: Option<String>,
    },
    Detach {
        id: String,
    },
    Statusline,
    Theme {
        theme: Option<String>,
    },
    Keymap,
    /// `/cheap` — force the next turn onto the provider's small-fast
    /// tier even when the router would not have routed it cheap.
    Cheap,
    /// `/parent` — force the next turn onto the user-configured parent
    /// model, bypassing the router entirely for one turn.
    Parent,
    /// `/router on|off` — toggle session-wide auto-routing. `None`
    /// reports the current state without changing it.
    Router {
        value: Option<String>,
    },
}

impl DispatchCommand {
    /// Canonical slash-name (`"/compact"`, `"/task-cancel"`, …). Used
    /// for echoing back the slash form and for outcome
    /// `Unsupported { command }` payloads.
    pub fn slash_name(&self) -> &'static str {
        match self {
            Self::Help { .. } => "/help",
            Self::Config { .. } => "/config",
            Self::Model => "/model",
            Self::Permissions => "/permissions",
            Self::Plan { .. } => "/plan",
            Self::Build { .. } => "/build",
            Self::Plans { .. } => "/plans",
            Self::Cost => "/cost",
            Self::Context => "/context",
            Self::Reviewer => "/reviewer",
            Self::Attach { .. } => "/attach",
            Self::Attachments => "/attachments",
            Self::Compact { .. } => "/compact",
            Self::Diff => "/diff",
            Self::Tasks => "/tasks",
            Self::Task { .. } => "/task",
            Self::TaskCancel { .. } => "/task-cancel",
            Self::Pin { .. } => "/pin",
            Self::Pins => "/pins",
            Self::Unpin { .. } => "/unpin",
            Self::Feedback { .. } => "/feedback",
            Self::Report { .. } => "/report",
            Self::Sessions => "/sessions",
            Self::Session { .. } => "/session",
            Self::SessionRename { .. } => "/session",
            Self::SessionLabel { .. } => "/session",
            Self::Resume { .. } => "/resume",
            Self::Fork => "/fork",
            Self::SessionExport { .. } => "/session-export",
            Self::SessionExportHtml { .. } => "/session-export-html",
            Self::SessionCleanup { .. } => "/session-cleanup",
            Self::Checkpoints => "/checkpoints",
            Self::Checkpoint { .. } => "/checkpoint",
            Self::Undo => "/undo",
            Self::RevertTurn { .. } => "/revert-turn",
            Self::Effort { .. } => "/effort",
            Self::Verbosity { .. } => "/verbosity",
            Self::ToolVerbosity { .. } => "/tool-verbosity",
            Self::Detach { .. } => "/detach",
            Self::Statusline => "/statusline",
            Self::Theme { .. } => "/theme",
            Self::Keymap => "/keymap",
            Self::Cheap => "/cheap",
            Self::Parent => "/parent",
            Self::Router { .. } => "/router",
        }
    }

    /// Parse a slash-prefixed input into a typed command. The input is
    /// the raw user line — including the leading `/` and any
    /// whitespace-separated arguments.
    ///
    /// Behaviour mirrors the TUI's pre-refactor handler: required-arg
    /// commands return [`DispatchCommandParseError::Usage`] (so the
    /// caller can surface the same usage string as before) and unknown
    /// heads return [`DispatchCommandParseError::Unknown`] so the
    /// caller can keep them as a fall-through user prompt.
    pub fn parse(input: &str) -> Result<Self, DispatchCommandParseError> {
        let mut iter = input.split_whitespace();
        let head = iter.next().ok_or(DispatchCommandParseError::Empty)?;
        if !head.starts_with('/') {
            return Err(DispatchCommandParseError::NotASlashCommand);
        }
        // `rest` preserves the user's interior whitespace so commands
        // like `/help quantum billing` see the full topic verbatim,
        // mirroring `input.strip_prefix(command).map(str::trim)` in
        // the old handler.
        let rest = input.strip_prefix(head).map(str::trim).unwrap_or_default();
        let cmd = match head {
            "/help" => Self::Help {
                topic: none_if_empty(rest),
            },
            "/config" | "/options" => Self::Config {
                section: none_if_empty(rest),
            },
            "/model" => Self::Model,
            "/permissions" => Self::Permissions,
            "/plan" => Self::Plan {
                prompt: none_if_empty(rest),
            },
            "/build" => Self::Build {
                prompt: none_if_empty(rest),
            },
            "/plans" => Self::Plans {
                args: rest.to_string(),
            },
            "/cost" => Self::Cost,
            "/context" => Self::Context,
            "/reviewer" => Self::Reviewer,
            "/attach" => {
                if rest.is_empty() {
                    return Err(DispatchCommandParseError::Usage {
                        command: head.to_string(),
                        hint: "usage: /attach <path>".to_string(),
                    });
                }
                Self::Attach {
                    path: rest.to_string(),
                }
            }
            "/attachments" => Self::Attachments,
            "/compact" => {
                let mut tokens = rest.split_whitespace();
                let undo = matches!(
                    tokens.next().map(str::to_ascii_lowercase).as_deref(),
                    Some("undo")
                );
                Self::Compact { undo }
            }
            "/diff" => Self::Diff,
            "/tasks" => Self::Tasks,
            "/task" => Self::Task {
                id: require_id(head, rest, "<id>")?,
            },
            "/task-cancel" => Self::TaskCancel {
                id: require_id(head, rest, "<id>")?,
            },
            "/pin" => Self::Pin {
                target: first_token(rest),
            },
            "/pins" => Self::Pins,
            "/unpin" => Self::Unpin {
                id: require_id(head, rest, "<pin_id>")?,
            },
            "/feedback" => Self::Feedback {
                args: rest.to_string(),
            },
            "/report" => Self::Report {
                args: rest.to_string(),
            },
            "/sessions" => Self::Sessions,
            "/session" => {
                // `/session rename <name>` and `/session label <name>`
                // mutate the active session's metadata; everything else
                // continues to be `/session <session_id>` (lookup-by-id).
                // `rename` and `label` are reserved subcommands and
                // therefore not usable as raw session ids, which is fine
                // because session ids in the wild are timestamped
                // hex-suffixed slugs (e.g. `session-1700000000-ab12cd`).
                let trimmed = rest.trim_start();
                let (first, remainder) = trimmed
                    .split_once(char::is_whitespace)
                    .unwrap_or((trimmed, ""));
                match first {
                    "rename" => Self::SessionRename {
                        name: remainder.trim().to_string(),
                    },
                    "label" => {
                        let name = remainder.trim().to_string();
                        if name.is_empty() {
                            return Err(DispatchCommandParseError::Usage {
                                command: head.to_string(),
                                hint: "usage: /session label <name>".to_string(),
                            });
                        }
                        Self::SessionLabel { name }
                    }
                    _ => Self::Session {
                        id: require_id(head, rest, "<session_id> | rename <name> | label <name>")?,
                    },
                }
            }
            "/resume" => Self::Resume {
                id: require_id(head, rest, "<session_id>")?,
            },
            "/fork" => Self::Fork,
            "/session-export" => Self::SessionExport {
                id: require_id(head, rest, "<session_id>")?,
            },
            "/session-export-html" => {
                let mut tokens = rest.split_whitespace();
                let id = match tokens.next() {
                    Some(value) => value.to_string(),
                    None => {
                        return Err(DispatchCommandParseError::Usage {
                            command: head.to_string(),
                            hint: "usage: /session-export-html <session_id> [path]".to_string(),
                        });
                    }
                };
                let path = tokens.next().map(str::to_string);
                Self::SessionExportHtml { id, path }
            }
            "/session-cleanup" => Self::SessionCleanup {
                args: rest.to_string(),
            },
            "/checkpoints" => Self::Checkpoints,
            "/checkpoint" => Self::Checkpoint {
                id: require_id(head, rest, "<checkpoint_id>")?,
            },
            "/undo" => Self::Undo,
            "/revert-turn" => Self::RevertTurn {
                group_id: require_id(head, rest, "<turn_id>")?,
            },
            "/effort" => Self::Effort {
                value: first_token(rest),
            },
            "/verbosity" => Self::Verbosity {
                value: first_token(rest),
            },
            "/tool-verbosity" => Self::ToolVerbosity {
                value: first_token(rest),
            },
            "/detach" => Self::Detach {
                id: require_id(head, rest, "<attachment_id>")?,
            },
            "/statusline" => Self::Statusline,
            "/theme" => Self::Theme {
                theme: rest.split_whitespace().next().map(str::to_string),
            },
            "/keymap" => Self::Keymap,
            "/cheap" => Self::Cheap,
            "/parent" => Self::Parent,
            "/router" => Self::Router {
                value: first_token(rest),
            },
            unknown => {
                return Err(DispatchCommandParseError::Unknown {
                    command: unknown.to_string(),
                });
            }
        };
        Ok(cmd)
    }
}

/// Error returned by [`DispatchCommand::parse`]. `Usage` carries the
/// one-line hint the caller should surface so the user sees the same
/// `usage:` text the pre-refactor TUI handler emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCommandParseError {
    Empty,
    NotASlashCommand,
    Unknown { command: String },
    Usage { command: String, hint: String },
}

impl fmt::Display for DispatchCommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty command"),
            Self::NotASlashCommand => write!(f, "expected a slash command"),
            Self::Unknown { command } => write!(f, "unknown slash command: {command}"),
            Self::Usage { hint, .. } => f.write_str(hint),
        }
    }
}

impl std::error::Error for DispatchCommandParseError {}

/// Stable string discriminator for [`DispatchCommand`] — useful for
/// logging and test assertions without holding the full variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DispatchCommandKind(&'static str);

impl DispatchCommandKind {
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DispatchCommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Structured result of [`crate::Agent::dispatch_command`]. Designed
/// to round-trip through serde so non-TUI drivers (eval, RPC) can log
/// the outcome without coupling to TUI types.
///
/// Variants in the `TuiOnly` family represent commands whose effect
/// lives in the TUI renderer (config overlays, transcript pushes,
/// clipboard, …). Agent-side state has already been observed (or not
/// affected) by the time `TuiOnly` is returned; the TUI is responsible
/// for rendering. Non-TUI drivers can treat `TuiOnly` as a no-op or
/// log it for triage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispatchOutcome {
    /// `/compact` (manual compaction) ran. `skipped` is `true` when
    /// the conversation had nothing eligible to compact (empty
    /// session, only the synthetic head, or already maximally
    /// compacted) — `false` when items were actually dropped.
    /// Distinguishing the two lets callers surface a graceful
    /// "nothing to compact yet" status line instead of an error.
    /// Fixes squeezy-kkdb (audit B4).
    Compacted {
        #[serde(default)]
        skipped: bool,
    },
    /// `/compact undo` succeeded — `restored` is `true` when a
    /// checkpoint was found, `false` when there was nothing to undo.
    CompactedUndo { restored: bool },
    /// `/plan` / `/build` mode switch on the agent. `changed` is the
    /// same boolean `Agent::set_session_mode` returns so callers can
    /// distinguish a real switch from a no-op. `prompt` carries the
    /// optional trailing prompt arg (e.g. `/plan analyze this`) back
    /// to the caller so non-TUI dispatchers (RPC, squeezy-eval) can
    /// start a user turn with it; the TUI handler reads the same arg
    /// off `DispatchCommand::Plan { prompt }` directly. Fixes
    /// squeezy-9n9w (audit B3).
    ModeChanged {
        mode: String,
        changed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// `/cost` snapshot. `debug` is the pretty-printed
    /// `SessionAccountingSnapshot` so eval traces stay greppable.
    CostSnapshot { debug: String },
    /// `/context` snapshot. Same shape as `CostSnapshot` — the
    /// session-accounting snapshot covers both.
    ContextSnapshot { debug: String },
    /// `/reviewer` snapshot — number of recent audit entries.
    ReviewerSnapshot { count: usize },
    /// `/tasks` / `/jobs` list — number of background jobs.
    JobsList { count: usize },
    /// `/task <id>` / `/job <id>` — whether the job was found.
    TaskDetail { id: String, found: bool },
    /// `/task-cancel <id>` / `/job-cancel <id>` — whether cancellation
    /// fired.
    TaskCancel { id: String, cancelled: bool },
    /// `/permissions` — session-scoped rule count.
    PermissionsList { count: usize },
    /// `/fork` — id of the newly forked child session.
    Forked { new_session_id: String },
    /// `/sessions` — number of sessions returned by the store.
    SessionsList { count: usize },
    /// `/session <id>` — whether the session exists in the store.
    SessionDetail { session_id: String, exists: bool },
    /// `/session rename <name>` — the active session's `display_name`
    /// was updated. `display_name` is `None` when the user passed an
    /// empty argument to clear the field.
    SessionRenamed {
        session_id: String,
        display_name: Option<String>,
    },
    /// `/session label <name>` — `label` was added to the active
    /// session's `labels` list. `added` is `false` when the label was
    /// already present, so the agent did not rewrite metadata.
    SessionLabelled {
        session_id: String,
        label: String,
        added: bool,
        labels: Vec<String>,
    },
    /// `/session-export <id>` — number of bytes in the JSON export.
    SessionExported { session_id: String, bytes: usize },
    /// `/session-export-html <id> [path]` — bytes written.
    SessionExportedHtml {
        session_id: String,
        path: String,
        bytes: usize,
    },
    /// `/session-cleanup` — archived + removed counts.
    SessionCleanup { archived: usize, removed: usize },
    /// `/attach <path>` — agent-side attach succeeded; `id` is the
    /// attachment id.
    Attached { id: String },
    /// `/detach <id>` — agent-side detach succeeded.
    Detached { id: String },
    /// `/attachments` — number of active attachments.
    AttachmentsList { count: usize },
    /// `/pin` — pin id created.
    Pinned { id: String },
    /// `/unpin` — pin id removed.
    Unpinned { id: String },
    /// `/pins` — number of pinned items.
    PinsList { count: usize },
    /// `/diff` worktree snapshot. Carries the structured
    /// [`squeezy_vcs::DiffSnapshot`] the TUI would render into a card
    /// so a headless driver (eval / RPC) can audit the same payload.
    /// `vcs_kind` is the short tag from the snapshot (`"git"` or
    /// `"none"`) lifted out for easy logging without traversing the
    /// nested `vcs` struct.
    DiffSnapshot {
        vcs_kind: String,
        files_changed: usize,
        additions: u64,
        deletions: u64,
        untracked_files: usize,
        snapshot: Box<VcsDiffSnapshot>,
    },
    /// `/undo` checkpoint rollback. `result` is `None` when the agent
    /// has no checkpoint store wired up (checkpoints disabled);
    /// otherwise it carries the structured
    /// [`squeezy_vcs::RollbackResult`] from
    /// `CheckpointStore::rollback(Latest, …)`. `applied` mirrors
    /// `result.applied` for easy `match` patterns; on a clean tree
    /// with no checkpoint to undo, `applied` is `false` and
    /// `skipped` is `true`.
    CheckpointUndo {
        applied: bool,
        skipped: bool,
        checkpoint_ids: Vec<String>,
        result: Option<Box<VcsRollbackResult>>,
    },
    /// TUI-only directive. The agent has no side effect for this
    /// command; the TUI matches on `command` to dispatch to its
    /// pre-existing handler (overlay toggle, diff job spawn, …).
    TuiOnly { command: String },
    /// A typed slash command landed on a variant that is not yet
    /// wired through the agent — primarily a forward-compat hatch.
    Unsupported { command: String },
    /// Command was recognised but the agent-side action failed.
    Error { command: String, message: String },
}

fn none_if_empty(rest: &str) -> Option<String> {
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn first_token(rest: &str) -> Option<String> {
    rest.split_whitespace().next().map(str::to_string)
}

fn require_id(
    command: &str,
    rest: &str,
    placeholder: &str,
) -> Result<String, DispatchCommandParseError> {
    let value = rest.split_whitespace().next().unwrap_or_default();
    if value.is_empty() {
        return Err(DispatchCommandParseError::Usage {
            command: command.to_string(),
            hint: format!("usage: {command} {placeholder}"),
        });
    }
    Ok(value.to_string())
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
