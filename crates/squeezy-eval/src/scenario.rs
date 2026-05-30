use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::driver::EvalError;
use crate::mock_provider::MockProviderConfig;

/// A full scenario loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub squeezy: SqueezyOverlay,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expect: Expect,
    #[serde(default)]
    pub triage: TriageConfig,
    /// Optional scripted responses used when `[squeezy] provider = "mock"`.
    #[serde(default)]
    pub mock: MockProviderConfig,
    /// Optional TUI render capture. When enabled, the driver emits a
    /// `frames_tui.jsonl` artifact per turn carrying a cell grid +
    /// ANSI re-render of the assistant text as it would appear in the
    /// TUI. Phase 5 of the eval-harness plan.
    #[serde(default)]
    pub tui_capture: TuiCaptureConfig,
    /// Environment variables exported into the agent process before
    /// `Agent::new`. Required for MCP servers that need API keys,
    /// and for `SQUEEZY_PROVIDER`-style overrides. Process-wide
    /// `unsafe` env mutation; eval runs one scenario per process
    /// today, so the blast radius is per-run.
    #[serde(default)]
    pub env_vars: std::collections::BTreeMap<String, String>,
    /// Soft platform pin. When set, the driver records a finding
    /// (`platform_mismatch`) if the host OS doesn't match. Useful
    /// for sandbox-related scenarios that are OS-specific. Values:
    /// `"linux"`, `"macos"`, `"windows"`. Case-insensitive.
    #[serde(default)]
    pub platform: Option<String>,
}

/// Per-scenario TUI render-capture knobs. Empty/default disables the
/// feature (no `frames_tui.jsonl` is written).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuiCaptureConfig {
    /// Enable per-turn TUI render capture. Off by default to keep
    /// existing scenarios cheap.
    #[serde(default)]
    pub enabled: bool,
    /// Cell-grid width to render into. Defaults to 120.
    #[serde(default)]
    pub width: Option<u16>,
    /// Cell-grid height. Defaults to 40.
    #[serde(default)]
    pub height: Option<u16>,
    /// Force a specific palette tone (`"dark"` or `"light"`) so
    /// captures are reproducible regardless of the surrounding
    /// terminal. Defaults to `"dark"`.
    #[serde(default)]
    pub palette_tone: Option<String>,
    /// When true, the driver builds a live `TuiHarness` (TuiApp +
    /// Agent + headless terminal) and routes all agent traffic
    /// through it. This unlocks the `send_key` / `send_keys`
    /// actions and the `tui_*` assertions. Off by default so
    /// existing scenarios pay no cost.
    #[serde(default)]
    pub drive_tui: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceSpec {
    Local {
        #[serde(rename = "local")]
        path: PathBuf,
        /// When true, the workspace is materialized as a per-run snapshot
        /// (git worktree if `<path>/.git` exists, otherwise an
        /// ignore-respecting tree copy) so the agent never reads the
        /// user's in-progress edits.
        #[serde(default)]
        snapshot: bool,
        /// Git ref to snapshot. Defaults to `HEAD`.
        #[serde(default)]
        snapshot_ref: Option<String>,
    },
    Github {
        github: GithubWorkspace,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubWorkspace {
    pub repo: String,
    pub sha: String,
}

/// Overlay applied on top of the resolved [`AppConfig`].
///
/// Every field is optional — anything omitted falls back to whatever
/// `AppConfig::from_env_and_settings` would have produced. This lets a
/// scenario pin specific knobs (provider, model, permission mode) without
/// having to spell out the entire config surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SqueezyOverlay {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub tool_choice: Option<String>,
    pub mode: Option<String>,
    pub permission_mode: Option<String>,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u32>,
    /// Override `AppConfig.max_tool_calls_per_turn` — squeezy's live
    /// per-turn tool-call cap (default 64). Scenarios that probe planner
    /// behavior want this lower (e.g. 6–10) so the budget broker
    /// actually fires.
    pub max_tool_calls_per_turn: Option<u64>,
    /// Override `AppConfig.max_tool_bytes_read_per_turn` (default 20MB).
    pub max_tool_bytes_read_per_turn: Option<u64>,
    /// Override `AppConfig.max_session_cost_usd_micros` (default 5_000_000
    /// = $5). Scenarios that should bail at a tight session budget set
    /// this lower.
    pub max_session_cost_usd_micros: Option<u64>,
    /// Override `tui.show_reasoning_usage`. The user's
    /// reasoning-toggle bug only manifests with `false`: a hidden
    /// reasoning entry can still be the Ctrl+O target.
    #[serde(default)]
    pub show_reasoning_usage: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Prompt {
        text: String,
        #[serde(default = "Step::default_wait")]
        wait_for: WaitFor,
    },
    Action(Action),
}

impl Step {
    fn default_wait() -> WaitFor {
        WaitFor::TurnCompleted
    }
}

/// What the driver waits for before moving past a `prompt` step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    TurnCompleted,
    ToolCall { tool: String },
    TextContains { text: String },
}

