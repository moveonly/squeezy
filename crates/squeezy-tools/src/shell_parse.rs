use std::collections::VecDeque;

use reqwest::Url;
use tree_sitter::{Node, Parser};

use crate::windows_cmd::is_destructive_windows_segment;
use crate::{PermissionCapability, PermissionRisk, ShellPermissionAnalysis, collapse_whitespace};

#[cfg(test)]
#[path = "shell_parse_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedShellCommand {
    pub(crate) segments: Vec<String>,
    pub(crate) dynamic: bool,
    pub(crate) heredoc_prefix: bool,
}

/// Structured view of one `command` node produced by tree-sitter-bash.
///
/// `extract_command_units` walks the parse tree once and returns one record
/// per `command` node. The five `is_*_shell_segment` classifiers re-split the
/// segment text on whitespace and lose quote boundaries; consumers that need
/// structural answers (is `arg[2]` `-f`? does the command write to
/// `/dev/null`?) should use this typed payload instead.
///
/// Currently only the unit tests consume this surface. The follow-up work to
/// route the existing classifiers through `&CommandUnit` is tracked alongside
/// the F05-cc-tree-sitter-richer-command-extraction audit finding; until
/// that lands, the fields are `#[allow(dead_code)]` so the `pub(crate)` API
/// stays available without tripping the `-D warnings` CI gate.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CommandUnit {
    /// First `command_name` child, with surrounding quotes stripped. Empty
    /// when the command begins with a substitution (e.g. `$(echo rm) -rf`).
    pub(crate) name: String,
    /// Remaining `argument` children in source order. Outer single or
    /// double quotes are stripped so `args[1] == "/tmp/x y"` for
    /// `rm -rf "/tmp/x y"`.
    pub(crate) args: Vec<String>,
    /// Leading `NAME=value` assignments attached to the command (the
    /// `NAME=` form of `env var=val cmd`). Order matches source order.
    pub(crate) env: Vec<(String, String)>,
    /// Redirects attached to either the `command` node itself or the
    /// enclosing `redirected_statement`. `2>/dev/null` becomes
    /// `Redirect { op: ">", target: "/dev/null", fd: Some(2) }`.
    pub(crate) redirects: Vec<Redirect>,
    /// True when the command (or one of its arguments / redirect targets)
    /// contains a `command_substitution`, `process_substitution`, or
    /// `expansion` node. Mirrors the `dynamic` flag on `ParsedShellCommand`
    /// but scoped to this unit.
    pub(crate) has_substitution: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// Redirect operator as it appears in source, normalised to ASCII:
    /// `>`, `>>`, `>|`, `<`, `<<`, `<<<`, `&>`, `&>>`, or `<>`.
    pub(crate) op: String,
    /// Destination text with outer quotes stripped. For heredoc redirects
    /// the target is the delimiter word (e.g. `PY`).
    pub(crate) target: String,
    /// Explicit file descriptor when present (`2>/dev/null` → `Some(2)`).
    /// Absent for the implicit fd 1 / fd 0 cases.
    pub(crate) fd: Option<u32>,
}

/// Walks the bash parse tree and returns one `CommandUnit` per `command`
/// node. Returns an empty vector when tree-sitter cannot parse the input.
/// A single-pass extraction covers command arguments, env vars, and
/// security-relevant AST features in one walk.
#[allow(dead_code)]
pub(crate) fn extract_command_units(command: &str) -> Vec<CommandUnit> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(command, None) else {
        return Vec::new();
    };
    let bytes = command.as_bytes();
    let mut units = Vec::new();
    collect_command_units(tree.root_node(), bytes, None, &mut units);
    units
}

/// Recursive walker. `enclosing` carries redirects attached to a
/// `redirected_statement` parent so they appear on the inner command unit
/// even though the tree-sitter grammar nests them on the wrapper.
#[allow(dead_code)]
fn collect_command_units(
    node: Node<'_>,
    bytes: &[u8],
    enclosing: Option<&[Redirect]>,
    units: &mut Vec<CommandUnit>,
) {
    match node.kind() {
        "command" | "declaration_command" => {
            let mut unit = build_command_unit(node, bytes);
            if let Some(extra) = enclosing {
                for redirect in extra {
                    unit.redirects.push(redirect.clone());
                }
            }
            units.push(unit);
        }
        "redirected_statement" => {
            // Gather the wrapper-level redirects once, then recurse into the
            // body command with them attached. The grammar puts redirects on
            // the `redirected_statement` for forms like `cmd args > out`.
            let mut wrapper_redirects = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        if let Some(redirect) = parse_redirect(child, bytes) {
                            wrapper_redirects.push(redirect);
                        }
                    }
                    _ => {}
                }
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !matches!(
                    child.kind(),
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect"
                ) {
                    collect_command_units(child, bytes, Some(&wrapper_redirects), units);
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_command_units(child, bytes, enclosing, units);
            }
        }
    }
}

#[allow(dead_code)]
fn build_command_unit(node: Node<'_>, bytes: &[u8]) -> CommandUnit {
    let mut unit = CommandUnit::default();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_assignment" => {
                if let Some((name, value)) = parse_variable_assignment(child, bytes) {
                    unit.env.push((name, value));
                } else if child_has_substitution(child) {
                    unit.has_substitution = true;
                }
            }
            "command_name" => {
                unit.name = literal_text(child, bytes);
                if child_has_substitution(child) {
                    unit.has_substitution = true;
                }
            }
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                if let Some(redirect) = parse_redirect(child, bytes) {
                    unit.redirects.push(redirect);
                }
            }
            _ => {
                if child_has_substitution(child) {
                    unit.has_substitution = true;
                }
                let text = literal_text(child, bytes);
                if !text.is_empty() {
                    unit.args.push(text);
                }
            }
        }
    }
    unit
}

#[allow(dead_code)]
fn parse_variable_assignment(node: Node<'_>, bytes: &[u8]) -> Option<(String, String)> {
    let name_node = node.child_by_field_name("name")?;
    let value_node = node.child_by_field_name("value")?;
    Some((
        literal_text(name_node, bytes),
        literal_text(value_node, bytes),
    ))
}

#[allow(dead_code)]
fn parse_redirect(node: Node<'_>, bytes: &[u8]) -> Option<Redirect> {
    let fd = node
        .child_by_field_name("descriptor")
        .and_then(|d| d.utf8_text(bytes).ok())
        .and_then(|s| s.parse::<u32>().ok());
    let op = match node.kind() {
        "heredoc_redirect" => "<<".to_string(),
        "herestring_redirect" => "<<<".to_string(),
        _ => redirect_operator_text(node, bytes).unwrap_or_else(|| ">".to_string()),
    };
    let target_node = node
        .child_by_field_name("destination")
        .or_else(|| node.child_by_field_name("argument"))
        .or_else(|| {
            // Anonymous children carry the operator; the destination is the
            // first named child after it. Heredoc redirects expose the
            // delimiter via the `argument` field above; fall back here only
            // for redirects with neither field populated.
            let mut cursor = node.walk();
            node.named_children(&mut cursor).next()
        })?;
    let target = literal_text(target_node, bytes);
    Some(Redirect { op, target, fd })
}

