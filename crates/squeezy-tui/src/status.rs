//! Status-bar items, accent grouping, and styled-line renderer.
//!
//! Each item is a segment in the built-in or user-configured status line.
//! Items are grouped into [`StatusLineAccent`] families so the rendered list
//! paints with a consistent color vocabulary across enabled items.
//!
//! The legacy `render_status_details` plain-text path is kept for tests and
//! for the historical verbose detail line that fires when no
//! `[tui].status_line` is configured AND `status_verbosity = verbose`.

use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::{TuiApp, compact_text, context_window_pct};

/// Separator drawn between rendered items.
const STATUS_LINE_SEPARATOR: &str = " · ";

/// Default status-bar items when the user hasn't configured `[tui].status_line`.
/// Tight, high-signal set: who you're talking to, where you are, what
/// you're shipping, and what you've spent. Drops the model+reasoning
/// duplicate of `provider-and-model`, the project-name duplicate of
/// `current-dir`, and `context-used` (only meaningful well into a long
/// session — users opt in via `/statusline` when they need it).
/// `CacheHit` is included but only renders when cached or cache-write
/// tokens are nonzero, so it is invisible until prompt caching warms up.
pub(crate) const DEFAULT_STATUS_LINE_ITEMS: &[StatusLineItem] = &[
    StatusLineItem::ProviderAndModel,
    StatusLineItem::CurrentDir,
    StatusLineItem::Languages,
    StatusLineItem::GitBranch,
    StatusLineItem::PullRequestNumber,
    StatusLineItem::BranchChanges,
    StatusLineItem::Cost,
    StatusLineItem::CacheHit,
];

/// One configurable status-bar item.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) enum StatusLineItem {
    // Model
    Model,
    ModelWithReasoning,
    ProviderAndModel,
    ReasoningEffort,
    // Path
    CurrentDir,
    ProjectName,
    Languages,
    // Branch
    GitBranch,
    PullRequestNumber,
    BranchChanges,
    // State
    RunState,
    Mode,
    // Usage
    ContextRemaining,
    ContextUsed,
    ContextWindowSize,
    UsedTokens,
    TotalInputTokens,
    TotalOutputTokens,
    CachedTokens,
    CacheWriteTokens,
    /// Compact cache indicator: renders only when cached or cache-write tokens
    /// are nonzero, showing `cache ↑W/↓R` (write/read) or `cached R` for
    /// read-only hits. Included in the default status line so prompt-cache
    /// health is visible without `/statusline` customization.
    CacheHit,
    Tools,
    BytesRead,
    // Limit
    Cost,
    CostCap,
    Budget,
    // Metadata
    SqueezyVersion,
    SessionId,
    ConfigSources,
    Telemetry,
    // Mode
    Permissions,
    ApprovalMode,
    Sandbox,
    Mcp,
    Redactions,
    Receipts,
    // Thread
    Pins,
    CompactGeneration,
    // Progress
    TaskProgress,
}

