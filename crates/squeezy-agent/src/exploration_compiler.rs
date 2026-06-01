use serde_json::json;
use squeezy_tools::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplorationIntent {
    FindDefinition,
    FindCallers,
    ChangeImpact,
    RouteDiscovery,
    TestPairing,
    RepoMap,
    MethodListing,
    Hierarchy,
}

impl ExplorationIntent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FindDefinition => "find_definition",
            Self::FindCallers => "find_callers",
            Self::ChangeImpact => "change_impact",
            Self::RouteDiscovery => "route_discovery",
            Self::TestPairing => "test_pairing",
            Self::RepoMap => "repo_map",
            Self::MethodListing => "method_listing",
            Self::Hierarchy => "hierarchy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplorationPlan {
    pub(crate) intent: ExplorationIntent,
    pub(crate) query: Option<String>,
    pub(crate) calls: Vec<ToolCall>,
    pub(crate) guard_raw_reads: bool,
}

pub(crate) const RAW_READ_DENIAL_REASON: &str = "exploration compiler refused raw read before graph context; call repo_map, definition_search, symbol_context, or another graph navigation tool first";
/// Cap on `max_results` for graph-tool calls the planner emits before the
/// model has run. A real-world subclass/hierarchy fan-out (e.g. all
/// `WidgetsBindingObserver` subclasses in a Flutter app) routinely exceeds
/// the previous value of 8 and silently truncated the tail. Keeping headroom
/// at 32 covers the realistic-but-not-pathological cases the planner sees;
/// the model can paginate or widen further from there.
pub(crate) const PLANNER_GRAPH_MAX_RESULTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplorationTurnState {
    guard_raw_reads: bool,
    graph_evidence_seen: bool,
}

impl ExplorationTurnState {
    pub(crate) fn from_plan(plan: Option<&ExplorationPlan>) -> Self {
        Self {
            guard_raw_reads: plan.is_some_and(|plan| plan.guard_raw_reads),
            graph_evidence_seen: false,
        }
    }

    pub(crate) fn read_denial_reason(&self, call: &ToolCall) -> Option<&'static str> {
        if !self.guard_raw_reads || self.graph_evidence_seen || call.name != "read_file" {
            return None;
        }
        Some(RAW_READ_DENIAL_REASON)
    }

    pub(crate) fn record_tool_result(&mut self, tool_name: &str, success: bool) {
        if success && is_graph_navigation_tool(tool_name) {
            self.graph_evidence_seen = true;
        }
    }

    /// Lifts the raw-read guard once the planner preflight block has finished
    /// executing, regardless of whether the graph tools succeeded. The planner
    /// is advisory: its output is already in the model's context, which is the
    /// actual goal. Without this escape hatch, a planner misfire (e.g. a junk
    /// query that returns no `Success` from graph tools) would lock out every
    /// `read_file` for the rest of the turn.
    pub(crate) fn mark_preflight_complete(&mut self) {
        self.graph_evidence_seen = true;
    }
}

pub(crate) fn compile_exploration_plan(input: &str) -> Option<ExplorationPlan> {
    let plan = compile_exploration_plan_inner(input)?;
    // File-named tasks (prompt mentions ≥2 explicit source file paths)
    // are a poor fit for speculative graph plumbing: the model can read
    // the named files directly. Suppressing the planner here saves the
    // tool round + tokens its result would have added. RepoMap and
    // RouteDiscovery are exempt — both genuinely need a wide-tree view
    // regardless of whether the prompt happens to cite a file path.
    if matches!(
        plan.intent,
        ExplorationIntent::RepoMap | ExplorationIntent::RouteDiscovery
    ) {
        return Some(plan);
    }
    if explicit_file_path_count(input) >= 2 {
        return None;
    }
    Some(plan)
}

