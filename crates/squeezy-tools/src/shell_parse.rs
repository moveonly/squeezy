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
    let parser_backed = parsed.is_some();
    let dynamic = parsed.as_ref().is_some_and(|parsed| parsed.dynamic);
    let heredoc_prefix = parsed.as_ref().is_some_and(|parsed| parsed.heredoc_prefix);
    let raw_segments = parsed
        .as_ref()
        .map(|parsed| parsed.segments.clone())
        .filter(|segments| !segments.is_empty())
        .unwrap_or_else(|| shell_segments(&normalized));
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
            // Walk past flag tokens; if any flag contains `c`, the next
            // positional argument is the script we want to surface.
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if let Some(flag_body) = tok.strip_prefix('-') {
                    if flag_body.contains('c') {
                        let script = tokens.get(idx + 1)?;
                        return Some(dequote_token(script).to_string());
                    }
                    idx += 1;
                } else {
                    break;
                }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
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
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        _ => None,
    }
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
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    if matches!(
        first,
        "rm" | "rmdir" | "dd" | "truncate" | "shred" | "chown" | "sudo"
    ) {
        return true;
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
        return true;
    }
    if destructive_git_pair(&tokens) || destructive_two_word_command(&tokens) {
        return true;
    }
    if shell_segment_has_destructive_redirect(segment) {
        return true;
    }
    if is_destructive_windows_segment(segment) {
        return true;
    }
    false
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
/// `2>&1`, `>&-`, and any `>` that appears inside single or double quotes.
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
                return true;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
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
    matches!(
        shell_command_prefix(segment).as_str(),
        "git status" | "git diff" | "git log" | "git show" | "git branch"
    )
}

pub(crate) fn is_read_only_shell_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "file" | "stat" | "du" | "grep" | "rg"
    )
}