impl StatusLineItem {
    /// Stable kebab-case identifier used in TOML and in the picker UI.
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ModelWithReasoning => "model-with-reasoning",
            Self::ProviderAndModel => "provider-and-model",
            Self::ReasoningEffort => "reasoning-effort",
            Self::CurrentDir => "current-dir",
            Self::ProjectName => "project-name",
            Self::Languages => "languages",
            Self::GitBranch => "git-branch",
            Self::PullRequestNumber => "pull-request-number",
            Self::BranchChanges => "branch-changes",
            Self::RunState => "run-state",
            Self::Mode => "mode",
            Self::ContextRemaining => "context-remaining",
            Self::ContextUsed => "context-used",
            Self::ContextWindowSize => "context-window-size",
            Self::UsedTokens => "used-tokens",
            Self::TotalInputTokens => "total-input-tokens",
            Self::TotalOutputTokens => "total-output-tokens",
            Self::CachedTokens => "cached-tokens",
            Self::CacheWriteTokens => "cache-write-tokens",
            Self::CacheHit => "cache-hit",
            Self::Tools => "tools",
            Self::BytesRead => "bytes-read",
            Self::Cost => "cost",
            Self::CostCap => "cost-cap",
            Self::Budget => "budget",
            Self::SqueezyVersion => "squeezy-version",
            Self::SessionId => "session-id",
            Self::ConfigSources => "config-sources",
            Self::Telemetry => "telemetry",
            Self::Permissions => "permissions",
            Self::ApprovalMode => "approval-mode",
            Self::Sandbox => "sandbox",
            Self::Mcp => "mcp",
            Self::Redactions => "redactions",
            Self::Receipts => "receipts",
            Self::Pins => "pins",
            Self::CompactGeneration => "compact-generation",
            Self::TaskProgress => "task-progress",
        }
    }

    /// One-line description shown next to the slug in the picker.
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Model => "Current model name",
            Self::ModelWithReasoning => "Current model name with reasoning level",
            Self::ProviderAndModel => "Active provider and model (provider:model)",
            Self::ReasoningEffort => {
                "Configured reasoning effort (omitted when unset / model default)"
            }
            Self::CurrentDir => "Current working directory",
            Self::ProjectName => "Project name (omitted when unavailable)",
            Self::Languages => "Detected workspace languages (counts when available)",
            Self::GitBranch => "Current Git branch (omitted when unavailable)",
            Self::PullRequestNumber => "Open pull request number for the current branch",
            Self::BranchChanges => "Committed branch changes against the default branch",
            Self::RunState => "Compact session run-state text (Ready, Working, Thinking)",
            Self::Mode => "Active session mode (Plan or Build)",
            Self::ContextRemaining => "Percentage of context window remaining",
            Self::ContextUsed => "Percentage of context window used",
            Self::ContextWindowSize => "Current model context-window size in tokens",
            Self::UsedTokens => "Total input + output tokens spent this session",
            Self::TotalInputTokens => "Total input tokens spent this session",
            Self::TotalOutputTokens => "Total output tokens spent this session",
            Self::CachedTokens => "Cached input tokens served from prompt cache",
            Self::CacheWriteTokens => "Input tokens written to prompt cache this session",
            Self::CacheHit => {
                "Compact cache-hit indicator (hidden when no cache activity); shows write↑ and read↓ counts"
            }
            Self::Tools => "Number of tool calls this session",
            Self::BytesRead => "Bytes read from tool outputs this session",
            Self::Cost => "Spend in USD this session (with cap %)",
            Self::CostCap => "Session cost cap in USD",
            Self::Budget => "Budget-denial counter for the active turn",
            Self::SqueezyVersion => "Running Squeezy version",
            Self::SessionId => "Current session id",
            Self::ConfigSources => "Active config tier labels",
            Self::Telemetry => "Telemetry on/off",
            Self::Permissions => "Active permission policy summary",
            Self::ApprovalMode => "Active command approval mode",
            Self::Sandbox => "Active shell sandbox mode",
            Self::Mcp => "Connected MCP servers summary",
            Self::Redactions => "Redactions applied this session",
            Self::Receipts => "Receipt stub + negative receipt cache hits",
            Self::Pins => "Number of pinned context items",
            Self::CompactGeneration => "Compaction generation counter",
            Self::TaskProgress => "Latest mid-turn task progress",
        }
    }

    /// All items, in the order shown in the picker.
    pub(crate) const ALL: &'static [StatusLineItem] = &[
        Self::ProviderAndModel,
        Self::ModelWithReasoning,
        Self::Model,
        Self::ReasoningEffort,
        Self::CurrentDir,
        Self::ProjectName,
        Self::Languages,
        Self::GitBranch,
        Self::PullRequestNumber,
        Self::BranchChanges,
        Self::RunState,
        Self::Mode,
        Self::ContextRemaining,
        Self::ContextUsed,
        Self::ContextWindowSize,
        Self::UsedTokens,
        Self::TotalInputTokens,
        Self::TotalOutputTokens,
        Self::CachedTokens,
        Self::CacheWriteTokens,
        Self::CacheHit,
        Self::Tools,
        Self::BytesRead,
        Self::Cost,
        Self::CostCap,
        Self::Budget,
        Self::Permissions,
        Self::ApprovalMode,
        Self::Sandbox,
        Self::Mcp,
        Self::Redactions,
        Self::Receipts,
        Self::Pins,
        Self::CompactGeneration,
        Self::ConfigSources,
        Self::Telemetry,
        Self::TaskProgress,
        Self::SqueezyVersion,
        Self::SessionId,
    ];
}