#[allow(dead_code)]
fn redirect_operator_text(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named()
            && let Ok(text) = child.utf8_text(bytes)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() && trimmed.chars().all(|c| matches!(c, '<' | '>' | '&' | '|')) {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Extract literal text from a node, stripping surrounding quotes when the
/// node represents a `string` or `raw_string`. Substitution nodes return an
/// empty string — the caller already records `has_substitution`.
#[allow(dead_code)]
fn literal_text(node: Node<'_>, bytes: &[u8]) -> String {
    match node.kind() {
        "command_substitution" | "process_substitution" | "expansion" | "simple_expansion" => {
            String::new()
        }
        _ => {
            let Ok(raw) = node.utf8_text(bytes) else {
                return String::new();
            };
            dequote_token(raw).to_string()
        }
    }
}

#[allow(dead_code)]
fn child_has_substitution(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "command_substitution" | "process_substitution" | "expansion"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(child_has_substitution)
}

pub(crate) fn analyze_shell_command(command: &str) -> ShellPermissionAnalysis {
    let normalized = collapse_whitespace(command);
    // Permission flow calls analyze_shell_command twice for the same
    // command (permission_request, then execute_shell_capped). A tiny
    // thread-local LRU avoids the second tree-sitter parse on the hot
    // path. The cache is bounded so long-running agents don't grow
    // unbounded memory.
    thread_local! {
        static MEMO: std::cell::RefCell<VecDeque<(String, ShellPermissionAnalysis)>> =
            const { std::cell::RefCell::new(VecDeque::new()) };
    }
    const MEMO_CAPACITY: usize = 16;
    if let Some(hit) = MEMO.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(key, _)| key == &normalized)
            .map(|(_, analysis)| analysis.clone())
    }) {
        return hit;
    }
    let parsed = parse_shell_command(command);
    let (parser_backed, dynamic, heredoc_prefix, raw_segments) = match parsed {
        Some(parsed) => {
            let segments = if parsed.segments.is_empty() {
                shell_segments(&normalized)
            } else {
                parsed.segments
            };
            (true, parsed.dynamic, parsed.heredoc_prefix, segments)
        }
        None => (false, false, false, shell_segments(&normalized)),
    };
    // Wrappers (sh -c "...", env BAR=v cmd, nohup cmd, xargs cmd, ...) hide
    // the real command behind boilerplate. Append the recursively unwrapped
    // inner commands so destructive/network/compiler checks fire on the
    // actual payload, not just the wrapper.
    let segments = expand_wrapper_segments(raw_segments);
    let first = segments
        .first()
        .map(|segment| shell_command_prefix(segment))
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or_else(|| "shell".to_string());

    let analysis = if segments.is_empty() {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: PermissionRisk::High,
            rule_target: "shell:*".to_string(),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if dynamic {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: PermissionRisk::High,
            rule_target: "shell:*".to_string(),
            network: segments
                .iter()
                .any(|segment| is_network_shell_segment(segment)),
            destructive: segments
                .iter()
                .any(|segment| is_destructive_shell_segment(segment))
                || shell_segment_has_destructive_redirect(&normalized),
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .any(|segment| is_destructive_shell_segment(segment))
        || shell_segment_has_destructive_redirect(&normalized)
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Destructive,
            risk: PermissionRisk::Critical,
            rule_target: format!("{first}:*"),
            network: segments
                .iter()
                .any(|segment| is_network_shell_segment(segment)),
            destructive: true,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .any(|segment| is_network_shell_segment(segment))
    {
        let target = extract_shell_network_host(&segments)
            .map(|host| format!("shell:{first}:{host}"))
            .unwrap_or_else(|| format!("shell:{first}:*"));
        ShellPermissionAnalysis {
            capability: PermissionCapability::Network,
            risk: PermissionRisk::High,
            rule_target: target,
            network: true,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .all(|segment| is_compiler_shell_segment(segment))
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Compiler,
            risk: PermissionRisk::Medium,
            rule_target: format!(
                "{}:*",
                shell_command_prefix(segments.first().unwrap_or(&normalized))
            ),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments.iter().all(|segment| is_git_shell_segment(segment)) {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Git,
            risk: if segments
                .iter()
                .all(|segment| is_git_read_only_segment(segment))
            {
                PermissionRisk::Low
            } else {
                PermissionRisk::High
            },
            rule_target: format!(
                "{}:*",
                shell_command_prefix(segments.first().unwrap_or(&normalized))
            ),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .all(|segment| is_read_only_shell_segment(segment))
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Search,
            risk: PermissionRisk::Low,
            rule_target: format!("{first}:*"),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .all(|segment| is_safe_metadata_write_segment(segment))
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Edit,
            risk: PermissionRisk::Medium,
            rule_target: format!("{first}:*"),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: if heredoc_prefix {
                PermissionRisk::Medium
            } else {
                PermissionRisk::High
            },
            rule_target: format!("{first}:*"),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    };
    MEMO.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MEMO_CAPACITY {
            cache.pop_front();
        }
        cache.push_back((normalized.clone(), analysis.clone()));
    });
    analysis
}

/// For each top-level command segment, append any wrapper-stripped inner
/// command so the rest of the analyzer sees the real argv. Recurses up to
/// `MAX_WRAPPER_DEPTH` times to cover nested wrappers like
/// `nohup sh -c "env BAR=v rm -rf /"`.
pub(crate) fn expand_wrapper_segments(segments: Vec<String>) -> Vec<String> {
    const MAX_WRAPPER_DEPTH: usize = 8;
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        out.push(segment.clone());
        let mut current = segment;
        for _ in 0..MAX_WRAPPER_DEPTH {
            let Some(inner) = unwrap_shell_wrapper(&current) else {
                break;
            };
            // Re-parse the inner: it can contain its own `&&`/`;`/`|`
            // operators, in which case we want each piece as a segment.
            for piece in shell_segments(&inner) {
                if !piece.is_empty() && !out.iter().any(|seg| seg == &piece) {
                    out.push(piece);
                }
            }
            current = inner;
        }
    }
    out
}

