//! User-authored slash macros sourced from frontmatter `.md` files.
//!
//! A prompt template is a markdown file with YAML frontmatter:
//!
//! ```text
//! ---
//! description: Summarize a file
//! argument-hint: <path>
//! args: [path]
//! ---
//! Summarize the file at {path}.
//! ```
//!
//! Discovery walks `~/.squeezy/prompts/*.md` (user scope) and
//! `<workspace>/.squeezy/prompts/*.md` (project scope). Project entries
//! shadow same-name user entries — the project copy wins so a checked-in
//! prompt is the source of truth for collaborators.
//!
//! Naming uses ASCII alphanumerics plus `-` and `_`; the first
//! character must be alphanumeric. Names are case-sensitive so
//! `/Review` and `/review` are distinct templates, though the macOS
//! case-insensitive default filesystem will keep one from clobbering
//! the other on the same disk. Files whose stem doesn't match are
//! skipped with a `tracing::warn!` rather than failing the load — a
//! malformed template must never prevent valid templates (or the rest
//! of the agent) from loading.
//!
//! Argument substitution supports two interchangeable surfaces so the
//! same template body works whether the author thinks in named slots or
//! in shell-style positional args:
//!
//! - Named: `{name}` resolves to the positional arg at the index where
//!   `name` appears in the `args` schema. `{ARGUMENTS}` resolves to all
//!   args joined with a single space. `{1}`, `{2}`, …  resolve to
//!   positional args (1-indexed, matching shell convention).
//! - Compat: `$1`, `$2`, …, `$@`, `$ARGUMENTS`, `${@:N}`, `${@:N:L}`
//!   use the bash slice syntax. `${@:N}` slices from the Nth
//!   argument onward (1-indexed); `${@:N:L}` slices `L` args starting at
//!   `N`.
//!
//! Tokens that don't resolve are left intact in the rendered output so
//! literal `{not_an_arg}` text in a template body passes through
//! unchanged.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Subdirectory under the workspace root scanned for project-scoped
/// prompt templates.
pub const PROJECT_PROMPTS_DIR: &str = ".squeezy/prompts";

/// Subdirectory under `$HOME` scanned for user-scoped prompt templates.
pub const USER_PROMPTS_SUBPATH: &str = ".squeezy/prompts";

/// Where a [`PromptTemplate`] was loaded from. Project scope shadows
/// user scope when the file stem collides — the project copy wins so
/// teammates see the checked-in version, not a personal override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTemplateSource {
    User,
    Project,
}

impl PromptTemplateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// A single slash macro parsed from a `.md` file in a prompts
/// directory. The body in [`content`](Self::content) is the post-
/// frontmatter portion of the file — argument tokens are still present
/// and substituted at expansion time, not at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    /// Optional usage hint surfaced in the slash menu, e.g. `<path>` or
    /// `<topic> [<branch>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Ordered argument-name schema. Maps positional args (in order
    /// typed by the user) to the names referenced by `{name}` tokens in
    /// the body. Empty when the template only uses `$1`/`$ARGUMENTS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    pub content: String,
    pub source: PromptTemplateSource,
    pub path: PathBuf,
}

/// In-memory catalog of prompt templates, keyed by name. Lookup is
/// cheap (`BTreeMap::get`), so the TUI can call [`expand`](Self::expand)
/// on every slash submission without measurable overhead.
#[derive(Debug, Clone, Default)]
pub struct PromptTemplateCatalog {
    templates: BTreeMap<String, PromptTemplate>,
}

