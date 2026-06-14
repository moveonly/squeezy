use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use squeezy_hooks::HookEvent;
use tracing::warn;

use crate::hooks::{HookFailurePolicy, SkillHookMatcher, SkillHookSpec};

/// Execution context a skill declares in its `SKILL.md` frontmatter.
///
/// `Inline` (the default and current behaviour) injects the skill body
/// into the main turn's instructions, so any tool calls the model issues
/// run on the parent thread. `Fork` is the marker that the skill author
/// expects this body to be dispatched into a clean subagent — the
/// downstream dispatcher (see `F10-cc-disk-loaded-agent-definitions`)
/// will read the field once it lands. Until then this surfaces the
/// declaration so callers can branch on it without re-parsing the
/// frontmatter, and an unknown value (`context: bogus`) is mapped to
/// `Inline` with a `tracing::warn!` rather than rejecting the skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillContextMode {
    #[default]
    Inline,
    Fork,
}

impl SkillContextMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Fork => "fork",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) when_to_use: Option<String>,
    pub(crate) triggers: Vec<String>,
    pub(crate) context_mode: SkillContextMode,
    pub(crate) hooks: BTreeMap<HookEvent, Vec<SkillHookMatcher>>,
}

pub(crate) fn parse_explicit_skill_command(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    let rest = trimmed.strip_prefix("/skill")?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    if !first.is_whitespace() {
        return None;
    }
    let rest = chars.as_str().trim_start();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let task = parts.next().unwrap_or("").trim_start();
    Some((name, task))
}

pub(crate) fn parse_skill_file(
    content: &str,
) -> std::result::Result<(SkillMetadata, String), String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines().skip_while(|line| line.trim().is_empty());
    if lines.next() != Some("---") {
        return Err("missing YAML frontmatter".to_string());
    }

    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
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
    if in_frontmatter {
        return Err("unterminated YAML frontmatter".to_string());
    }
    let metadata = parse_frontmatter(&frontmatter)?;
    Ok((metadata, body.join("\n")))
}

fn parse_frontmatter(lines: &[&str]) -> std::result::Result<SkillMetadata, String> {
    let mut name = None;
    let mut description = None;
    let mut when_to_use = None;
    let mut triggers = Vec::new();
    let mut context_mode = SkillContextMode::Inline;
    let mut hooks: BTreeMap<HookEvent, Vec<SkillHookMatcher>> = BTreeMap::new();
    let mut list_key: Option<&str> = None;
    let mut idx = 0;

    while idx < lines.len() {
        let raw = lines[idx];
        idx += 1;
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(key) = list_key {
            if let Some(item) = trimmed.strip_prefix("- ") {
                if key == "triggers" {
                    triggers.push(unquote(item.trim()).to_string());
                }
                continue;
            }
            list_key = None;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("invalid frontmatter line: {line}"));
        };
        let key = key.trim();
        let value = value.trim();

        // A YAML block scalar header (`|`/`>`, optionally with chomping/indent
        // indicators) means the real value spans the following indented lines.
        // This parser is line-based rather than a full YAML parser, so gather
        // and fold those continuation lines here. It keeps SKILL.md files
        // portable: frontmatter that other agents accept — which commonly wraps
        // a long `description` in a `>-` block — loads here too.
        let block_value;
        let value = if let Some(header) = parse_block_scalar_header(value) {
            let (folded, consumed) = parse_block_scalar(&lines[idx..], header);
            idx += consumed;
            block_value = folded;
            block_value.as_str()
        } else {
            value
        };

        match key {
            "name" => name = Some(unquote(value).to_string()),
            "description" => description = Some(unquote(value).to_string()),
            "when_to_use" => when_to_use = Some(unquote(value).to_string()),
            "triggers" if value.is_empty() => list_key = Some("triggers"),
            "triggers" => triggers.extend(parse_inline_list(value)),
            "context" => {
                context_mode = parse_context_mode(unquote(value));
            }
            "hooks" if value.is_empty() => {
                let consumed = parse_hooks_block(&lines[idx..], &mut hooks);
                idx += consumed;
            }
            _ => {}
        }
    }

    let name = name
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "skill frontmatter requires name".to_string())?;
    let description = description
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "skill frontmatter requires description".to_string())?;
    Ok(SkillMetadata {
        name,
        description,
        when_to_use,
        triggers,
        context_mode,
        hooks,
    })
}

