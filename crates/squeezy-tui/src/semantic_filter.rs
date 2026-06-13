//! Main-view Semantic Filters (§12.5.2).
//!
//! The Ctrl+T transcript overlay already narrows its view by semantic category
//! via [`crate::OverlayFilter`] (errors, tool calls, a single tool, …). This
//! module brings the SAME idea to the *main* inline transcript: a
//! [`SemanticCategory`] the user cycles in place so the main view shows only
//! (say) the errors, just the tool calls, or only the assistant answers — the
//! complement of the overlay's local `f` filter, reachable without opening the
//! overlay at all.
//!
//! It is a thin leaf over the existing classifiers: every category projects onto
//! one [`crate::OverlayFilter`] variant ([`SemanticCategory::to_overlay_filter`])
//! so the entry-matching logic the renderer, jump-nav, and overlay already share
//! ([`crate::entry_matches_overlay_filter`]) is reused verbatim — there is no
//! second, drift-prone classifier here. The module owns only the small bits that
//! are genuinely main-view-local: the cycle order, the wrap-around stepping, and
//! the human label painted in the active-filter badge.
//!
//! State lives on [`crate::TuiApp::main_semantic_filter`]; it is `All` (off) by
//! default so an idle session is byte-identical to before this landed.

/// The semantic category the main view is narrowed to. `All` is the resting
/// "no filter — show everything" state; the other variants isolate one kind of
/// entry. `Tool(i)` indexes the derived distinct-tool-name list
/// ([`crate::distinct_overlay_tool_names`]), exactly like
/// [`crate::OverlayFilter::Tool`].
///
/// Deliberately a *subset* of [`crate::OverlayFilter`]: the spec scopes the
/// main-view filter to the categories a reader reaches for inline — errors, tool
/// calls, one tool, user turns, assistant answers — rather than the overlay's
/// full set (which also has `Subagent`/`CurrentTurn`). Mapping onto
/// `OverlayFilter` keeps the matching shared; the narrower enum keeps the
/// in-place cycle short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum SemanticCategory {
    /// No filter — every entry renders (the resting state).
    #[default]
    All,
    /// Failure surfaces only ([`crate::OverlayFilter::Errors`]).
    Errors,
    /// Tool-result entries only ([`crate::OverlayFilter::ToolCalls`]).
    ToolCalls,
    /// A single tool, indexed into the distinct-tool-name list.
    Tool(usize),
    /// User messages only.
    UserTurns,
    /// Assistant messages only.
    Assistant,
}

impl SemanticCategory {
    /// Project this category onto the shared [`crate::OverlayFilter`] so the
    /// renderer/jump-nav entry-matcher ([`crate::entry_matches_overlay_filter`])
    /// is reused verbatim — no second classifier to drift.
    ///
    /// `UserTurns`/`Assistant` have no single `OverlayFilter` twin (the overlay
    /// lumps both into `Conversation`), so they are handled by the caller; this
    /// returns `None` for them and the caller applies the role test directly.
    pub(crate) fn to_overlay_filter(self) -> Option<crate::OverlayFilter> {
        match self {
            Self::All => Some(crate::OverlayFilter::All),
            Self::Errors => Some(crate::OverlayFilter::Errors),
            Self::ToolCalls => Some(crate::OverlayFilter::ToolCalls),
            Self::Tool(i) => Some(crate::OverlayFilter::Tool(i)),
            Self::UserTurns | Self::Assistant => None,
        }
    }

    /// `true` when this category narrows the view (anything but `All`). Drives
    /// whether the active-filter badge paints and whether the main render path
    /// pays the per-entry filter test at all.
    pub(crate) fn is_active(self) -> bool {
        !matches!(self, Self::All)
    }

    /// Short human label for the active-filter badge / status. `Tool(i)` reads
    /// the live distinct-tool-name list so the badge names the actual tool; an
    /// out-of-range index (the list shrank) degrades to a bare `tool`.
    pub(crate) fn label(self, tool_names: &[String]) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Errors => "errors".to_string(),
            Self::ToolCalls => "tool calls".to_string(),
            Self::Tool(i) => tool_names
                .get(i)
                .map(|name| format!("tool: {name}"))
                .unwrap_or_else(|| "tool".to_string()),
            Self::UserTurns => "user turns".to_string(),
            Self::Assistant => "assistant".to_string(),
        }
    }
}

/// The cycle order the in-place filter key walks, for the current transcript.
/// `All` is first (so a cycle from the resting state lands on the first real
/// filter, and one more wrap returns to `All`). One `Tool(i)` per distinct tool
/// is appended only when more than one tool appears — a single-tool transcript
/// already has `ToolCalls`, so a redundant per-tool entry would just be a second
/// identical view.
pub(crate) fn cycle(tool_names: &[String]) -> Vec<SemanticCategory> {
    let mut cats = vec![
        SemanticCategory::All,
        SemanticCategory::UserTurns,
        SemanticCategory::Assistant,
        SemanticCategory::ToolCalls,
        SemanticCategory::Errors,
    ];
    if tool_names.len() > 1 {
        for i in 0..tool_names.len() {
            cats.push(SemanticCategory::Tool(i));
        }
    }
    cats
}

/// Step `current` to the next/previous category in [`cycle`], wrapping around.
/// A `current` that is not in the cycle (a stale `Tool(i)` after the tool list
/// shrank) restarts the walk from `All`'s neighbour, so the user is never stuck
/// on a filter the transcript can no longer satisfy.
pub(crate) fn step(
    current: SemanticCategory,
    tool_names: &[String],
    backward: bool,
) -> SemanticCategory {
    let cats = cycle(tool_names);
    // `cats` is never empty (`All` is always present), so the modular arithmetic
    // below is total.
    let pos = cats.iter().position(|c| *c == current);
    let len = cats.len();
    let next_index = match pos {
        Some(i) if backward => (i + len - 1) % len,
        Some(i) => (i + 1) % len,
        // Not in the cycle: forward → first real filter, backward → last.
        None if backward => len - 1,
        None => 1,
    };
    cats[next_index]
}

#[cfg(test)]
#[path = "semantic_filter_tests.rs"]
mod tests;
