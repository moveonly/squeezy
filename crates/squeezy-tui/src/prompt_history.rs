//! Rolling 100-entry prompt history for the composer.
//!
//! Up / Down at the composer cycle through previously-submitted prompts.
//! Consecutive duplicates are dropped at push-time so repeatedly resending
//! the same prompt does not eat the recall buffer.
//!
//! When `[tui].persist_prompt_history = true` is set in `settings.toml`,
//! the buffer mirrors to a flat newline-delimited file on disk (default
//! `~/.squeezy/prompt_history`, with an XDG-compatible
//! `$XDG_DATA_HOME/squeezy/prompt_history` fallback). Multi-line prompts
//! are encoded with simple `\\` / `\n` / `\r` escapes so each prompt
//! still occupies one line and the file stays grep-friendly.
//!
//! Disk persistence is best-effort: I/O errors are logged via `tracing`
//! and the in-memory buffer keeps functioning so a borked history file
//! never prevents the TUI from starting or from accepting new prompts.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Maximum number of prompts retained. Matches clear-code's recall
/// buffer; large enough to span an afternoon of work, small enough to
/// keep the file readable and the scrollback cheap.
pub(crate) const DEFAULT_PROMPT_HISTORY_CAPACITY: usize = 100;

/// In-memory ring of recent prompts with optional disk mirror. Reads
/// (`len`, `get`, `last`) are O(1); pushes amortise to O(1) and only
/// touch disk when persistence is enabled.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptHistory {
    entries: VecDeque<String>,
    capacity: usize,
    persist_path: Option<PathBuf>,
}

impl PromptHistory {
    /// In-memory only history; never touches disk. Used when the user
    /// has `persist_prompt_history = false` (the default) and in unit
    /// tests so the real `~/.squeezy/prompt_history` file stays out
    /// of CI.
    pub(crate) fn in_memory(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            persist_path: None,
        }
    }

    /// Disk-backed history. Loads any existing entries on construction
    /// (truncated to the most recent `capacity` lines) and mirrors
    /// every subsequent `push` back to the file.
    pub(crate) fn with_persistence(capacity: usize, path: PathBuf) -> Self {
        let capacity = capacity.max(1);
        let mut history = Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            persist_path: Some(path),
        };
        history.load_from_disk();
        history
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn last(&self) -> Option<&str> {
        self.entries.back().map(String::as_str)
    }

    /// Push a prompt onto the history. Trims leading/trailing whitespace
    /// only to detect emptiness — the raw value is what's stored so
    /// recall reproduces the user's draft verbatim. Consecutive
    /// duplicates collapse so re-sending a prompt doesn't pollute the
    /// recall list. When the capacity cap is reached the oldest entry
    /// is dropped before the new one lands.
    pub(crate) fn push(&mut self, prompt: String) {
        if prompt.trim().is_empty() {
            return;
        }
        if self.entries.back().is_some_and(|last| last == &prompt) {
            return;
        }
        let dropped_oldest = if self.entries.len() == self.capacity {
            self.entries.pop_front();
            true
        } else {
            false
        };
        self.entries.push_back(prompt.clone());
        if let Some(path) = self.persist_path.clone() {
            let result = if dropped_oldest {
                rewrite_disk(&path, &self.entries)
            } else {
                append_disk(&path, &prompt)
            };
            if let Err(err) = result {
                tracing::warn!(
                    target: "squeezy_tui::prompt_history",
                    error = %err,
                    path = %path.display(),
                    "failed to persist prompt history",
                );
            }
        }
    }

    fn load_from_disk(&mut self) {
        let Some(path) = self.persist_path.as_deref() else {
            return;
        };
        match read_entries(path) {
            Ok(loaded) => {
                for entry in loaded {
                    if entry.trim().is_empty() {
                        continue;
                    }
                    if self.entries.back().is_some_and(|last| last == &entry) {
                        continue;
                    }
                    if self.entries.len() == self.capacity {
                        self.entries.pop_front();
                    }
                    self.entries.push_back(entry);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    target: "squeezy_tui::prompt_history",
                    error = %err,
                    path = %path.display(),
                    "failed to load prompt history",
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn persist_path(&self) -> Option<&Path> {
        self.persist_path.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

fn encode(prompt: &str) -> String {
    let mut buf = String::with_capacity(prompt.len());
    for ch in prompt.chars() {
        match ch {
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            _ => buf.push(ch),
        }
    }
    buf
}

fn decode(line: &str) -> String {
    let mut buf = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            buf.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => buf.push('\n'),
            Some('r') => buf.push('\r'),
            Some('\\') => buf.push('\\'),
            Some(other) => {
                buf.push('\\');
                buf.push(other);
            }
            None => buf.push('\\'),
        }
    }
    buf
}

fn read_entries(path: &Path) -> io::Result<Vec<String>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        out.push(decode(&line?));
    }
    Ok(out)
}

fn append_disk(path: &Path, prompt: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", encode(prompt))?;
    Ok(())
}

fn rewrite_disk(path: &Path, entries: &VecDeque<String>) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    let mut writer = BufWriter::new(file);
    for entry in entries {
        writeln!(writer, "{}", encode(entry))?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
#[path = "prompt_history_tests.rs"]
mod tests;