/// Parse the `context:` frontmatter value into a [`SkillContextMode`].
///
/// Only `fork` (case-insensitive) maps to [`SkillContextMode::Fork`].
/// Anything else — including the explicit `inline` literal, an empty
/// string, or a typo like `bogus` — falls back to
/// [`SkillContextMode::Inline`]. Unknown values warn so authors can
/// catch typos without losing the skill.
fn parse_context_mode(value: &str) -> SkillContextMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "inline" => SkillContextMode::Inline,
        "fork" => SkillContextMode::Fork,
        other => {
            warn!(
                target: "squeezy_skills",
                value = %other,
                "unrecognised skill context mode; defaulting to inline"
            );
            SkillContextMode::Inline
        }
    }
}

/// Parse the nested block following a top-level `hooks:` key.
///
/// Returns how many input lines were consumed so the caller can advance
/// its cursor past the block. The block ends at the first non-blank line
/// with zero indentation (a new top-level frontmatter key). Indent is
/// the structural anchor: event names sit at the shallowest indent,
/// `- matcher:` clauses one level deeper, and per-spec key/value pairs
/// inside a matcher's `hooks:` sub-list one level deeper still.
/// Unrecognised event names and hook kinds log a `tracing::warn!` and
/// drop, matching how unknown top-level keys are handled today.
fn parse_hooks_block(rest: &[&str], out: &mut BTreeMap<HookEvent, Vec<SkillHookMatcher>>) -> usize {
    let mut consumed = 0;
    let mut current_event: Option<HookEvent> = None;
    let mut current_matchers: Vec<SkillHookMatcher> = Vec::new();
    let mut event_indent: Option<usize> = None;
    let mut matcher_indent: Option<usize> = None;

    fn flush_event(
        out: &mut BTreeMap<HookEvent, Vec<SkillHookMatcher>>,
        event: &mut Option<HookEvent>,
        matchers: &mut Vec<SkillHookMatcher>,
    ) {
        if !matchers.is_empty()
            && let Some(ev) = event.take()
        {
            out.entry(ev).or_default().append(matchers);
        }
        *event = None;
        matchers.clear();
    }

    for line in rest {
        let raw = line.trim_end();
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            consumed += 1;
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if indent == 0 {
            break;
        }
        consumed += 1;
        let trimmed = raw.trim_start();

        // Establish the event indent on the first non-blank child of
        // the `hooks:` block; any line at that same indent is treated
        // as a new event name.
        let level = event_indent.get_or_insert(indent);
        if indent == *level {
            flush_event(out, &mut current_event, &mut current_matchers);
            matcher_indent = None;
            if let Some((key, value)) = trimmed.split_once(':')
                && value.trim().is_empty()
            {
                match parse_hook_event(key.trim()) {
                    Some(event) => current_event = Some(event),
                    None => warn!(
                        target: "squeezy_skills",
                        event = %key.trim(),
                        "ignoring unknown skill hook event"
                    ),
                }
            }
            continue;
        }

        // A matcher item opens a new hook group under the current
        // event. `- matcher: ...` installs a tool-name filter;
        // `- hooks:` is the documented shorthand for an omitted matcher
        // and therefore matches every payload for the event. The
        // matcher indent is locked on first sight so later
        // `command:`/`once:` lines can be told apart from a sibling
        // matcher reliably.
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some((key, value)) = item.split_once(':')
            && matcher_indent.is_none_or(|m| indent <= m)
        {
            match key.trim() {
                "matcher" => {
                    matcher_indent = Some(indent);
                    let raw_match = unquote(value.trim()).to_string();
                    let matcher = if raw_match.is_empty() || raw_match == "*" {
                        None
                    } else {
                        Some(raw_match)
                    };
                    current_matchers.push(SkillHookMatcher {
                        matcher,
                        hooks: Vec::new(),
                    });
                    continue;
                }
                "hooks" if value.trim().is_empty() => {
                    matcher_indent = Some(indent);
                    current_matchers.push(SkillHookMatcher {
                        matcher: None,
                        hooks: Vec::new(),
                    });
                    continue;
                }
                _ => {}
            }
        }

        // A `- type: command` (or any `- key: value`) at indent
        // strictly greater than the matcher line opens a new spec on
        // the active matcher, then the same line's `type:` is parsed
        // as the spec's first key.
        if let Some(item) = trimmed.strip_prefix("- ")
            && matcher_indent.is_some_and(|m| indent > m)
            && let Some(matcher) = current_matchers.last_mut()
        {
            matcher.hooks.push(SkillHookSpec {
                command: String::new(),
                once: false,
                timeout_secs: None,
                fail_open: true,
                kind_valid: true,
                failure_policy: HookFailurePolicy::Allow,
            });
            if let Some(spec) = matcher.hooks.last_mut() {
                apply_spec_kv(spec, item);
            }
            continue;
        }

        // Plain `key: value` line below a `- type:` opener — apply to
        // the most recent spec on the current matcher.
        if let Some(matcher) = current_matchers.last_mut()
            && let Some(spec) = matcher.hooks.last_mut()
        {
            apply_spec_kv(spec, trimmed);
        }
    }

    flush_event(out, &mut current_event, &mut current_matchers);
    consumed
}