fn compile_exploration_plan_inner(input: &str) -> Option<ExplorationPlan> {
    let lowered = input.to_ascii_lowercase();
    let extracted = extract_symbol_query(input);
    let query = extracted.as_ref().map(|q| q.value.clone());
    // The `definition` and `route` intent heuristics match common English
    // phrasing ("where does", "how does", "flow", "which file"). When the
    // user did not quote a literal and the extracted identifier doesn't look
    // like a Rust-y symbol, fall through to the un-planned path rather than
    // compiling a plan with a garbage query.
    let symbolic_query = extracted
        .as_ref()
        .filter(|q| q.quoted || looks_like_rust_symbol(&q.value))
        .map(|q| q.value.clone());

    if repo_map_intent(&lowered) {
        return Some(ExplorationPlan {
            intent: ExplorationIntent::RepoMap,
            query: None,
            calls: vec![tool_call(
                "planner_repo_map",
                "repo_map",
                json!({"max_depth": 2}),
            )],
            guard_raw_reads: true,
        });
    }

    if test_pairing_intent(&lowered)
        && let Some(query) = query
    {
        return Some(ExplorationPlan {
            intent: ExplorationIntent::TestPairing,
            query: Some(query.clone()),
            calls: vec![
                tool_call(
                    "planner_symbol_context",
                    "symbol_context",
                    json!({"query": query.clone(), "max_results": PLANNER_GRAPH_MAX_RESULTS, "max_references": 12}),
                ),
                tool_call(
                    "planner_test_glob",
                    "glob",
                    json!({"pattern": "**/*test*.rs", "max_paths": 50}),
                ),
            ],
            guard_raw_reads: true,
        });
    }

    if change_impact_intent(&lowered)
        && let Some(query) = query
    {
        return Some(ExplorationPlan {
            intent: ExplorationIntent::ChangeImpact,
            query: Some(query.clone()),
            calls: vec![
                tool_call(
                    "planner_symbol_context",
                    "symbol_context",
                    json!({"query": query.clone(), "max_results": PLANNER_GRAPH_MAX_RESULTS, "max_references": 20}),
                ),
                tool_call(
                    "planner_upstream_flow",
                    "upstream_flow",
                    json!({"query": query.clone(), "max_depth": 3, "max_results": 25}),
                ),
                tool_call(
                    "planner_downstream_flow",
                    "downstream_flow",
                    json!({"query": query.clone(), "max_depth": 2, "max_results": 25}),
                ),
            ],
            guard_raw_reads: true,
        });
    }

    if callers_intent(&lowered)
        && let Some(query) = query
    {
        return Some(ExplorationPlan {
            intent: ExplorationIntent::FindCallers,
            query: Some(query.clone()),
            calls: vec![
                tool_call(
                    "planner_definition_search",
                    "definition_search",
                    json!({"query": query.clone(), "max_results": PLANNER_GRAPH_MAX_RESULTS}),
                ),
                tool_call(
                    "planner_upstream_flow",
                    "upstream_flow",
                    json!({"query": query.clone(), "max_depth": 3, "max_results": 25}),
                ),
            ],
            guard_raw_reads: true,
        });
    }

    if route_intent(&lowered)
        && let Some(query) = symbolic_query.clone()
    {
        return Some(ExplorationPlan {
            intent: ExplorationIntent::RouteDiscovery,
            query: Some(query.clone()),
            calls: vec![
                tool_call("planner_repo_map", "repo_map", json!({"max_depth": 2})),
                tool_call(
                    "planner_downstream_flow",
                    "downstream_flow",
                    json!({"query": query.clone(), "max_depth": 3, "max_results": 25}),
                ),
            ],
            guard_raw_reads: true,
        });
    }

    if hierarchy_intent(&lowered)
        && let Some(query) = symbolic_query.clone()
    {
        // A "subclasses of Foo" / "implementors of Trait" question is
        // exactly what `hierarchy` answers — the model would otherwise
        // grep for `extends Foo` / `: Foo` / etc. across the tree, which
        // is both noisier and language-specific. Pre-issuing the graph
        // call lets the model see the canonical subclass list in one
        // round and decide whether to drill into individual subclasses.
        return Some(ExplorationPlan {
            intent: ExplorationIntent::Hierarchy,
            query: Some(query.clone()),
            calls: vec![tool_call(
                "planner_hierarchy",
                "hierarchy",
                json!({"query": query, "max_results": PLANNER_GRAPH_MAX_RESULTS}),
            )],
            guard_raw_reads: true,
        });
    }

    if method_listing_intent(&lowered)
        && let Some(query) = symbolic_query.clone()
    {
        // A "list methods on Foo" question is satisfied by a single
        // symbol_context call: it returns the matching type plus its
        // declared methods and short reference snippets. Pre-issuing more
        // tools (definition_search + upstream_flow + downstream_flow + ...)
        // burns rounds and adds no new information.
        return Some(ExplorationPlan {
            intent: ExplorationIntent::MethodListing,
            query: Some(query.clone()),
            calls: vec![tool_call(
                "planner_symbol_context",
                "symbol_context",
                json!({"query": query, "max_results": PLANNER_GRAPH_MAX_RESULTS, "max_references": 4}),
            )],
            guard_raw_reads: true,
        });
    }

    if definition_intent(&lowered)
        && let Some(query) = symbolic_query
    {
        // A plain "which file defines X?" prompt only needs the
        // definition packet. Extra context tools are reserved for
        // relationship/member questions so the preflight does not seed a
        // costly fan-out before the model has seen whether one result is
        // already enough.
        return Some(ExplorationPlan {
            intent: ExplorationIntent::FindDefinition,
            query: Some(query.clone()),
            calls: vec![tool_call(
                "planner_definition_search",
                "definition_search",
                json!({"query": query.clone(), "max_results": PLANNER_GRAPH_MAX_RESULTS}),
            )],
            guard_raw_reads: true,
        });
    }

    None
}