impl PromptTemplateCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Discover templates from the standard locations: `~/.squeezy/prompts/`
    /// (user scope, legacy) and, when available,
    /// `$XDG_CONFIG_HOME/squeezy/prompts/` or
    /// `~/.config/squeezy/prompts/` (user scope, XDG-aware).  Project
    /// entries from `<workspace_root>/.squeezy/prompts/` shadow all
    /// user-scope entries.
    pub fn discover(workspace_root: &Path) -> Self {
        let user_dir = home_prompts_dir();
        // Delegate XDG resolution to squeezy-core to avoid duplicate logic.
        // The core function already deduplicates against the legacy path.
        let xdg_dir = squeezy_core::default_xdg_prompts_dir();
        let project_dir = workspace_root.join(PROJECT_PROMPTS_DIR);
        let mut catalog = Self::default();
        // Legacy path first so it is the lower-priority source.
        if let Some(dir) = user_dir.as_deref() {
            catalog.discover_dir(dir, PromptTemplateSource::User);
        }
        // XDG path: only scanned when it differs from the legacy path.
        if let Some(dir) = xdg_dir.as_deref() {
            catalog.discover_dir(dir, PromptTemplateSource::User);
        }
        catalog.discover_dir(project_dir.as_path(), PromptTemplateSource::Project);
        catalog
    }

    /// Discover templates from explicit `(user, project)` directories.
    /// Either side can be `None` — useful for tests that only want one
    /// scope, and for hosts that disable global discovery (`HOME`
    /// missing on Windows during tests).
    pub fn from_dirs(user_dir: Option<&Path>, project_dir: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        if let Some(dir) = user_dir {
            catalog.discover_dir(dir, PromptTemplateSource::User);
        }
        if let Some(dir) = project_dir {
            catalog.discover_dir(dir, PromptTemplateSource::Project);
        }
        catalog
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn get(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.templates.keys().map(String::as_str).collect()
    }

    pub fn templates(&self) -> impl Iterator<Item = &PromptTemplate> {
        self.templates.values()
    }

    /// Expand a slash-prefixed input into the rendered template body,
    /// or return `None` when the input doesn't match a loaded template.
    ///
    /// Inputs that start with anything other than `/`, or whose head
    /// after `/` doesn't match a template name, return `None`. Argument
    /// parsing follows bash-style quoting so `/review "two words"`
    /// passes a single argument.
    pub fn expand(&self, input: &str) -> Option<String> {
        self.expand_with_info(input).map(|(text, _, _)| text)
    }

    /// Like [`expand`] but also returns `(source, arg_count)` for telemetry.
    /// Returns `None` if the input doesn't match a template.
    pub fn expand_with_info(&self, input: &str) -> Option<(String, PromptTemplateSource, u32)> {
        let trimmed = input.trim_start();
        let rest = trimmed.strip_prefix('/')?;
        let (head, args_str) = match rest.find(char::is_whitespace) {
            Some(idx) => (&rest[..idx], rest[idx..].trim_start()),
            None => (rest, ""),
        };
        if head.is_empty() {
            return None;
        }
        let template = self.templates.get(head)?;
        let arg_values = parse_command_args(args_str);
        let arg_count = arg_values.len() as u32;
        let source = template.source;
        Some((
            substitute_args(&template.content, &arg_values, &template.args),
            source,
            arg_count,
        ))
    }

    fn discover_dir(&mut self, dir: &Path, source: PromptTemplateSource) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                warn!(
                    target: "squeezy_skills::prompt_templates",
                    dir = %dir.display(),
                    error = %error,
                    "skipping prompt template directory due to read error"
                );
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills::prompt_templates",
                        dir = %dir.display(),
                        error = %error,
                        "skipping prompt template entry due to read error"
                    );
                    continue;
                }
            };
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills::prompt_templates",
                        path = %path.display(),
                        error = %error,
                        "skipping prompt template due to file-type error"
                    );
                    continue;
                }
            };
            let is_file = if file_type.is_symlink() {
                fs::metadata(&path).is_ok_and(|m| m.is_file())
            } else {
                file_type.is_file()
            };
            if !is_file {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = name.to_string();
            if !is_valid_template_name(&name) {
                warn!(
                    target: "squeezy_skills::prompt_templates",
                    path = %path.display(),
                    name = %name,
                    "skipping prompt template with invalid name"
                );
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills::prompt_templates",
                        path = %path.display(),
                        error = %error,
                        "skipping prompt template due to read error"
                    );
                    continue;
                }
            };
            let parsed = match parse_template(&content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills::prompt_templates",
                        path = %path.display(),
                        error = %error,
                        "skipping malformed prompt template"
                    );
                    continue;
                }
            };
            if let Some(existing) = self.templates.get(&name)
                && existing.source == PromptTemplateSource::Project
                && source == PromptTemplateSource::User
            {
                continue;
            }
            self.templates.insert(
                name.clone(),
                PromptTemplate {
                    name,
                    description: parsed.description,
                    argument_hint: parsed.argument_hint,
                    args: parsed.args,
                    content: parsed.body,
                    source,
                    path,
                },
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedTemplate {
    description: String,
    argument_hint: Option<String>,
    args: Vec<String>,
    body: String,
}

fn parse_template(content: &str) -> Result<ParsedTemplate, String> {
    let mut iter = content.lines();
    let mut frontmatter: Vec<&str> = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let first = iter.next();
    let mut in_frontmatter = false;
    let mut has_frontmatter = false;
    if first == Some("---") {
        in_frontmatter = true;
        has_frontmatter = true;
    } else if let Some(line) = first {
        body.push(line);
    }
    for line in iter {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    if has_frontmatter && in_frontmatter {
        return Err("unterminated YAML frontmatter".to_string());
    }
    let (description, argument_hint, args) = parse_frontmatter(&frontmatter);
    let body_text = body.join("\n");
    let body_trimmed_start = body_text.trim_start_matches('\n').to_string();
    let description = description.unwrap_or_else(|| infer_description(&body_trimmed_start));
    Ok(ParsedTemplate {
        description,
        argument_hint,
        args,
        body: body_trimmed_start,
    })
}

fn infer_description(body: &str) -> String {
    let Some(line) = body.lines().find(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let line = line.trim();
    if line.chars().count() > 60 {
        let truncated: String = line.chars().take(60).collect();
        format!("{truncated}...")
    } else {
        line.to_string()
    }
}

fn parse_frontmatter(lines: &[&str]) -> (Option<String>, Option<String>, Vec<String>) {
    let mut description = None;
    let mut argument_hint = None;
    let mut args = Vec::new();
    let mut list_key: Option<&str> = None;

    for raw in lines {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(key) = list_key {
            if let Some(item) = trimmed.strip_prefix("- ") {
                if key == "args" {
                    args.push(unquote(item.trim()).to_string());
                }
                continue;
            }
            list_key = None;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "description" => description = Some(unquote(value).to_string()),
            "argument-hint" | "argument_hint" => {
                argument_hint = Some(unquote(value).to_string());
            }
            "args" if value.is_empty() => list_key = Some("args"),
            "args" => args.extend(parse_inline_list(value)),
            _ => {}
        }
    }
    (description, argument_hint, args)
}

fn parse_inline_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return vec![unquote(value).to_string()];
    };
    inner
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(unquote)
        .map(str::to_string)
        .collect()
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn is_valid_template_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
}

/// Split a slash-command argument string into shell-style tokens. Single
/// and double quotes preserve interior whitespace; mismatched closing
/// quotes are tolerated by treating end-of-string as an implicit close,
/// matching how `pi` and shell-script users expect a half-typed quote to
/// behave inside the TUI input.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    for ch in input.chars() {
        match in_quote {
            Some(quote_char) => {
                if ch == quote_char {
                    in_quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    in_quote = Some(ch);
                } else if ch.is_whitespace() {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(ch);
                }
            }
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Render a template body by substituting argument tokens.
///
/// The substitution honours both the `{name}`/`{ARGUMENTS}`/`{N}` curly
/// syntax (used by the task spec) and the
/// `$1`/`$@`/`$ARGUMENTS`/`${@:N[:L]}` shell-style syntax. Tokens that
/// don't resolve to a value are passed through verbatim so authors can
/// keep literal braces or dollar signs in the prompt body.
pub fn substitute_args(content: &str, args: &[String], schema: &[String]) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'$'
            && let Some(consumed) = try_substitute_dollar(&content[i..], args, &mut out)
        {
            i += consumed;
            continue;
        }
        if b == b'{'
            && let Some(consumed) = try_substitute_brace(&content[i..], args, schema, &mut out)
        {
            i += consumed;
            continue;
        }
        // Append the next full UTF-8 codepoint and advance past it. The
        // body is `&str`, so every byte index is on a char boundary or
        // an interior continuation byte we can copy verbatim.
        let next = next_char_boundary(content, i);
        out.push_str(&content[i..next]);
        i = next;
    }
    out
}