/// Actions are imperative side-steps the driver performs synchronously
/// between (or, when `when` is set, during) prompt turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Auto-approve any matching ApprovalRequested. Optionally filtered by tool name.
    Approve {
        #[serde(default)]
        r#match: Option<ApprovalMatch>,
        #[serde(default)]
        when: Option<When>,
    },
    /// Auto-deny any matching ApprovalRequested.
    Deny {
        #[serde(default)]
        r#match: Option<ApprovalMatch>,
        #[serde(default)]
        when: Option<When>,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Run a slash command (e.g. `/compact`).
    SlashCommand {
        command: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Write a file in the workspace mid-run.
    EditFile {
        path: PathBuf,
        /// Whole-file replacement. Either this or `replace` must be set.
        #[serde(default)]
        content: Option<String>,
        /// Find/with substitution. Errors if `find` is not present in the file.
        #[serde(default)]
        replace: Option<EditReplace>,
        #[serde(default)]
        when: Option<When>,
    },
    /// Sleep before continuing.
    WaitSeconds {
        seconds: u64,
        #[serde(default)]
        when: Option<When>,
    },
    /// Cancel the most recently started turn (if any).
    CancelTurn {
        #[serde(default)]
        when: Option<When>,
    },
    /// Soft assertion against the running run state. Failure produces a finding.
    Assert {
        check: Assertion,
        #[serde(default)]
        when: Option<When>,
    },
    /// Append a user message into the agent's conversation transcript
    /// (without starting a new turn). Useful for testing the
    /// "interrupting user" path — pair with `when.on_tool = "..."`
    /// to fire mid-stream during a long-running turn.
    InjectUserText {
        text: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Scripted response to an `McpElicitationRequested` event. Replaces
    /// the Phase 1 auto-cancel: queue a `RespondElicitation` action and
    /// the driver sends the configured decision through the agent's
    /// `response_tx` when a matching request arrives.
    RespondElicitation {
        #[serde(default)]
        r#match: Option<ElicitationMatch>,
        decision: ElicitationDecision,
        #[serde(default)]
        when: Option<When>,
    },
    /// Scripted response to a `RequestUserInputRequested` event.
    /// Mirrors `RespondElicitation` for the agent-side
    /// `RequestUserInputResponse` channel (choice / freeform / cancel).
    RespondUserInput {
        #[serde(default)]
        r#match: Option<UserInputMatch>,
        decision: UserInputDecision,
        #[serde(default)]
        when: Option<When>,
    },
    /// Apply a unified diff to a workspace file mid-run. Lets scenarios
    /// stage a deliberate broken-build state without requiring a
    /// fixture branch on disk. Diffs that don't apply cleanly produce
    /// an `asserted_fail` ActionStep instead of aborting the run.
    ApplyDiff {
        path: PathBuf,
        unified_diff: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Switch the session mode mid-run via `/plan` / `/build`. Maps to
    /// `Agent::dispatch_command` so the existing slash-handler logic
    /// (mode-change events, plan-mode indicator updates) fires.
    SwitchMode {
        /// `"plan"` or `"build"`. Validated when the action fires; an
        /// unknown value produces an `asserted_fail` ActionStep.
        mode: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Attach a file (or pasted text) as conversation context. Maps to
    /// `Agent::attach_file_context` / `attach_pasted_context`.
    AttachFile {
        /// Path relative to the workspace root. Absolute paths are
        /// honored as-is.
        path: PathBuf,
        #[serde(default)]
        when: Option<When>,
    },
    /// Detach a previously-attached context entry by attachment id.
    DetachAttachment {
        id: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Send a single key event into the live `TuiHarness`. Requires
    /// `[tui_capture] drive_tui = true`. `key` uses the same dialect
    /// as `[tui.keymap]` overrides — `"Ctrl+O"`, `"Alt+Up"`,
    /// `"PageDown"`, `"F11"`, `"Enter"`. Validated at scenario-load
    /// time so a typo fails parsing, not dispatch.
    SendKey {
        key: String,
        #[serde(default)]
        when: Option<When>,
    },
    /// Send a sequence of keys into the live `TuiHarness`. Pumps the
    /// drain loop between each key so the harness sees the same
    /// "between frames" state production does. Optional `delay_ms`
    /// inserts a real sleep between keys for scenarios that need to
    /// give an async background task time to land.
    SendKeys {
        keys: Vec<String>,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default)]
        when: Option<When>,
    },
}

impl Action {
    pub fn when(&self) -> Option<&When> {
        match self {
            Action::Approve { when, .. }
            | Action::Deny { when, .. }
            | Action::SlashCommand { when, .. }
            | Action::EditFile { when, .. }
            | Action::WaitSeconds { when, .. }
            | Action::CancelTurn { when, .. }
            | Action::Assert { when, .. }
            | Action::InjectUserText { when, .. }
            | Action::RespondElicitation { when, .. }
            | Action::RespondUserInput { when, .. }
            | Action::ApplyDiff { when, .. }
            | Action::SwitchMode { when, .. }
            | Action::AttachFile { when, .. }
            | Action::DetachAttachment { when, .. }
            | Action::SendKey { when, .. }
            | Action::SendKeys { when, .. } => when.as_ref(),
        }
    }
}

/// Matcher used by `Action::RespondElicitation`. All fields are
/// optional; an unset field matches anything. An empty `ElicitationMatch`
/// matches the first incoming MCP elicitation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElicitationMatch {
    #[serde(default)]
    pub server: Option<String>,
    /// `"form"` or `"url"` — matches `McpElicitationKind`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// The decision payload the driver sends back through the agent's
/// `McpElicitationResponse` oneshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ElicitationDecision {
    /// Accept the elicitation. `content` is forwarded verbatim into
    /// `McpElicitationResponse.content`; the MCP server interprets the
    /// shape per its schema.
    Accept {
        #[serde(default)]
        content: Option<serde_json::Value>,
    },
    /// Decline the elicitation (the user-visible "deny" path).
    Decline,
    /// Cancel — same effect as the pre-Phase-2 auto-cancel.
    Cancel,
}

/// Matcher used by `Action::RespondUserInput`. Both fields are
/// optional; an unset field matches anything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserInputMatch {
    /// Substring match against `RequestUserInputRequest.question`.
    #[serde(default)]
    pub prompt_contains: Option<String>,
}

/// The decision payload the driver sends back through the agent's
/// `RequestUserInputResponse` oneshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UserInputDecision {
    /// Select a multiple-choice option by `value`.
    Choice { value: String },
    /// Submit free-form text.
    Freeform { text: String },
    /// Cancel — same effect as the pre-Phase-2 auto-cancel.
    Cancel,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalMatch {
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditReplace {
    pub find: String,
    pub with: String,
}

/// Optional predicate. Empty `When` means "fire immediately when the step
/// becomes current."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct When {
    /// Fire when an event of this kind is observed during the current turn.
    pub on_event: Option<String>,
    /// Fire when a tool call with this name is observed.
    pub on_tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    /// The given text appears in the assembled assistant output of the latest turn.
    TextContains { text: String },
    /// At most this many tool calls have been observed in the run so far.
    MaxToolCalls { max: u64 },
    /// A `ToolCallStarted` event for `tool` was observed with
    /// arguments JSON containing `args_contains` as a substring of its
    /// serialized form. Useful for "assert the agent ran `grep` with
    /// pattern X" without spelling out the full arg shape.
    ToolCallWithArgs { tool: String, args_contains: String },
    /// The named finding rule fired at least once during this run.
    /// Deferred: the check runs after the run-time auto-findings
    /// scan, so during dispatch this records a pending status.
    FindingFired { rule_id: String },
    /// The most-recent `TurnCompleted.stop_reason` (mapped through
    /// `stop_reason_label`) matches the constraint. Both `equals` and
    /// `not_in` are optional; an assertion with neither always passes.
    StopReason {
        /// Required exact match (lowercase label like `"end_turn"` /
        /// `"max_tokens"` / `"refusal"`).
        #[serde(default)]
        equals: Option<String>,
        /// Set of forbidden labels — failure when the actual label is
        /// in this set.
        #[serde(default)]
        not_in: Vec<String>,
    },
    /// At least one `TaskStateUpdated` snapshot satisfies the
    /// constraints. Both fields are optional; an unset field matches
    /// anything.
    TaskStateContains {
        /// A step title (or step.detail) substring match. The
        /// assertion passes when ANY captured snapshot has a step
        /// whose `title` OR `detail` contains this substring.
        #[serde(default)]
        step_matches: Option<String>,
        /// A `blocker` substring match against any captured snapshot's
        /// `blocker` field.
        #[serde(default)]
        blocker_contains: Option<String>,
    },
    /// The live harness's `status_text()` contains `text`. Requires
    /// `[tui_capture] drive_tui = true`. Use to pin status-bar
    /// feedback like `"expanded 1 of"` after a Ctrl-O.
    TuiStatusContains { text: String },
    /// A specific transcript entry has the expected `entry_kind`
    /// and/or `collapsed` state. `index` picks the target — last
    /// entry, last of a specific kind, or an absolute position.
    /// Useful for asserting that Ctrl-O toggled the right row.
    /// (`entry_kind` instead of `kind` to avoid collision with the
    /// outer `#[serde(tag = "kind")]` discriminator.)
    TuiTranscriptEntry {
        index: TranscriptIndex,
        #[serde(default)]
        entry_kind: Option<String>,
        #[serde(default)]
        collapsed: Option<bool>,
    },
    /// The most recent rendered frame's `plain_text` contains `text`.
    /// Substring match — reads the same projection the eval
    /// `frames_tui.jsonl` records.
    TuiFrameContains { text: String },
    /// Inverse of `TuiFrameContains` — fails when the frame still
    /// holds `text`. Used to pin "every chip flipped" invariants
    /// after a bulk toggle.
    TuiFrameDoesNotContain { text: String },
}

/// Selector for `Assertion::TuiTranscriptEntry`. Mirrors the way TUI
/// tests reach for entries: by absolute position when the scenario is
/// deterministic, by entry kind when the scenario script can vary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum TranscriptIndex {
    /// The last transcript entry, regardless of kind.
    Last,
    /// The last entry whose kind tag (`"reasoning" | "tool_result" |
    /// "message" | "log" | "plan_card" | "diff" | "slash_echo"`)
    /// matches `entry_kind`.
    LastOfKind { entry_kind: String },
    /// A specific absolute index into the transcript. Out-of-bounds
    /// produces an `asserted_fail`.
    Absolute { index: usize },
}

