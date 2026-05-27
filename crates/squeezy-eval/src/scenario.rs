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
            | Action::InjectUserText { when, .. } => when.as_ref(),
        }
    }
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