impl FromStr for StatusLineItem {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // Codex/squeezy compatibility aliases.
        let normalized = match s {
            "status" => "run-state",
            "project" | "project-root" => "project-name",
            "codex-version" => "squeezy-version",
            "thread-id" => "session-id",
            "context-usage" => "context-used",
            "model-name" => "model",
            other => other,
        };
        for item in Self::ALL {
            if item.slug() == normalized {
                return Ok(*item);
            }
        }
        Err(())
    }
}

/// Color family for an item — drives the 10-accent palette the status
/// bar paints each item with.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StatusLineAccent {
    Model,
    Path,
    Branch,
    State,
    Usage,
    Limit,
    Metadata,
    Mode,
    Thread,
    Progress,
}

impl StatusLineAccent {
    pub(crate) const fn for_item(item: StatusLineItem) -> Self {
        match item {
            StatusLineItem::Model
            | StatusLineItem::ModelWithReasoning
            | StatusLineItem::ProviderAndModel
            | StatusLineItem::ReasoningEffort => Self::Model,
            StatusLineItem::CurrentDir
            | StatusLineItem::ProjectName
            | StatusLineItem::Languages => Self::Path,
            StatusLineItem::GitBranch
            | StatusLineItem::PullRequestNumber
            | StatusLineItem::BranchChanges => Self::Branch,
            StatusLineItem::RunState | StatusLineItem::Mode => Self::State,
            StatusLineItem::ContextRemaining
            | StatusLineItem::ContextUsed
            | StatusLineItem::ContextWindowSize
            | StatusLineItem::UsedTokens
            | StatusLineItem::TotalInputTokens
            | StatusLineItem::TotalOutputTokens
            | StatusLineItem::CachedTokens
            | StatusLineItem::CacheWriteTokens
            | StatusLineItem::CacheHit
            | StatusLineItem::Tools
            | StatusLineItem::BytesRead => Self::Usage,
            StatusLineItem::Cost | StatusLineItem::CostCap | StatusLineItem::Budget => Self::Limit,
            StatusLineItem::SqueezyVersion
            | StatusLineItem::SessionId
            | StatusLineItem::ConfigSources
            | StatusLineItem::Telemetry => Self::Metadata,
            StatusLineItem::Permissions
            | StatusLineItem::ApprovalMode
            | StatusLineItem::Sandbox
            | StatusLineItem::Mcp
            | StatusLineItem::Redactions
            | StatusLineItem::Receipts => Self::Mode,
            StatusLineItem::Pins | StatusLineItem::CompactGeneration => Self::Thread,
            StatusLineItem::TaskProgress => Self::Progress,
        }
    }

    pub(crate) fn fallback_color(self) -> Color {
        match self {
            Self::Model | Self::State | Self::Metadata | Self::Mode => crate::render::theme::cyan(),
            Self::Path | Self::Usage | Self::Progress => crate::render::theme::green(),
            Self::Branch | Self::Limit | Self::Thread => crate::render::theme::magenta(),
        }
    }
}