pub(crate) fn is_graph_navigation_tool(name: &str) -> bool {
    matches!(
        name,
        "repo_map"
            | "decl_search"
            | "definition_search"
            | "reference_search"
            | "upstream_flow"
            | "downstream_flow"
            | "symbol_context"
            | "hierarchy"
            | "read_slice"
    )
}

fn tool_call(call_id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

fn repo_map_intent(input: &str) -> bool {
    input.contains("repo map")
        || input.contains("repository map")
        || input.contains("architecture")
        || input.contains("map the repo")
        || input.contains("project structure")
}

fn definition_intent(input: &str) -> bool {
    input.contains("define")
        || input.contains("definition")
        || input.contains("declaration")
        || input.contains("where is")
        || input.contains("where does")
        || input.contains("which file")
        || input.contains("find function")
        || input.contains("find struct")
        || input.contains("find trait")
}

fn callers_intent(input: &str) -> bool {
    input.contains("who calls")
        || input.contains("what calls")
        || input.contains("find callers")
        || input.contains("callers of")
        || input.contains("called by")
        || input.contains("references to")
}

fn change_impact_intent(input: &str) -> bool {
    input.contains("impact")
        || input.contains("affected")
        || input.contains("blast radius")
        || input.contains("what changes")
        || input.contains("if i change")
        || input.contains("change impact")
}

fn route_intent(input: &str) -> bool {
    input.contains("route")
        || input.contains("flow")
        || input.contains("path from")
        || input.contains("dependency path")
        || input.contains("how does")
        || input.contains("reach")
}

fn method_listing_intent(input: &str) -> bool {
    input.contains("methods on")
        || input.contains("methods of")
        || input.contains("methods for")
        || input.contains("list methods")
        || input.contains("list the methods")
        || input.contains("what methods")
        || input.contains("which methods")
        || input.contains("members of")
        || input.contains("members on")
        || input.contains("api of")
        || input.contains("api for")
}

/// Cheap heuristic: count substrings in `input` that look like a source
/// file path (e.g. `Sources/Vapor/Routing/RoutesBuilder+Method.swift`,
/// `crates/squeezy-tools/src/file_ops.rs`). A prompt that names ≥2 such
/// paths is almost always doing a targeted multi-file read where
/// speculative graph queries are dead overhead. Avoids a regex
/// dependency: walks the bytes, finds `.<ext>` tokens whose preceding
/// run is path-shaped, dedupes by case-insensitive value.
fn explicit_file_path_count(input: &str) -> usize {
    const EXTS: &[&str] = &[
        "rs", "go", "py", "java", "cs", "js", "ts", "tsx", "jsx", "swift", "kt", "scala", "php",
        "rb", "c", "cpp", "h", "hpp", "dart",
    ];
    let mut found: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, _) in input.match_indices('.') {
        let after = &input[idx + 1..];
        let ext_end = after
            .find(|ch: char| !ch.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        let ext = &after[..ext_end];
        if ext.is_empty() || !EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            continue;
        }
        // Walk backwards from `.` to find the run of path-shaped chars.
        let before = &input[..idx];
        let start = before
            .rfind(|ch: char| {
                !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '+' | '.'))
            })
            .map(|i| i + 1)
            .unwrap_or(0);
        let path = &input[start..idx + 1 + ext_end];
        // Must contain at least one `/` to count as a path (not bare filename
        // commentary like "fix.rs is a file"). Two file paths in a directory
        // section like `{widgets,material,cupertino}/...` will naturally pass
        // this check because the realistic prompts always include the parent
        // package or `src/` prefix.
        if !path.contains('/') {
            continue;
        }
        found.insert(path.to_ascii_lowercase());
        if found.len() >= 2 {
            return found.len();
        }
    }
    found.len()
}

fn hierarchy_intent(input: &str) -> bool {
    input.contains("subclass")
        || input.contains("subclasses")
        || input.contains("implementors")
        || input.contains("implementers")
        || input.contains("implementations of")
        || input.contains("concrete class")
        || input.contains("classes that extend")
        || input.contains("classes that implement")
        || input.contains("classes that mix")
        || input.contains("class extends")
        || input.contains("class implements")
        || input.contains("that mixes in")
        || input.contains("that extends")
        || input.contains("that implements")
        || input.contains("uses the mixin")
        || input.contains("mixers of")
        || input.contains("inherit from")
        || input.contains("inheritors")
        || input.contains("derived classes")
        || input.contains("derived types")
        || input.contains("everything that implements")
        || input.contains("every implementor")
        || input.contains("every subclass")
}

