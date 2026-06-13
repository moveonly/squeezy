use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use squeezy_core::{
    PermissionAction, PermissionRule, PermissionRuleSource, escape_toml_basic_string,
};

pub(crate) fn persist_permission_rule(path: &Path, rule: &PermissionRule) -> io::Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = lock_path(path);
    let _lock = FileLock::acquire(&lock_path, Duration::from_secs(5))?;

    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if contains_rule(&existing, rule) {
        return Ok(false);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(&format_permission_rule(rule));

    let tmp = path.with_extension("toml.tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(next.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(true)
}

fn format_permission_rule(rule: &PermissionRule) -> String {
    let reason = rule
        .reason
        .as_deref()
        .unwrap_or("added from approval prompt");
    format!(
        "[[permissions.rules]]\ncapability = {}\ntarget = {}\naction = {}\nsource = {}\nreason = {}\n",
        escape_toml_basic_string(&rule.capability),
        escape_toml_basic_string(&rule.target),
        escape_toml_basic_string(rule.action.as_str()),
        escape_toml_basic_string(rule.source.as_str()),
        escape_toml_basic_string(reason),
    )
}

fn contains_rule(text: &str, rule: &PermissionRule) -> bool {
    let mut current: Option<RuleKey> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[[permissions.rules]]" {
            if current
                .as_ref()
                .is_some_and(|key| rule_key_matches(key, rule))
            {
                return true;
            }
            current = Some(RuleKey::default());
            continue;
        }
        let Some(candidate) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let value = parse_basic_string(value.trim());
        match key.trim() {
            "capability" => candidate.capability = value,
            "target" => candidate.target = value,
            "action" => {
                if let Some(action) = PermissionAction::parse(&value) {
                    candidate.action = action;
                }
            }
            "source" => {
                if let Some(source) = PermissionRuleSource::parse(&value) {
                    candidate.source = source;
                }
            }
            _ => {}
        }
    }
    current
        .as_ref()
        .is_some_and(|key| rule_key_matches(key, rule))
}

fn rule_key_matches(key: &RuleKey, rule: &PermissionRule) -> bool {
    key.capability == rule.capability
        && key.target == rule.target
        && key.action == rule.action
        && key.source == rule.source
}

#[derive(Debug)]
struct RuleKey {
    capability: String,
    target: String,
    action: PermissionAction,
    source: PermissionRuleSource,
}

impl Default for RuleKey {
    fn default() -> Self {
        Self {
            capability: String::new(),
            target: String::new(),
            action: PermissionAction::Ask,
            source: PermissionRuleSource::User,
        }
    }
}

/// Inverse of [`escape_toml_basic_string`]: strip at most one enclosing quote on
/// each side, then unescape in a single left-to-right scan. Unlike a greedy
/// `trim_matches('"')` plus ordered `replace` calls, this round-trips targets
/// whose value itself contains quotes or backslashes, so `contains_rule` can
/// recognize an already-persisted identical rule.
fn parse_basic_string(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix('"').unwrap_or(value);
    let value = value.strip_suffix('"').unwrap_or(value);

    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        // Not a valid \uXXXX escape; preserve the raw input.
                        out.push('\\');
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            // Unknown escape: keep the backslash and the following char verbatim.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            // Trailing lone backslash: keep it verbatim.
            None => out.push('\\'),
        }
    }
    out
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("toml.lock")
}

struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: &Path, timeout: Duration) -> io::Result<Self> {
        let started = Instant::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if started.elapsed() >= timeout {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!("timed out waiting for {}", path.display()),
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
#[path = "permission_persist_tests.rs"]
mod tests;