/// Build the styled detail line. Returns `None` when the configured (or
/// default) item list produces no rendered segments — the caller decides
/// whether to fall back to the legacy plain-text detail line or to draw
/// nothing.
pub(crate) fn render_status_detail_line(
    app: &TuiApp,
    items: &[StatusLineItem],
    use_theme_colors: bool,
) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len().saturating_mul(2));
    for item in items {
        let Some(text) = resolve_status_item(app, *item) else {
            continue;
        };
        if !spans.is_empty() {
            spans.push(Span::styled(
                STATUS_LINE_SEPARATOR,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        let mut style = if use_theme_colors {
            let color = if matches!(*item, StatusLineItem::Languages) {
                crate::render::theme::brand_accent()
            } else {
                StatusLineAccent::for_item(*item).fallback_color()
            };
            Style::default().fg(color)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        if matches!(item, StatusLineItem::PullRequestNumber) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        spans.push(Span::styled(text, style));
    }
    if spans.is_empty() {
        None
    } else {
        Some(Line::from(spans))
    }
}

/// Resolve a single item to its display string. `None` hides the item.
pub(crate) fn resolve_status_item(app: &TuiApp, item: StatusLineItem) -> Option<String> {
    match item {
        StatusLineItem::Model => Some(compact_text(&app.model, 40)),
        StatusLineItem::ModelWithReasoning => {
            let frag = crate::reasoning_status_fragment(app);
            if frag.is_empty() {
                Some(compact_text(&app.model, 40))
            } else {
                let mut text = String::with_capacity(app.model.len() + frag.len());
                text.push_str(&app.model);
                text.push_str(&frag);
                Some(compact_text(&text, 48))
            }
        }
        StatusLineItem::ProviderAndModel => {
            let mut text = String::with_capacity(app.provider_name.len() + 1 + app.model.len());
            text.push_str(app.provider_name);
            text.push(':');
            text.push_str(&app.model);
            Some(compact_text(&text, 54))
        }
        StatusLineItem::ReasoningEffort => app
            .reasoning_effort
            .map(|effort| format!("effort {}", effort.as_str())),
        StatusLineItem::CurrentDir => Some(compact_text(&app.directory, 48)),
        StatusLineItem::ProjectName => app
            .workspace_root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        StatusLineItem::Languages => {
            let summary = app.language_summary.trim();
            if summary.is_empty() || summary == "none" {
                None
            } else {
                Some(compact_text(summary, 48))
            }
        }
        StatusLineItem::GitBranch => app
            .repo
            .branch
            .as_deref()
            .map(|b| compact_text(b, 32))
            .or_else(|| {
                if app.repo.available {
                    Some("detached".to_string())
                } else {
                    None
                }
            }),
        StatusLineItem::PullRequestNumber => {
            app.repo.pull_request.map(|number| format!("PR #{number}"))
        }
        StatusLineItem::BranchChanges => {
            app.repo.branch_changes.as_ref().map(|(added, removed)| {
                if *added == 0 && *removed == 0 {
                    "no branch changes".to_string()
                } else {
                    format!("+{added} -{removed}")
                }
            })
        }
        StatusLineItem::RunState => Some(format_run_state(app)),
        StatusLineItem::Mode => Some(crate::title_case_mode(app.mode).to_string()),
        StatusLineItem::ContextRemaining => context_pct(app).map(|pct| {
            let remaining = 100u64.saturating_sub(pct.min(100));
            format!("ctx {remaining}% left")
        }),
        StatusLineItem::ContextUsed => context_pct(app).map(|pct| format!("ctx {pct}% used")),
        StatusLineItem::ContextWindowSize => {
            context_window_tokens(app).map(|window| format!("window {window}"))
        }
        StatusLineItem::UsedTokens => {
            let total = app.cost.input_tokens.unwrap_or(0) + app.cost.output_tokens.unwrap_or(0);
            if total == 0 {
                None
            } else {
                Some(format!("tok {total}"))
            }
        }
        StatusLineItem::TotalInputTokens => {
            app.cost.input_tokens.map(|tokens| format!("in {tokens}"))
        }
        StatusLineItem::TotalOutputTokens => {
            app.cost.output_tokens.map(|tokens| format!("out {tokens}"))
        }
        StatusLineItem::CachedTokens => app
            .cost
            .cached_input_tokens
            .map(|tokens| format!("cached {tokens}")),
        StatusLineItem::CacheWriteTokens => app
            .cost
            .cache_write_input_tokens
            .map(|tokens| format!("cache_write {tokens}")),
        StatusLineItem::CacheHit => {
            let reads = app.cost.cached_input_tokens.unwrap_or(0);
            let writes = app.cost.cache_write_input_tokens.unwrap_or(0);
            match (writes, reads) {
                (0, 0) => None,
                (0, r) => Some(format!("cache ↓{r}")),
                (w, 0) => Some(format!("cache ↑{w}")),
                (w, r) => Some(format!("cache ↑{w}/↓{r}")),
            }
        }
        StatusLineItem::Tools => Some(format!("tools {}", app.metrics.tool_calls)),
        StatusLineItem::BytesRead => Some(format!("read {}", format_bytes(app.metrics.bytes_read))),
        StatusLineItem::Cost => Some(format_cost_segment(&app.cost, app.cost_cap_usd_micros)),
        StatusLineItem::CostCap => app
            .cost_cap_usd_micros
            .filter(|cap| *cap > 0)
            .map(|cap| format!("cap ${:.2}", cap as f64 / 1_000_000.0)),
        StatusLineItem::Budget => Some(format_budget(app)),
        StatusLineItem::SqueezyVersion => Some(format!("v{}", app.version)),
        StatusLineItem::SessionId => app.session_id.as_ref().map(|id| compact_text(id, 18)),
        StatusLineItem::ConfigSources => Some(format!("cfg {}", app.config_sources)),
        StatusLineItem::Telemetry => Some(format!("telemetry {}", app.telemetry.as_str())),
        StatusLineItem::Permissions => Some(app.permissions.compact()),
        StatusLineItem::ApprovalMode => Some(format!("approval {}", app.permissions.shell)),
        StatusLineItem::Sandbox => Some(format!("sandbox {}", app.permissions.sandbox)),
        StatusLineItem::Mcp => Some(format!("mcp {}", crate::format_mcp_status(app))),
        StatusLineItem::Redactions => Some(format!("redactions {}", app.metrics.redactions)),
        StatusLineItem::Receipts => Some(format!(
            "receipts {}",
            app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits
        )),
        StatusLineItem::Pins => Some(format!("pins {}", app.context_compaction.pinned.len())),
        StatusLineItem::CompactGeneration => {
            Some(format!("compact {}", app.context_compaction.generation))
        }
        StatusLineItem::TaskProgress => app
            .latest_plan_progress
            .as_ref()
            .map(|progress| compact_text(progress, 60)),
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn context_pct(app: &TuiApp) -> Option<u64> {
    context_percent_window_tokens(app)
        .map(|window| context_window_pct(context_used_tokens(app), window))
}

fn context_used_tokens(app: &TuiApp) -> u64 {
    app.status_context_input_tokens
        .unwrap_or(app.context_estimate.estimated_tokens)
}

fn context_window_tokens(app: &TuiApp) -> Option<u64> {
    app.status_context_window_tokens.or({
        if app.context_window_tokens == 0 {
            None
        } else {
            Some(app.context_window_tokens)
        }
    })
}

fn context_percent_window_tokens(app: &TuiApp) -> Option<u64> {
    match (
        app.status_context_input_tokens,
        app.status_context_window_tokens,
    ) {
        (Some(_), Some(window)) => Some(window),
        _ if app.context_window_tokens > 0 => Some(app.context_window_tokens),
        _ => None,
    }
}

fn format_run_state(app: &TuiApp) -> String {
    if app.cancel.is_some() {
        "Working".to_string()
    } else if app.turn_rx.is_some() {
        "Thinking".to_string()
    } else {
        "Ready".to_string()
    }
}

fn format_budget(app: &TuiApp) -> String {
    if app.metrics.budget_denials == 0 {
        "budget ok".to_string()
    } else {
        format!("budget denied:{}", app.metrics.budget_denials)
    }
}

/// Render the legacy comma-separated detail line. Kept for tests and for
/// the verbose-fallback path used when the user has not yet configured a
/// status-line list.
pub(crate) fn render_status_details(app: &TuiApp) -> String {
    let segments: [Option<String>; 18] = [
        segments::permissions(app),
        segments::repo(app),
        segments::sandbox(app),
        segments::telemetry(app),
        segments::mcp(app),
        segments::cost(app),
        segments::tokens(app),
        segments::context(app),
        segments::pins(app),
        segments::compact(app),
        segments::tools(app),
        segments::budget(app),
        segments::config(app),
        segments::bytes_read(app),
        segments::receipts(app),
        segments::redactions(app),
        segments::cached_tokens(app),
        segments::cache_write_tokens(app),
    ];
    let mut output = String::new();
    for segment in segments.into_iter().flatten() {
        if !output.is_empty() {
            output.push_str("  ");
        }
        output.push_str(&segment);
    }
    output
}

/// Render the `cost ...` segment with optional cap and percent.
pub(crate) fn format_cost_segment(
    cost: &squeezy_core::CostSnapshot,
    cap_usd_micros: Option<u64>,
) -> String {
    use crate::commands::format_cost;
    match cap_usd_micros {
        Some(cap) if cap > 0 => {
            let spent = cost.estimated_usd_micros.unwrap_or(0);
            // Use one decimal place so small movements near a low cap are
            // visible before a full percentage point changes.
            let percent = if cap == 0 {
                0.0
            } else {
                ((spent as f64) / (cap as f64) * 100.0).min(999.9)
            };
            format!(
                "cost {} / ${:.2} ({:.1}%)",
                format_cost(cost),
                cap as f64 / 1_000_000.0,
                percent
            )
        }
        _ => format!("cost {}", format_cost(cost)),
    }
}

pub(crate) mod segments {
    use super::*;
    use crate::commands::format_optional_u64;
    use crate::{format_mcp_status, reasoning_status_fragment};

    pub(crate) fn permissions(app: &TuiApp) -> Option<String> {
        Some(app.permissions.compact())
    }

    pub(crate) fn repo(app: &TuiApp) -> Option<String> {
        Some(format!("repo {}", app.repo.detail()))
    }

    pub(crate) fn sandbox(app: &TuiApp) -> Option<String> {
        Some(format!("sandbox {}", app.permissions.sandbox))
    }

    pub(crate) fn telemetry(app: &TuiApp) -> Option<String> {
        Some(format!("telemetry {}", app.telemetry.as_str()))
    }

    pub(crate) fn mcp(app: &TuiApp) -> Option<String> {
        Some(format!("mcp {}", format_mcp_status(app)))
    }

    pub(crate) fn cost(app: &TuiApp) -> Option<String> {
        Some(format_cost_segment(&app.cost, app.cost_cap_usd_micros))
    }

    pub(crate) fn tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "tok {}/{}{}",
            format_optional_u64(app.cost.input_tokens),
            format_optional_u64(app.cost.output_tokens),
            reasoning_status_fragment(app),
        ))
    }

    pub(crate) fn context(app: &TuiApp) -> Option<String> {
        let used = context_used_tokens(app);
        let Some(window) = context_percent_window_tokens(app) else {
            return Some(format!("ctx {used}"));
        };
        let pct = context_window_pct(used, window);
        Some(format!(
            "ctx {used}/{threshold} ({pct}%)",
            threshold = window,
        ))
    }

    pub(crate) fn pins(app: &TuiApp) -> Option<String> {
        Some(format!("pins {}", app.context_compaction.pinned.len()))
    }

    pub(crate) fn compact(app: &TuiApp) -> Option<String> {
        Some(format!("compact {}", app.context_compaction.generation))
    }

    pub(crate) fn tools(app: &TuiApp) -> Option<String> {
        Some(format!("tools {}", app.metrics.tool_calls))
    }

    pub(crate) fn budget(app: &TuiApp) -> Option<String> {
        if app.metrics.budget_denials == 0 {
            Some("budget ok".to_string())
        } else {
            Some(format!("budget denied:{}", app.metrics.budget_denials))
        }
    }

    pub(crate) fn config(app: &TuiApp) -> Option<String> {
        Some(format!("cfg {}", app.config_sources))
    }

    pub(crate) fn bytes_read(app: &TuiApp) -> Option<String> {
        Some(format!("read {}B", app.metrics.bytes_read))
    }

    pub(crate) fn receipts(app: &TuiApp) -> Option<String> {
        let total = app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits;
        Some(format!("receipts {total}"))
    }

    pub(crate) fn redactions(app: &TuiApp) -> Option<String> {
        Some(format!("redactions {}", app.metrics.redactions))
    }

    pub(crate) fn cached_tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "cached {}",
            format_optional_u64(app.cost.cached_input_tokens)
        ))
    }

    pub(crate) fn cache_write_tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "cache_write {}",
            format_optional_u64(app.cost.cache_write_input_tokens)
        ))
    }
}