fn test_pairing_intent(input: &str) -> bool {
    input.contains("test")
        && (input.contains("pair")
            || input.contains("cover")
            || input.contains("coverage")
            || input.contains("which test")
            || input.contains("tests for")
            || input.contains("where are the tests"))
}

struct ExtractedQuery {
    value: String,
    /// True when the value came from a properly-delimited literal in the
    /// input. Quoted strings are treated as explicit user intent and bypass
    /// the Rust-symbol heuristic at plan-compile time.
    quoted: bool,
}

fn extract_symbol_query(input: &str) -> Option<ExtractedQuery> {
    if let Some(value) = extract_quoted(input) {
        return Some(ExtractedQuery {
            value,
            quoted: true,
        });
    }
    extract_identifier(input).map(|value| ExtractedQuery {
        value,
        quoted: false,
    })
}

fn extract_quoted(input: &str) -> Option<String> {
    for quote in ['`', '"', '\''] {
        // Require a matched closing quote of the same kind so contractions
        // like `What's` don't yield the trailing fragment as a query. A
        // properly-delimited literal produces at least three parts after
        // `splitn` (prefix, inside, suffix).
        let parts: Vec<&str> = input.splitn(3, quote).collect();
        if parts.len() < 3 {
            continue;
        }
        let candidate = parts[1].trim();
        if is_useful_query(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn extract_identifier(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-' | '/'))
        })
        .map(|token| token.trim_matches(|ch: char| matches!(ch, '.' | ':' | '-' | '/')))
        .filter(|token| is_useful_query(token))
        .filter(|token| !is_stopword(token))
        .map(str::to_string)
        .collect();
    // Prefer tokens that look like Rust identifier paths so prompts like
    // "Who calls Runner::run from main()?" do not pick up `main` as the
    // symbol just because it appears last. Fall back to the last remaining
    // token only when nothing identifier-shaped is present.
    tokens
        .iter()
        .rfind(|token| looks_like_rust_symbol(token))
        .or_else(|| tokens.last())
        .cloned()
}

fn looks_like_rust_symbol(token: &str) -> bool {
    token.contains("::")
        || token.contains('_')
        || token.starts_with(|ch: char| ch.is_ascii_uppercase())
}

fn is_useful_query(token: &str) -> bool {
    if token.len() < 3 || !token.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    if looks_like_path(token) {
        return false;
    }
    !is_prompt_noise_word(token)
}

/// English prompt scaffolding words (`ONLY`, `OUTPUT`, `EXPECTED`, ...) that
/// the surrounding instructions routinely capitalize for emphasis. Without
/// this rejection, `looks_like_rust_symbol` accepts them as identifier-shaped
/// (uppercase first char) and the planner fires nonsense graph queries like
/// `symbol_context "ONLY"` that drag whole runs into degraded paths.
fn is_prompt_noise_word(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "only"
            | "todo"
            | "note"
            | "notes"
            | "output"
            | "outputs"
            | "return"
            | "returns"
            | "error"
            | "errors"
            | "warning"
            | "warnings"
            | "stop"
            | "exactly"
            | "must"
            | "expect"
            | "expects"
            | "expected"
            | "actual"
            | "input"
            | "inputs"
            | "testing"
            | "please"
            | "answer"
            | "explain"
            | "describe"
            | "summary"
            | "summarize"
    )
}

fn looks_like_path(token: &str) -> bool {
    if token.contains('/') {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    matches!(
        std::path::Path::new(lower.as_str())
            .extension()
            .and_then(|ext| ext.to_str()),
        Some(
            "rs" | "py"
                | "java"
                | "cs"
                | "go"
                | "cpp"
                | "hpp"
                | "c"
                | "h"
                | "js"
                | "ts"
                | "tsx"
                | "jsx"
                | "rb"
                | "php"
                | "kt"
                | "kts"
                | "swift"
                | "scala"
                | "sc"
                | "dart"
        )
    )
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "which"
            | "where"
            | "what"
            | "when"
            | "does"
            | "file"
            | "files"
            | "define"
            | "defines"
            | "definition"
            | "declaration"
            | "function"
            | "struct"
            | "trait"
            | "method"
            | "calls"
            | "callers"
            | "called"
            | "references"
            | "change"
            | "impact"
            | "tests"
            | "test"
            | "coverage"
            | "route"
            | "flow"
            | "path"
            | "dependency"
            | "from"
            | "into"
            | "with"
            | "that"
            | "this"
            | "the"
            | "for"
            | "and"
            | "how"
            | "are"
    )
}

#[cfg(test)]
#[path = "exploration_compiler_tests.rs"]
mod tests;