fn next_char_boundary(content: &str, start: usize) -> usize {
    let mut i = start + 1;
    while i < content.len() && !content.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Try to substitute a `$…` token. Returns the byte length consumed
/// (including the leading `$`) on success, or `None` when the input at
/// `slice` is not a recognised `$` token.
fn try_substitute_dollar(slice: &str, args: &[String], out: &mut String) -> Option<usize> {
    debug_assert!(slice.starts_with('$'));
    let rest = &slice[1..];
    let first = rest.chars().next()?;
    if first.is_ascii_digit() {
        let digit_bytes = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        let num_str = &rest[..digit_bytes];
        if let Ok(idx) = num_str.parse::<usize>()
            && idx > 0
            && let Some(value) = args.get(idx - 1)
        {
            out.push_str(value);
        }
        return Some(1 + digit_bytes);
    }
    if first == '@' {
        push_joined_args(out, args);
        return Some(1 + '@'.len_utf8());
    }
    if rest.starts_with("ARGUMENTS") {
        push_joined_args(out, args);
        return Some(1 + "ARGUMENTS".len());
    }
    if first == '{' {
        // ${@:N} or ${@:N:L}
        let body = &rest[1..];
        let end = body.find('}')?;
        let spec = body[..end].strip_prefix("@:")?;
        let (start_str, len_str) = match spec.split_once(':') {
            Some((s, l)) => (s, Some(l)),
            None => (spec, None),
        };
        let parsed_start = start_str.parse::<usize>().unwrap_or(1).max(1) - 1;
        let start = parsed_start.min(args.len());
        let slice_args: &[String] = match len_str {
            Some(l) => {
                let len = l.parse::<usize>().unwrap_or(0);
                let end_idx = start.saturating_add(len).min(args.len());
                &args[start..end_idx]
            }
            None => &args[start..],
        };
        push_joined_args(out, slice_args);
        return Some(1 + 1 + end + 1);
    }
    None
}

/// Try to substitute a `{…}` token. Returns the byte length consumed
/// on success.
fn try_substitute_brace(
    slice: &str,
    args: &[String],
    schema: &[String],
    out: &mut String,
) -> Option<usize> {
    debug_assert!(slice.starts_with('{'));
    let rest = &slice[1..];
    let end = rest.find('}')?;
    let name = &rest[..end];
    if name.is_empty() || name.contains('{') || name.chars().any(char::is_whitespace) {
        return None;
    }
    if name == "ARGUMENTS" {
        push_joined_args(out, args);
        return Some(1 + end + 1);
    }
    if let Ok(idx) = name.parse::<usize>() {
        if idx > 0
            && let Some(value) = args.get(idx - 1)
        {
            out.push_str(value);
        }
        return Some(1 + end + 1);
    }
    let pos = schema.iter().position(|n| n == name)?;
    if let Some(value) = args.get(pos) {
        out.push_str(value);
    }
    Some(1 + end + 1)
}

fn push_joined_args(out: &mut String, args: &[String]) {
    for (idx, arg) in args.iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        out.push_str(arg);
    }
}

fn home_prompts_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(USER_PROMPTS_SUBPATH))
}

#[cfg(test)]
#[path = "prompt_templates_tests.rs"]
mod tests;