/// Apply a single `key: value` token to an in-progress spec.
fn apply_spec_kv(spec: &mut SkillHookSpec, line: &str) {
    let Some((key, value)) = line.split_once(':') else {
        return;
    };
    let value = unquote(value.trim());
    match key.trim() {
        "command" => spec.command = value.to_string(),
        "once" => spec.once = matches!(value, "true" | "yes" | "1"),
        "timeout" => {
            if let Ok(secs) = value.parse::<u64>() {
                spec.timeout_secs = Some(secs);
            } else {
                warn!(
                    target: "squeezy_skills",
                    value = %value,
                    "ignoring invalid hook timeout value; expected integer seconds"
                );
            }
        }
        "fail_open" => spec.fail_open = matches!(value, "true" | "yes" | "1"),
        "failure_policy" => {
            spec.failure_policy = match value {
                "deny" => HookFailurePolicy::Deny,
                "allow" => HookFailurePolicy::Allow,
                other => {
                    warn!(
                        target: "squeezy_skills",
                        value = %other,
                        "unrecognized failure_policy value; expected \"allow\" or \"deny\", defaulting to allow"
                    );
                    HookFailurePolicy::Allow
                }
            };
        }
        "type" if value == "command" => {
            // Explicit `type: command` — already the default, no-op.
        }
        "type" => {
            warn!(
                target: "squeezy_skills",
                kind = %value,
                "ignoring unsupported skill hook kind; spec will be dropped"
            );
            spec.kind_valid = false;
        }
        _ => {}
    }
}