/// Try to unwrap one layer of shell wrapping. Returns the inner command
/// string with the wrapper boilerplate removed, or `None` if the segment
/// doesn't begin with a recognized wrapper. The recognized wrappers fall
/// into three families:
///
/// - `sh -c "<cmd>"` / `bash -c '<cmd>'` (and `-lc`, `-ic`) — the script
///   passed to a shell interpreter.
/// - `env [VAR=val …] [-i|-] <argv>` — environment-prefix runners.
/// - `nohup <argv>`, `nice [-n N] <argv>`, `time <argv>`, `timeout <DUR>
///   <argv>`, `stdbuf <opts> <argv>`, `xargs [opts] <argv>`,
///   `sudo [opts] <argv>` — passthrough wrappers.
fn unwrap_shell_wrapper(segment: &str) -> Option<String> {
    let tokens = tokenize_shell_segment(segment);
    let head = tokens.first()?.as_str();
    match head {
        "sh" | "bash" | "zsh" | "fish" | "csh" | "tcsh" | "ksh" | "dash" => {
            // Find the `-c` script flag and surface its argument. The script
            // flag is the single-dash form whose cluster ends in `c` (`-c`,
            // `-lc`, `-ic`) — bash requires `-c` to be terminal because it
            // consumes the next argument. Long options like `--rcfile`,
            // `--norc`, or `--init-file` merely *contain* a `c` and must not
            // be mistaken for it, so we skip every other token (including the
            // positional arguments those options take) rather than stopping at
            // the first non-flag token.
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if tok.strip_prefix("--").is_none()
                    && let Some(short) = tok.strip_prefix('-')
                    && !short.is_empty()
                    && short.ends_with('c')
                {
                    let script = tokens.get(idx + 1)?;
                    return Some(dequote_token(script).to_string());
                }
                idx += 1;
            }
            None
        }
        "env" => {
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if tok == "-" || tok == "-i" || tok == "--ignore-environment" {
                    idx += 1;
                } else if tok.starts_with('-') {
                    // Unknown env flag; bail out conservatively to avoid
                    // swallowing the inner command behind a flag we don't
                    // understand.
                    return None;
                } else if shell_env_assignment_token(tok) {
                    idx += 1;
                } else {
                    break;
                }
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        "nohup" | "time" | "sudo" => {
            // Skip the wrapper and any leading flags so the inner argv is
            // returned cleanly. `sudo` accepts complex flags but stays a
            // passthrough.
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if tok.starts_with('-') {
                    idx += 1;
                } else {
                    break;
                }
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        "nice" => {
            let mut idx = 1;
            if tokens.get(idx).map(String::as_str) == Some("-n") {
                idx += 2;
            } else if tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        "stdbuf" => {
            let mut idx = 1;
            while tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        "timeout" => {
            let mut idx = 1;
            while tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            // First non-flag is the duration (e.g. "30", "10s"). Skip it.
            if tokens.get(idx).is_some() {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        "xargs" => {
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if !tok.starts_with('-') {
                    break;
                }
                let flag = tok.as_str();
                idx += 1;
                if matches!(
                    flag,
                    "-I" | "-L" | "-n" | "-P" | "--max-args" | "--max-procs"
                ) {
                    // Consume the flag's value if present.
                    if tokens.get(idx).is_some() {
                        idx += 1;
                    }
                }
            }
            let inner = tokens.get(idx..)?;
            join_shell_tokens(inner)
        }
        _ => None,
    }
}

fn join_shell_tokens(tokens: &[String]) -> Option<String> {
    let (first, rest) = tokens.split_first()?;
    let mut joined = String::with_capacity(
        tokens
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(tokens.len().saturating_sub(1)),
    );
    joined.push_str(first);
    for token in rest {
        joined.push(' ');
        joined.push_str(token);
    }
    Some(joined)
}

/// True for tokens shaped like `NAME=value` (the env-assignment prefix
/// passed to `env`). Mirrors `split_env_assignment` but operates on owned
/// strings.
fn shell_env_assignment_token(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Quote-aware tokenizer used by the wrapper unwrapper. Single and double
/// quotes group whitespace-separated runs into a single token; the surrounding
/// quotes are preserved on the token so the caller can `dequote_token` it.
pub(crate) fn tokenize_shell_segment(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut iter = segment.chars().peekable();
    while let Some(ch) = iter.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => {
                current.push(ch);
                quote = None;
            }
            (None, '\'') | (None, '"') => {
                current.push(ch);
                quote = Some(ch);
            }
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (_, '\\') => {
                current.push(ch);
                if let Some(next) = iter.next() {
                    current.push(next);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip a single pair of matching outer quotes from a token, leaving its
/// contents otherwise unchanged. Bash escape semantics are not interpreted
/// (the classifier is conservative: `sh -c "rm -rf \\"$HOME\\""` will still
/// surface the literal payload, including the escaped backslashes).
pub(crate) fn dequote_token(token: &str) -> &str {
    let bytes = token.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &token[1..token.len() - 1];
        }
    }
    token
}

pub(crate) fn parse_shell_command(command: &str) -> Option<ParsedShellCommand> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return None;
    }
    let tree = parser.parse(command, None)?;
    let root = tree.root_node();
    let mut segments = Vec::new();
    let heredoc_prefix = shell_heredoc_prefix(root, command);
    let heredoc_prefix_command = heredoc_prefix.as_ref().map(|words| words.join(" "));
    let ignore_heredoc_dynamic = heredoc_prefix.is_some();
    if let Some(prefix_command) = heredoc_prefix_command.as_ref() {
        segments.push(prefix_command.to_owned());
    } else {
        collect_shell_command_nodes(root, command.as_bytes(), &mut segments);
    }
    let dynamic = if let Some(prefix_command) = heredoc_prefix_command.as_deref() {
        root.has_error() || shell_text_is_dynamic(prefix_command)
    } else {
        root.has_error()
            || shell_tree_contains_dynamic(root, false)
            || shell_text_is_dynamic(command)
    };
    Some(ParsedShellCommand {
        segments: if segments.is_empty() {
            shell_segments(command)
        } else {
            segments
        },
        dynamic,
        heredoc_prefix: ignore_heredoc_dynamic,
    })
}

fn collect_shell_command_nodes(node: Node<'_>, bytes: &[u8], segments: &mut Vec<String>) {
    if node.kind() == "command"
        && let Ok(text) = node.utf8_text(bytes)
    {
        let text = collapse_whitespace(text);
        if !text.is_empty() {
            segments.push(text);
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_shell_command_nodes(child, bytes, segments);
    }
}

fn shell_heredoc_prefix(root: Node<'_>, src: &str) -> Option<Vec<String>> {
    if root.has_error() {
        return None;
    }
    if !has_named_descendant_kind(root, "heredoc_redirect")
        && !has_named_descendant_kind(root, "herestring_redirect")
    {
        return None;
    }
    if has_named_descendant_kind(root, "file_redirect") {
        return None;
    }
    let command_node = find_single_command_node(root)?;
    parse_heredoc_command_words(command_node, src)
}

fn parse_heredoc_command_words(cmd: Node<'_>, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if !matches!(word_node.kind(), "word" | "number")
                    || !is_literal_word_or_number(word_node)
                {
                    return None;
                }
                words.push(word_node.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "word" | "number" => {
                if !is_literal_word_or_number(child) {
                    return None;
                }
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "comment" => {}
            kind if is_allowed_heredoc_attachment_kind(kind) => {}
            _ => return None,
        }
    }
    if words.is_empty() { None } else { Some(words) }
}

fn is_literal_word_or_number(node: Node<'_>) -> bool {
    if !matches!(node.kind(), "word" | "number") {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next().is_none()
}

fn is_allowed_heredoc_attachment_kind(kind: &str) -> bool {
    matches!(
        kind,
        "heredoc_body"
            | "simple_heredoc_body"
            | "heredoc_redirect"
            | "herestring_redirect"
            | "redirected_statement"
    )
}

fn find_single_command_node(root: Node<'_>) -> Option<Node<'_>> {
    let mut stack = vec![root];
    let mut single_command = None;
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            if single_command.is_some() {
                return None;
            }
            single_command = Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    single_command
}

fn has_named_descendant_kind(node: Node<'_>, kind: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == kind {
            return true;
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn shell_tree_contains_dynamic(node: Node<'_>, ignore_heredoc_redirect: bool) -> bool {
    if matches!(
        node.kind(),
        "command_substitution"
            | "process_substitution"
            | "expansion"
            | "simple_expansion"
            | "subscript"
    ) || (!ignore_heredoc_redirect && node.kind() == "heredoc_redirect")
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| shell_tree_contains_dynamic(child, ignore_heredoc_redirect))
}

fn shell_text_is_dynamic(command: &str) -> bool {
    command.contains("$(")
        || command.contains('`')
        || command.contains("${")
        || command.contains("<(")
        || command.contains(">(")
}

pub(crate) fn shell_coverage_warnings(command: &str) -> Vec<String> {
    let segments = shell_segments(&collapse_whitespace(command));
    let suspicious = segments.iter().any(|segment| {
        let words = segment.split_whitespace().collect::<Vec<_>>();
        let mut has_mutation = false;
        let mut has_outside_path = false;
        for word in words {
            let trimmed = word.trim_matches(|ch| matches!(ch, '\'' | '"' | '(' | ')' | ';'));
            if matches!(
                trimmed,
                "rm" | "rmdir" | "mv" | "cp" | "dd" | "truncate" | "touch" | "mkdir"
            ) || matches!(trimmed, ">" | ">>")
            {
                has_mutation = true;
            }
            if trimmed.starts_with('/') || trimmed.contains("../") || trimmed == ".." {
                has_outside_path = true;
            }
        }
        has_mutation && has_outside_path
    });
    if suspicious {
        vec![
            "shell command may mutate paths outside the workspace; checkpoint rollback only protects workspace files"
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

pub(crate) fn shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') => quote = None,
            (Some('"'), '"') => quote = None,
            (Some(_), '\\') => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(ch),
            (None, ';') => {
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            (None, '&') if chars.peek() == Some(&'&') => {
                let _ = chars.next();
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            (None, '|') if chars.peek() == Some(&'|') => {
                let _ = chars.next();
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            (None, '|') => {
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    push_shell_segment(&mut segments, &mut current);
    segments
}

fn push_shell_segment(segments: &mut Vec<String>, current: &mut String) {
    let segment = current.trim();
    if !segment.is_empty() {
        segments.push(segment.to_string());
    }
    current.clear();
}

pub(crate) fn shell_command_prefix(segment: &str) -> String {
    let mut parts = segment.split_whitespace();
    let mut first = parts.next().unwrap_or("shell");
    while let Some((name, _)) = split_env_assignment(first) {
        if !shell_env_assignment_allowed_for_prefix(name) {
            return "shell".to_string();
        }
        first = parts.next().unwrap_or("shell");
    }
    if is_bare_shell_prefix(first) {
        return "shell".to_string();
    }
    match first {
        "cargo" | "git" | "npm" | "pnpm" | "yarn" | "bun" | "make" | "just" => parts
            .next()
            .map(|sub| format!("{first} {sub}"))
            .unwrap_or_else(|| first.to_string()),
        _ => first.to_string(),
    }
}

fn split_env_assignment(token: &str) -> Option<(&str, &str)> {
    let (name, value) = token.split_once('=')?;
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some((name, value))
}

fn shell_env_assignment_allowed_for_prefix(name: &str) -> bool {
    matches!(
        name,
        "CI" | "NO_COLOR"
            | "RUST_BACKTRACE"
            | "RUSTFLAGS"
            | "CARGO_TERM_COLOR"
            | "CARGO_INCREMENTAL"
            | "RUST_LOG"
    )
}

fn is_bare_shell_prefix(prefix: &str) -> bool {
    matches!(
        prefix,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "csh"
            | "tcsh"
            | "ksh"
            | "dash"
            | "env"
            | "xargs"
            | "nice"
            | "nohup"
            | "time"
            | "timeout"
            | "stdbuf"
            | "sudo"
    )
}

pub(crate) fn is_destructive_shell_segment(segment: &str) -> bool {
    destructive_shell_segment_reason(segment).is_some()
}

pub(crate) fn destructive_shell_segment_reason(segment: &str) -> Option<String> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    if matches!(
        first,
        "rm" | "rmdir" | "dd" | "truncate" | "shred" | "chown" | "sudo"
    ) {
        return Some(format!("destructive verb {first:?}"));
    }
    // mv defaults to overwrite-on-dest-exists. Plain `mv a b` is treated as a
    // metadata-write-safe verb (the kernel sandbox enforces the actual write
    // rules); explicit `-f`/`--force` is the strict "force overwrite" idiom
    // and stays gated behind approval.
    if first == "mv"
        && tokens
            .iter()
            .skip(1)
            .any(|tok| *tok == "-f" || *tok == "--force")
    {
        return Some("destructive forced move".to_string());
    }
    if destructive_git_pair(&tokens) {
        return Some("destructive git command".to_string());
    }
    if destructive_two_word_command(&tokens) {
        let command = tokens.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        return Some(format!("destructive command {command:?}"));
    }
    if shell_segment_has_destructive_redirect(segment) {
        return Some("destructive redirect".to_string());
    }
    if is_destructive_windows_segment(segment) {
        return Some("destructive Windows command".to_string());
    }
    None
}

/// Pre-spawn safe-verb allowlist for benign metadata-write commands. These
/// verbs touch only inode metadata or create new entries (`mkdir`, `chmod`,
/// `ln`, `touch`) or perform a single rename (`mv` without `-f`), and the
/// kernel sandbox enforces the actual fs scope. Verbs that delete or
/// arbitrarily overwrite (`rm`, `dd`, `truncate`, `chown`, `mv -f`) remain
/// destructive and stay gated behind approval.
pub(crate) fn is_safe_metadata_write_segment(segment: &str) -> bool {
    if is_destructive_shell_segment(segment) {
        return false;
    }
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    matches!(first, "mkdir" | "chmod" | "ln" | "mv" | "touch")
}

/// Detects shell output redirects that write to a filename (`>`, `>>`, `>|`,
/// `&>`, `&>>`, `<>`), while ignoring file-descriptor duplications like
/// `2>&1`, `>&-`, harmless redirects to `/dev/null`, and any `>` that appears
/// inside single or double quotes.
fn shell_segment_has_destructive_redirect(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match (quote, b) {
            (Some(q), c) if c == q => {
                quote = None;
                i += 1;
            }
            (None, b'\'') | (None, b'"') => {
                quote = Some(b);
                i += 1;
            }
            (None, b'\\') if i + 1 < bytes.len() => {
                i += 2;
            }
            (None, b'>') => {
                // `=>` is a JS/TS arrow (e.g. inside a `node - <<'NODE'`
                // heredoc body the byte-scan fallback inspects when the
                // bash grammar choked on the body). It is not a shell
                // redirect, so skip it. Mirrors the existing `>&` (fd
                // duplication) and `>|` (force-overwrite) exclusions
                // below.
                if i > 0 && bytes[i - 1] == b'=' {
                    i += 1;
                    continue;
                }
                // Skip the run of `>` characters (handles `>`, `>>`).
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] == b'>' {
                    j += 1;
                }
                // Optional `|` (force overwrite, `>|`).
                if j < bytes.len() && bytes[j] == b'|' {
                    j += 1;
                }
                // Skip whitespace between operator and target.
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                // `>&N` or `>&-` is a file-descriptor duplication, not a
                // write to a path.
                if j < bytes.len() && bytes[j] == b'&' {
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    let dup_dash = k < bytes.len() && bytes[k] == b'-';
                    if k > j + 1 || dup_dash {
                        i = if dup_dash { k + 1 } else { k };
                        continue;
                    }
                }
                if redirect_target_is_dev_null(bytes, j) {
                    i = j + "/dev/null".len();
                    continue;
                }
                return true;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

fn redirect_target_is_dev_null(bytes: &[u8], start: usize) -> bool {
    let target = b"/dev/null";
    if !bytes[start..].starts_with(target) {
        return false;
    }
    let end = start + target.len();
    bytes
        .get(end)
        .is_none_or(|next| next.is_ascii_whitespace() || matches!(*next, b'|' | b'&' | b';'))
}

/// Recognises the destructive git command families we want to surface
/// without misfiring on substrings like `git push -foreign-rule`. Each entry
/// matches `git <verb> [optional flag]` exactly on token boundaries.
fn destructive_git_pair(tokens: &[&str]) -> bool {
    let Some(&"git") = tokens.first() else {
        return false;
    };
    let Some(&verb) = tokens.get(1) else {
        return false;
    };
    match verb {
        "reset" | "clean" | "checkout" | "restore" => true,
        "stash" => matches!(tokens.get(2).copied(), Some("drop" | "clear")),
        "branch" => tokens.iter().skip(2).any(|tok| *tok == "-D"),
        "push" => tokens
            .iter()
            .skip(2)
            .any(|tok| *tok == "-f" || tok.starts_with("--force")),
        _ => false,
    }
}

fn destructive_two_word_command(tokens: &[&str]) -> bool {
    match tokens.first().copied() {
        Some("terraform") => tokens.get(1).copied() == Some("destroy"),
        Some("kubectl") => tokens.get(1).copied() == Some("delete"),
        Some("docker") => matches!(tokens.get(1).copied(), Some("rm" | "rmi" | "system")),
        _ => false,
    }
}

fn is_network_shell_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "curl"
            | "wget"
            | "nc"
            | "netcat"
            | "ssh"
            | "scp"
            | "sftp"
            | "rsync"
            | "telnet"
            | "ftp"
            | "dig"
            | "nslookup"
            | "ping"
            | "traceroute"
            | "gh"
            | "git fetch"
            | "git pull"
            | "git push"
            | "git clone"
            | "git ls-remote"
            | "cargo fetch"
            | "cargo install"
            | "cargo update"
            | "npm install"
            | "pnpm install"
            | "yarn install"
            | "bun install"
    )
}

fn extract_shell_network_host(segments: &[String]) -> Option<String> {
    for segment in segments {
        for token in tokenize_shell_segment(segment) {
            if let Some(host) = host_from_network_token(dequote_token(&token)) {
                return Some(host);
            }
        }
    }
    None
}

fn host_from_network_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() || token.starts_with('-') {
        return None;
    }
    if let Ok(url) = Url::parse(token)
        && matches!(url.scheme(), "http" | "https" | "ssh" | "git")
    {
        return url.host_str().map(normalize_permission_host);
    }
    if let Some(rest) = token.strip_prefix("git@")
        && let Some((host, _path)) = rest.split_once(':')
    {
        return Some(normalize_permission_host(host));
    }
    if let Some((host, _path)) = token.split_once(':')
        && !host.is_empty()
        && host.contains('.')
        && !host.contains('/')
    {
        return Some(normalize_permission_host(host));
    }
    token
        .contains('.')
        .then(|| token.split('/').next().unwrap_or(token))
        .filter(|host| !host.is_empty() && !host.contains('@'))
        .map(normalize_permission_host)
}

fn normalize_permission_host(host: &str) -> String {
    host.trim_matches(|ch| matches!(ch, '[' | ']'))
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn is_compiler_shell_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "cargo test"
            | "cargo nextest"
            | "cargo check"
            | "cargo clippy"
            | "cargo fmt"
            | "cargo build"
            | "rustc"
            | "make test"
            | "just test"
    )
}

fn is_git_shell_segment(segment: &str) -> bool {
    segment.split_whitespace().next() == Some("git")
}

fn is_git_read_only_segment(segment: &str) -> bool {
    is_plan_mode_read_only_git_segment(segment)
}

fn is_plan_mode_read_only_git_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    if tokens.first().map(String::as_str) != Some("git") {
        return false;
    }
    match tokens.get(1).map(|token| dequote_token(token)) {
        Some("status") => git_args_have_no_write_flags(&tokens[2..]),
        Some("diff" | "log" | "show") => git_args_are_safe_read_only(&tokens[2..]),
        Some("branch") => tokens.iter().skip(2).all(|token| {
            matches!(
                dequote_token(token),
                "-a" | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "--list"
                    | "--show-current"
                    | "--contains"
                    | "--merged"
                    | "--no-merged"
                    | "--color"
                    | "--no-color"
            )
        }),
        _ => false,
    }
}

fn git_args_are_safe_read_only(args: &[String]) -> bool {
    args.iter()
        .map(|arg| dequote_token(arg))
        .all(|arg| !git_arg_writes_output(arg))
}

fn git_args_have_no_write_flags(args: &[String]) -> bool {
    args.iter()
        .map(|arg| dequote_token(arg))
        .all(|arg| !git_arg_writes_output(arg))
}

fn git_arg_writes_output(arg: &str) -> bool {
    matches!(arg, "--output" | "-o") || arg.starts_with("--output=")
}

fn is_plan_mode_read_only_compiler_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    let Some(first) = tokens.first().map(|token| dequote_token(token)) else {
        return false;
    };
    match first {
        "cargo" => match tokens.get(1).map(|token| dequote_token(token)) {
            Some("test" | "nextest" | "check" | "build") => true,
            Some("clippy") => !tokens
                .iter()
                .skip(2)
                .any(|token| matches!(dequote_token(token), "--fix")),
            Some("fmt") => tokens
                .iter()
                .skip(2)
                .any(|token| matches!(dequote_token(token), "--check")),
            _ => false,
        },
        "rustc" => true,
        _ => false,
    }
}

fn is_plan_mode_read_only_shell_segment(segment: &str) -> bool {
    is_read_only_shell_segment(segment)
        || is_plan_mode_read_only_find_segment(segment)
        || is_plan_mode_read_only_filter_segment(segment)
        || is_plan_mode_read_only_sed_segment(segment)
        || is_plan_mode_read_only_python_filter_segment(segment)
        || is_plan_mode_read_only_git_segment(segment)
        || is_plan_mode_read_only_compiler_segment(segment)
}

fn is_plan_mode_mutating_compiler_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    let Some(first) = tokens.first().map(|token| dequote_token(token)) else {
        return false;
    };
    match first {
        "cargo" => match tokens.get(1).map(|token| dequote_token(token)) {
            Some("fmt") => !tokens
                .iter()
                .skip(2)
                .any(|token| matches!(dequote_token(token), "--check")),
            Some("clippy") => tokens
                .iter()
                .skip(2)
                .any(|token| matches!(dequote_token(token), "--fix")),
            _ => false,
        },
        _ => false,
    }
}

fn is_plan_mode_mutating_git_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    if tokens.first().map(|token| dequote_token(token)) != Some("git") {
        return false;
    }
    match tokens.get(1).map(|token| dequote_token(token)) {
        Some("checkout" | "switch" | "restore" | "reset" | "clean" | "add" | "commit") => true,
        Some("branch") => !is_plan_mode_read_only_git_segment(segment),
        Some("diff" | "log" | "show") => tokens
            .iter()
            .skip(2)
            .map(|token| dequote_token(token))
            .any(git_arg_writes_output),
        _ => false,
    }
}

fn dynamic_shell_text_contains_mutator(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    [
        "rm ",
        "rmdir ",
        "touch ",
        "mkdir ",
        "mv ",
        "cp ",
        "tee ",
        "sed -i",
        "chmod ",
        "chown ",
        "truncate ",
        "dd ",
        "git checkout",
        "git switch",
        "git reset",
        "git clean",
        "cargo fmt",
        "clippy --fix",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn is_plan_mode_mutating_shell_segment(segment: &str) -> bool {
    is_destructive_shell_segment(segment)
        || is_safe_metadata_write_segment(segment)
        || is_plan_mode_mutating_filter_segment(segment)
        || is_plan_mode_mutating_compiler_segment(segment)
        || is_plan_mode_mutating_git_segment(segment)
}

fn is_plan_mode_mutating_filter_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    let Some(first) = tokens.first().map(|token| dequote_token(token)) else {
        return false;
    };
    match first {
        "sed" => tokens.iter().skip(1).any(|token| {
            let token = dequote_token(token);
            token == "--in-place" || token.starts_with("-i")
        }),
        "base64" => tokens.iter().skip(1).any(|token| {
            let token = dequote_token(token);
            matches!(token, "-o" | "--output")
                || token.starts_with("-o")
                || token.starts_with("--output=")
        }),
        "sort" => tokens.iter().skip(1).any(|token| {
            let token = dequote_token(token);
            matches!(token, "-o" | "--output" | "--output-document")
                || token.starts_with("--output=")
        }),
        _ => false,
    }
}

/// Classify a shell command for Plan mode's additional mutation boundary.
/// `NeedsApproval` means the command is neither proven read-only nor a known
/// mutation; the normal policy layer should ask the user instead of denying.
pub fn classify_plan_mode_shell_command(command: &str) -> PlanModeShellSafety {
    let normalized = collapse_whitespace(command);
    let Some(parsed) = parse_shell_command(&normalized) else {
        return PlanModeShellSafety::NeedsApproval;
    };
    if parsed.dynamic && dynamic_shell_text_contains_mutator(&normalized) {
        return PlanModeShellSafety::Mutating;
    }
    if shell_segment_has_destructive_redirect(&normalized) {
        return PlanModeShellSafety::Mutating;
    }
    let segments = expand_wrapper_segments(parsed.segments);
    if segments.is_empty() {
        return PlanModeShellSafety::NeedsApproval;
    }
    if segments
        .iter()
        .any(|segment| is_plan_mode_mutating_shell_segment(segment))
    {
        return PlanModeShellSafety::Mutating;
    }
    if !parsed.dynamic
        && segments
            .iter()
            .all(|segment| is_plan_mode_read_only_shell_segment(segment))
    {
        return PlanModeShellSafety::ReadOnly;
    }
    PlanModeShellSafety::NeedsApproval
}

/// Returns true when every command segment is known not to mutate repository
/// files. Build/test probes may still write normal compiler artifacts.
pub fn plan_mode_shell_command_is_read_only(command: &str) -> bool {
    classify_plan_mode_shell_command(command) == PlanModeShellSafety::ReadOnly
}

pub(crate) fn is_read_only_shell_segment(segment: &str) -> bool {
    let prefix = shell_command_prefix(segment);
    if matches!(
        prefix.as_str(),
        "cat"
            | "du"
            | "echo"
            | "file"
            | "grep"
            | "head"
            | "id"
            | "ls"
            | "nl"
            | "paste"
            | "pwd"
            | "rev"
            | "rg"
            | "seq"
            | "stat"
            | "tail"
            | "uname"
            | "wc"
            | "which"
            | "whoami"
    ) {
        return true;
    }
    // Windows PowerShell and cmd.exe read-only commands. These appear when
    // Squeezy runs on Windows or analyses commands submitted via the shell
    // tool. Including them avoids unnecessary AI-reviewer round-trips for
    // safe exploration commands.
    //
    // IMPORTANT: Only classify as read-only when the segment contains no
    // output-redirect flags (-OutFile, -FilePath, -Encoding with -OutFile,
    // etc.). Many PowerShell exploration cmdlets accept -OutFile which turns
    // them into file-writing operations.
    if windows_segment_has_output_flag(segment) {
        return false;
    }
    is_read_only_windows_segment(&prefix)
}

/// True when the PowerShell segment contains a flag that redirects output
/// to a file, making the command a writer rather than a pure reader.
fn windows_segment_has_output_flag(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    // `-LiteralPath` and `-FilePath` are intentionally excluded: they're the
    // *path* parameter on read cmdlets in the allowlist (e.g. `Get-Content
    // -LiteralPath`, `Get-FileHash -FilePath`). The prefix-only allowlist in
    // `is_read_only_windows_segment` already excludes writer cmdlets like
    // `Out-File`/`Set-Content`/`Add-Content`, so this stays defensive.
    tokens.iter().any(|tok| {
        matches!(
            *tok,
            "-outfile" | "-destination" | "-destinationpath" | "-append"
        )
    })
}

/// True when `prefix` is a PowerShell or cmd.exe command whose default
/// behaviour is read-only exploration. Commands that accept an `-OutFile` or
/// output-redirect variant are only accepted here when they have no such flag
/// (callers pass the already-extracted prefix, not the full segment, so
/// flag-level checking is not done here — the conservative default is that an
/// unknown flag keeps the command out of this fast path).
fn is_read_only_windows_segment(prefix: &str) -> bool {
    matches!(
        prefix.to_ascii_lowercase().as_str(),
        // PowerShell exploration cmdlets and unambiguous built-in aliases.
        "get-childitem"
            | "gci"
            // `dir` is a PowerShell alias for Get-ChildItem as well as the
            // cmd.exe directory listing command; both are read-only.
            | "dir"
            | "get-content"
            | "get-item"
            | "get-location"
            | "get-process"
            | "get-service"
            | "get-command"
            | "get-help"
            | "help"
            | "get-member"
            | "get-variable"
            | "get-history"
            | "get-alias"
            | "select-string"
            | "sls"
            | "measure-object"
            | "select-object"
            | "where-object"
            | "format-list"
            | "fl"
            | "format-table"
            | "ft"
            | "format-wide"
            | "out-string"
            | "test-path"
            | "resolve-path"
            // cmd.exe read-only commands not already in the POSIX list.
            | "type"
            | "ver"
            | "vol"
            | "ipconfig"
            | "systeminfo"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanModeShellSafety {
    ReadOnly,
    NeedsApproval,
    Mutating,
}

fn is_plan_mode_read_only_find_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    if tokens.first().map(|token| dequote_token(token)) != Some("find") {
        return false;
    }
    !tokens.iter().skip(1).any(|token| {
        matches!(
            dequote_token(token),
            "-delete"
                | "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
        )
    })
}

fn is_plan_mode_read_only_filter_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    let Some(first) = tokens.first().map(|token| dequote_token(token)) else {
        return false;
    };
    match first {
        "true" | "cut" | "tr" | "jq" => true,
        "base64" => !tokens.iter().skip(1).any(|token| {
            let token = dequote_token(token);
            matches!(token, "-o" | "--output")
                || token.starts_with("-o")
                || token.starts_with("--output=")
        }),
        "sort" => !tokens.iter().skip(1).any(|token| {
            matches!(
                dequote_token(token),
                "-o" | "--output" | "--output-document"
            ) || dequote_token(token).starts_with("--output=")
        }),
        "uniq" => {
            let positionals = tokens
                .iter()
                .skip(1)
                .filter(|token| !dequote_token(token).starts_with('-'))
                .count();
            positionals <= 1
        }
        _ => false,
    }
}

fn is_plan_mode_read_only_sed_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    if tokens.first().map(|token| dequote_token(token)) != Some("sed") {
        return false;
    }
    let mut args = tokens.iter().skip(1).map(|token| dequote_token(token));
    if args.next() != Some("-n") {
        return false;
    }
    let Some(script) = args.next() else {
        return false;
    };
    if args.any(|arg| arg.starts_with('-')) {
        return false;
    }
    sed_print_script_is_read_only(script)
}

fn sed_print_script_is_read_only(script: &str) -> bool {
    let Some(rest) = script.strip_suffix('p') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let mut parts = rest.split(',');
    let Some(start) = parts.next() else {
        return false;
    };
    if start.is_empty() || !start.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(end) => {
            !end.is_empty() && end.chars().all(|ch| ch.is_ascii_digit()) && parts.next().is_none()
        }
    }
}

fn is_plan_mode_read_only_python_filter_segment(segment: &str) -> bool {
    let tokens = tokenize_shell_segment(segment);
    let Some(first) = tokens.first().map(|token| dequote_token(token)) else {
        return false;
    };
    if !matches!(first, "python" | "python2" | "python3") {
        return false;
    }
    let Some(script) = tokens.windows(2).find_map(|pair| {
        matches!(dequote_token(&pair[0]), "-c" | "--command").then(|| dequote_token(&pair[1]))
    }) else {
        return false;
    };
    python_inline_script_is_read_only_filter(script)
}

fn python_inline_script_is_read_only_filter(script: &str) -> bool {
    let lowered = script.to_ascii_lowercase();
    let banned = [
        "__",
        "open(",
        "exec(",
        "eval(",
        "compile(",
        "os.",
        "subprocess",
        "socket",
        "requests",
        "urllib",
        "pathlib",
        "shutil",
        "glob",
        "system(",
        "popen",
        "remove(",
        "unlink(",
        "mkdir(",
        "rmdir(",
        "rename(",
        "replace(",
        "chmod(",
        "chown(",
        ".write(",
        "write(",
    ];
    if banned.iter().any(|needle| lowered.contains(needle)) {
        return false;
    }
    python_inline_imports_are_read_only(script)
}

fn python_inline_imports_are_read_only(script: &str) -> bool {
    for statement in script.split([';', '\n']) {
        let trimmed = statement.trim();
        if let Some(rest) = trimmed.strip_prefix("import ") {
            for module in rest.split(',') {
                let name = module
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("");
                if !matches!(
                    name,
                    "sys"
                        | "json"
                        | "csv"
                        | "re"
                        | "collections"
                        | "itertools"
                        | "math"
                        | "statistics"
                ) {
                    return false;
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if !matches!(name, "json" | "csv" | "re" | "collections" | "itertools") {
                return false;
            }
        }
    }
    true
}

/// Extract candidate filesystem write-target path strings from the
/// destination arguments of common non-destructive file-mutating verbs
/// (`tee`, `cp`, `mv`, `install`, `dd of=`, `ln`, `sed -i`, `chmod`,
/// `touch`, `mkdir`).
///
/// The verb set deliberately omits destructive verbs (`rm`, `chown`,
/// `truncate`, `shred`, `mv -f`, …) and output redirects (`>`, `>>`):
/// those already classify as the `Destructive` capability and gate behind
/// approval, whereas the verbs here would otherwise be auto-allowed as a
/// plain `Shell`/`Edit` write under the workspace-write default.
///
/// Parsing reuses the tree-sitter-backed [`extract_command_units`] over the
/// unwrapped segments (so `sh -c "cp x /etc/y"` is inspected on its real
/// payload). Targets are returned with a leading `~` / `~/` expanded to the
/// user's home directory so the common `sed -i ~/.bashrc` form resolves
/// outside the workspace. Unexpanded `$VAR` references are left as-is and
/// will not be flagged (a known limitation of static analysis; the kernel
/// sandbox remains the backstop for those).
pub(crate) fn extract_shell_write_targets(command: &str) -> Vec<String> {
    let normalized = collapse_whitespace(command);
    let segments = expand_wrapper_segments(shell_segments(&normalized));
    let mut targets = Vec::new();
    for segment in &segments {
        for unit in extract_command_units(segment) {
            collect_verb_write_targets(&unit.name, &unit.args, &mut targets);
        }
    }
    targets
}

fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| home.to_string_lossy().into_owned())
}

fn is_valid_var_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Substitute `$VAR` / `${VAR}` / `$env:VAR` / `${env:VAR}` / `%VAR%` from
/// the process environment. An unresolved variable is left literal (with its
/// `$` or `%…%`) so the caller can treat it as an unverifiable — and
/// therefore out-of-workspace — target.
///
/// Handles the PowerShell-specific `$env:VARNAME` and `${env:VARNAME}`
/// provider syntax (Bug 3): these are lowercased to the environment variable
/// name after the colon and resolved the same way as `$VARNAME`.
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        // cmd-style `%VAR%`. Resolved vars are substituted; an unresolved one
        // is left literal so `path_has_unresolved_var` escalates it.
        if c == '%' {
            let mut name = String::new();
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' {
                    name.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }
            if !name.is_empty() && chars.peek() == Some(&'%') {
                chars.next();
                match std::env::var(&name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        out.push('%');
                        out.push_str(&name);
                        out.push('%');
                    }
                }
            } else {
                out.push('%');
                out.push_str(&name);
            }
            continue;
        }
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for nc in chars.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    // PowerShell `${env:VARNAME}` provider syntax.
                    let resolved_name =
                        powershell_env_provider_var(&name).unwrap_or_else(|| name.clone());
                    if is_valid_var_name(&resolved_name)
                        && let Ok(val) = std::env::var(&resolved_name)
                    {
                        out.push_str(&val);
                        continue;
                    }
                }
                out.push_str("${");
                out.push_str(&name);
                if closed {
                    out.push('}');
                }
            }
            Some(c2) if c2.is_ascii_alphabetic() || c2 == '_' => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_alphanumeric() || nc == '_' || nc == ':' {
                        name.push(nc);
                        chars.next();
                        // Stop after reading a single `:` in PowerShell
                        // provider form (`$env:VAR`) — only one colon is
                        // valid; anything after it is the variable name.
                        if nc == ':' {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                // Handle `$env:VARNAME` — consume the rest of the var name
                // after the colon.
                let env_var_name = if name.ends_with(':') {
                    let mut rest = String::new();
                    while let Some(&nc) = chars.peek() {
                        if nc.is_ascii_alphanumeric() || nc == '_' {
                            rest.push(nc);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // For `$env:VARNAME` resolve VARNAME from environment.
                    if name.eq_ignore_ascii_case("env:") && !rest.is_empty() {
                        rest
                    } else {
                        // Unknown provider — leave literal.
                        out.push('$');
                        out.push_str(&name);
                        out.push_str(&rest);
                        continue;
                    }
                } else {
                    name.clone()
                };

                match std::env::var(&env_var_name) {
                    Ok(val) => out.push_str(&val),
                    // Leave the literal `$NAME`; a remaining `$` signals an
                    // unverifiable target to the workspace-escape check.
                    Err(_) => {
                        out.push('$');
                        out.push_str(&name);
                        if env_var_name != name {
                            out.push_str(&env_var_name);
                        }
                    }
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

/// Extract the variable name from a PowerShell `${env:VARNAME}` brace form.
/// Returns `Some("VARNAME")` when the prefix is `env:` (case-insensitive).
fn powershell_env_provider_var(brace_content: &str) -> Option<String> {
    // "env:" is always 4 ASCII bytes regardless of case. Use `get` to avoid
    // panicking if `brace_content` has a multi-byte UTF-8 codepoint straddling
    // byte index 4 (e.g. `${ℓ:VAR}`); such inputs simply aren't `env:` prefixed.
    const ENV_PREFIX_LEN: usize = 4;
    let prefix = brace_content.get(..ENV_PREFIX_LEN)?;
    if !prefix.eq_ignore_ascii_case("env:") {
        return None;
    }
    let var_name = brace_content.get(ENV_PREFIX_LEN..)?;
    if !var_name.is_empty()
        && var_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some(var_name.to_string())
    } else {
        None
    }
}

/// True when `path` still contains an unresolved shell variable after
/// expansion (a literal `$VAR`/`${VAR}` or cmd-style `%VAR%`). Such a target
/// cannot be proven to stay in the workspace, so callers escalate it.
pub(crate) fn path_has_unresolved_var(path: &str) -> bool {
    if path.contains('$') {
        return true;
    }
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        let mut len = 0usize;
        let mut closed = false;
        while let Some(&nc) = chars.peek() {
            if nc.is_ascii_alphanumeric() || nc == '_' {
                len += 1;
                chars.next();
            } else {
                closed = nc == '%';
                break;
            }
        }
        if len > 0 && closed {
            return true;
        }
    }
    false
}

/// Expand `~`/`~/`, `$VAR`/`${VAR}`, and cmd-style `%VAR%` in a shell path so
/// targets like `~/.bashrc`, `$HOME/x`, and `%USERPROFILE%\x` resolve to their
/// real (out-of-workspace) locations instead of looking like in-workspace
/// relative paths.
fn expand_path_vars(path: &str) -> String {
    let tilde_expanded = if path == "~" {
        home_dir().unwrap_or_else(|| path.to_string())
    } else if let Some(rest) = path.strip_prefix("~/") {
        match home_dir() {
            Some(home) => format!("{}/{}", home.trim_end_matches('/'), rest),
            None => path.to_string(),
        }
    } else {
        path.to_string()
    };
    expand_env_vars(&tilde_expanded)
}

fn push_write_target(raw: &str, out: &mut Vec<String>) {
    let raw = raw.trim();
    // `-` is stdin/stdout for most of these verbs, not a path; `/dev/*`
    // entries (`/dev/null`, `/dev/stdout`, …) are not real file mutations.
    if raw.is_empty() || raw == "-" || raw.starts_with("/dev/") {
        return;
    }
    let expanded = expand_path_vars(raw);
    if !out.contains(&expanded) {
        out.push(expanded);
    }
}

fn is_shell_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
}

/// `--target-directory=DIR`, `--target-directory DIR`, `-t DIR`, `-tDIR`.
fn target_directory_arg(args: &[String]) -> Option<&str> {
    for (idx, arg) in args.iter().enumerate() {
        if arg == "-t" || arg == "--target-directory" {
            return args.get(idx + 1).map(String::as_str);
        }
        if let Some(rest) = arg.strip_prefix("--target-directory=") {
            return Some(rest);
        }
        if let Some(rest) = arg.strip_prefix("-t")
            && !rest.is_empty()
        {
            return Some(rest);
        }
    }
    None
}

fn collect_verb_write_targets(name: &str, args: &[String], out: &mut Vec<String>) {
    match name {
        // Every non-flag argument is a write target. `ln` is included with
        // both operands: an in-workspace symlink whose *target* points
        // outside (`ln -s /etc/passwd link`) is a sandbox-escape vector, so
        // the outside target must escalate even though the link name itself
        // is in-bounds.
        "tee" | "touch" | "mkdir" | "ln" => {
            for arg in args {
                if !is_shell_flag(arg) {
                    push_write_target(arg, out);
                }
            }
        }
        // Destination is `--target-directory` if present, else the last
        // non-flag operand.
        "cp" | "mv" | "install" => {
            if let Some(dir) = target_directory_arg(args) {
                push_write_target(dir, out);
            } else if let Some(dest) = args.iter().rev().find(|arg| !is_shell_flag(arg)) {
                push_write_target(dest, out);
            }
        }
        // `dd of=PATH` is the write target (`if=` is the read source).
        "dd" => {
            for arg in args {
                if let Some(path) = arg.strip_prefix("of=") {
                    push_write_target(path, out);
                }
            }
        }
        // In-place edit (`-i`/`--in-place`) writes its file operands. When
        // the script is positional (no `-e`/`-f`) it is the first non-flag
        // operand, so drop it.
        "sed" => {
            let in_place = args
                .iter()
                .any(|arg| arg.starts_with("-i") || arg.starts_with("--in-place"));
            if in_place {
                let script_via_flag = args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "-e" | "--expression" | "-f" | "--file"));
                let files: Vec<&String> = args.iter().filter(|arg| !is_shell_flag(arg)).collect();
                let targets: &[&String] = if script_via_flag || files.len() <= 1 {
                    &files
                } else {
                    &files[1..]
                };
                for file in targets {
                    push_write_target(file, out);
                }
            }
        }
        // First non-flag operand is the mode; the rest are files.
        "chmod" => {
            let files: Vec<&String> = args.iter().filter(|arg| !is_shell_flag(arg)).collect();
            for file in files.iter().skip(1) {
                push_write_target(file, out);
            }
        }
        _ => collect_windows_write_targets(name, args, out),
    }
}

/// A cmd.exe switch like `/Y`, `/S`, `/MIR`, `/LOG:file` — distinct from a
/// POSIX path that merely begins with `/` (those contain a path separator, so
/// they are not all alphanumeric).
fn is_cmd_flag(token: &str) -> bool {
    token.starts_with('/')
        && token.len() >= 2
        && token[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ':')
}

/// Value of a PowerShell named parameter (`-Destination dst` / `-Path:dst`,
/// case-insensitive).
fn powershell_named_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    for (idx, arg) in args.iter().enumerate() {
        if let Some((flag, inline)) = arg.split_once(':')
            && names.iter().any(|n| flag.eq_ignore_ascii_case(n))
            && !inline.is_empty()
        {
            return Some(inline);
        }
        if names.iter().any(|n| arg.eq_ignore_ascii_case(n)) {
            return args.get(idx + 1).map(String::as_str);
        }
    }
    None
}