/// Soft expectations evaluated at the end of the run; failures produce
/// findings rather than aborting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expect {
    #[serde(default)]
    pub final_text_contains: Vec<String>,
    #[serde(default)]
    pub max_wall_clock_seconds: Option<u64>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub no_tool_errors: bool,
    /// Threshold for the `high_tool_burst` auto-finding. Defaults to 10.
    #[serde(default)]
    pub max_tools_per_turn: Option<u64>,
    /// Threshold for the `expect_input_tokens_per_turn` auto-finding —
    /// the per-turn equivalent of `max_input_tokens`. When omitted, the
    /// rule does not fire.
    #[serde(default)]
    pub max_input_tokens_per_turn: Option<u64>,
    /// Provider-reported `finish_reason` values that are NOT allowed on
    /// any completed turn. Each match produces an `expect_finish_reason`
    /// finding. Useful for asserting "this run must not end with
    /// `length` truncation" or, with the synthetic
    /// `stop_no_action` sentinel, "no turn finished with `stop` but
    /// emitted no tool call and only intent text". The sentinel is
    /// implemented by the `stop_with_intent_text_no_tool_call` rule.
    #[serde(default)]
    pub finish_reason_not: Vec<String>,
    /// Maximum tolerated count of dropped tool-call frames (sum across
    /// all turns). The chat-completions provider silently drops tool
    /// calls whose stream cut before a function name arrived, which is
    /// invisible to the user and a likely root cause of the
    /// "I'll do X then stop" Qwen pattern. Default 0.
    #[serde(default)]
    pub max_dropped_tool_calls: Option<u32>,
    /// Per-event timeout for the driver's `start_turn` event pump.
    /// Defaults to 60s (replaces the legacy hardcoded 10s). A
    /// `ToolProgress` heartbeat resets the timer so long-running
    /// tools that emit regular progress events no longer
    /// silently truncate. Set lower in scenarios that should bail
    /// quickly on stalled providers; set higher (or omit) for
    /// scenarios with deliberately slow tools.
    #[serde(default)]
    pub event_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriageConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// One-line summary of the surface area this scenario is testing.
    /// Appended to the triage instructions so the LLM drops findings
    /// unrelated to this area.
    #[serde(default)]
    pub focus: Option<String>,
    /// Arbitrary extra prompt text appended verbatim after `focus`.
    #[serde(default)]
    pub extra_prompt: Option<String>,
}

pub fn load(path: &Path) -> Result<Scenario, EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| EvalError::Io(format!("reading {path:?}: {err}")))?;
    let scenario: Scenario = toml::from_str(&text)
        .map_err(|err| EvalError::ScenarioParse(format!("parsing {path:?}: {err}")))?;
    scenario.validate()?;
    Ok(scenario)
}

impl Scenario {
    pub fn validate(&self) -> Result<(), EvalError> {
        if self.id.is_empty() {
            return Err(EvalError::ScenarioParse("id must be non-empty".into()));
        }
        for (idx, step) in self.steps.iter().enumerate() {
            if let Step::Action(Action::EditFile {
                content, replace, ..
            }) = step
                && content.is_none()
                && replace.is_none()
            {
                return Err(EvalError::ScenarioParse(format!(
                    "step {idx}: edit_file requires either `content` or `replace`"
                )));
            }
        }
        Ok(())
    }

    /// Slugify the scenario id for use as a directory name.
    pub fn slug(&self) -> String {
        self.id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "scenario_tests.rs"]
mod tests;