/// Map a YAML key to a [`HookEvent`]. Accepts the canonical PascalCase
/// names used in [`HookEvent`] plus the `snake_case` aliases produced by
/// serde so frontmatter authors can use either convention.
pub(crate) fn parse_hook_event(name: &str) -> Option<HookEvent> {
    match name {
        "PreTurn" | "pre_turn" => Some(HookEvent::PreTurn),
        "PreToolUse" | "pre_tool_use" => Some(HookEvent::PreToolUse),
        "PostToolUse" | "post_tool_use" => Some(HookEvent::PostToolUse),
        "PostToolUseFailure" | "post_tool_use_failure" => Some(HookEvent::PostToolUseFailure),
        "PostTool" | "post_tool" => Some(HookEvent::PostTool),
        "PreCompact" | "pre_compact" => Some(HookEvent::PreCompact),
        "PostCompact" | "post_compact" => Some(HookEvent::PostCompact),
        "SubagentStart" | "subagent_start" => Some(HookEvent::SubagentStart),
        "SubagentStop" | "subagent_stop" => Some(HookEvent::SubagentStop),
        "PermissionRequest" | "permission_request" => Some(HookEvent::PermissionRequest),
        "PermissionDenied" | "permission_denied" => Some(HookEvent::PermissionDenied),
        "UserPromptSubmit" | "user_prompt_submit" => Some(HookEvent::UserPromptSubmit),
        "SessionStart" | "session_start" => Some(HookEvent::SessionStart),
        "Stop" | "stop" => Some(HookEvent::Stop),
        "Setup" | "setup" => Some(HookEvent::Setup),
        _ => None,
    }
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

#[derive(Clone, Copy)]
struct BlockScalarHeader {
    /// `true` for literal (`|`) style, `false` for folded (`>`).
    literal: bool,
    chomp: BlockChomp,
}

#[derive(Clone, Copy)]
enum BlockChomp {
    /// `-`: strip every trailing line break.
    Strip,
    /// default: keep a single trailing line break.
    Clip,
    /// `+`: keep all trailing line breaks.
    Keep,
}

/// Parse a YAML block scalar header such as `>`, `>-`, `|`, `|+`, or `|2-`.
///
/// Returns `None` when `value` is an ordinary scalar (the common case), so the
/// caller falls back to treating the rest of the line as the value. The
/// optional indentation-indicator digit is accepted but ignored — block
/// indentation is detected from the first content line instead.
fn parse_block_scalar_header(value: &str) -> Option<BlockScalarHeader> {
    let mut chars = value.chars();
    let literal = match chars.next()? {
        '|' => true,
        '>' => false,
        _ => return None,
    };
    let mut chomp = BlockChomp::Clip;
    for ch in chars {
        match ch {
            '-' => chomp = BlockChomp::Strip,
            '+' => chomp = BlockChomp::Keep,
            c if c.is_ascii_digit() => {} // explicit indentation indicator: ignored
            _ => return None,
        }
    }
    Some(BlockScalarHeader { literal, chomp })
}

/// Collect the indented continuation lines of a block scalar, returning the
/// folded/literal text and the number of lines consumed.
///
/// Block indentation is taken from the first non-blank line; a later non-blank
/// line indented less than that ends the block (and is not consumed, so the
/// caller reparses it as the next key). Folded (`>`) style joins consecutive
/// non-blank lines with a single space and turns blank lines into newlines;
/// literal (`|`) style preserves line breaks verbatim.
fn parse_block_scalar(lines: &[&str], header: BlockScalarHeader) -> (String, usize) {
    let mut consumed = 0;
    let mut block_indent: Option<usize> = None;
    let mut content: Vec<String> = Vec::new();

    for raw in lines {
        let indent = raw.len() - raw.trim_start().len();
        if raw.trim().is_empty() {
            content.push(String::new());
            consumed += 1;
            continue;
        }
        match block_indent {
            Some(bi) if indent < bi => break,
            Some(_) => {}
            None => block_indent = Some(indent),
        }
        content.push(strip_leading_spaces(raw, block_indent.unwrap_or(indent)));
        consumed += 1;
    }

    // Count of trailing blank lines drives chomping; the lines themselves stay
    // in `consumed` so the caller's cursor skips past them.
    let mut trailing_blanks = 0;
    while matches!(content.last(), Some(line) if line.is_empty()) {
        content.pop();
        trailing_blanks += 1;
    }

    let mut folded = String::new();
    if header.literal {
        folded = content.join("\n");
    } else {
        let mut at_start = true;
        for line in &content {
            if line.is_empty() {
                folded.push('\n');
                at_start = true;
            } else {
                if !at_start {
                    folded.push(' ');
                }
                folded.push_str(line);
                at_start = false;
            }
        }
    }

    match header.chomp {
        BlockChomp::Strip => {}
        BlockChomp::Clip if !folded.is_empty() => folded.push('\n'),
        BlockChomp::Clip => {}
        BlockChomp::Keep if !folded.is_empty() => {
            for _ in 0..trailing_blanks + 1 {
                folded.push('\n');
            }
        }
        BlockChomp::Keep => {}
    }
    (folded, consumed)
}

/// Remove up to `n` leading space/tab characters from `raw`, preserving any
/// indentation deeper than the block's base indent (relevant for literal
/// blocks). Stops early at the first non-whitespace character.
fn strip_leading_spaces(raw: &str, n: usize) -> String {
    // `count` is the leading-whitespace char position; every char before the
    // break is a space/tab, so it equals the number stripped so far. Defaults
    // to the full length when the line is entirely whitespace shorter than `n`.
    let mut start = raw.len();
    for (count, (i, ch)) in raw.char_indices().enumerate() {
        if count >= n || (ch != ' ' && ch != '\t') {
            start = i;
            break;
        }
    }
    raw[start..].to_string()
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

pub(crate) fn is_valid_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
}

pub(crate) fn input_matches_trigger(lowered_input: &str, trigger: &str) -> bool {
    let needle = trigger.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    let bytes = lowered_input.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut cursor = 0;
    while cursor + needle_bytes.len() <= bytes.len() {
        let Some(rel) = lowered_input[cursor..].find(needle.as_str()) else {
            return false;
        };
        let start = cursor + rel;
        let end = start + needle_bytes.len();
        let prev_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let next_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if prev_ok && next_ok {
            return true;
        }
        // `start` is a `find` offset and therefore a valid char boundary, so
        // `lowered_input[start..]` is safe. Advance past the first char of the
        // match (one byte for ASCII) instead of a fixed `+ 1`, which could land
        // inside a multi-byte UTF-8 character and panic when the next iteration
        // slices `lowered_input[cursor..]`.
        cursor = start
            + lowered_input[start..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
#[path = "frontmatter_tests.rs"]
mod tests;