/// PowerShell positional operands: tokens that are neither a `-Parameter` nor
/// the value immediately following a bare `-Parameter`.
fn powershell_positionals(args: &[String]) -> Vec<&str> {
    let mut positionals = Vec::new();
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg.starts_with('-') {
            // A bare `-Param` (no inline `:value`) consumes the next token.
            skip_value = !arg.contains(':');
            continue;
        }
        positionals.push(arg.as_str());
    }
    positionals
}

/// Windows file-mutating verbs (cmd.exe + PowerShell). The tree-sitter-bash
/// parser still tokenises these into a name + args, so we can extract their
/// write targets even though their flag/parameter syntax differs from POSIX.
fn collect_windows_write_targets(name: &str, args: &[String], out: &mut Vec<String>) {
    let lowered = name.to_ascii_lowercase();
    let verb = lowered.strip_suffix(".exe").unwrap_or(&lowered);
    match verb {
        // cmd: `copy SRC DEST`, `move SRC DEST`, `xcopy SRC DEST [/flags]` —
        // destination is the last non-switch operand.
        "copy" | "move" | "xcopy" => {
            if let Some(dest) = args.iter().rev().find(|arg| !is_cmd_flag(arg)) {
                push_write_target(dest, out);
            }
        }
        // `robocopy SOURCE DEST [files] [options]` — the written dir is the
        // second positional.
        "robocopy" => {
            let positional: Vec<&String> = args.iter().filter(|arg| !is_cmd_flag(arg)).collect();
            if let Some(dest) = positional.get(1) {
                push_write_target(dest, out);
            }
        }
        // `md DIR` (cmd alias for mkdir).
        "md" => {
            for arg in args {
                if !is_cmd_flag(arg) {
                    push_write_target(arg, out);
                }
            }
        }
        // PowerShell copy/move: `-Destination` (or the last positional).
        "copy-item" | "move-item" => {
            if let Some(dest) = powershell_named_value(args, &["-destination", "-dest"]) {
                push_write_target(dest, out);
            } else if let Some(dest) = powershell_positionals(args).last() {
                push_write_target(dest, out);
            }
        }
        // PowerShell file writers: `-Path`/`-FilePath`/`-LiteralPath` (or the
        // first positional).
        "set-content" | "add-content" | "out-file" | "new-item" => {
            if let Some(target) =
                powershell_named_value(args, &["-path", "-filepath", "-literalpath"])
            {
                push_write_target(target, out);
            } else if let Some(target) = powershell_positionals(args).first() {
                push_write_target(target, out);
            }
        }
        _ => {}
    }
}
