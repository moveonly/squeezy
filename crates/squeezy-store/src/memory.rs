//! Model-curated, file-based cross-session memory.
//!
//! Each memory is one fact in its own topic file (`<base>/memory/<slug>.md`)
//! carrying YAML frontmatter (`name`, `description`, `metadata.type`), pointed
//! to by a one-line entry in the `<base>/MEMORY.md` index the agent stitches
//! into the system prompt. Memory is split across two bases by *scope*, and
//! scope follows the memory `type` — the model never picks a location directly:
//!
//! - **global** (`~/.squeezy/`): `user` + `feedback` — about the user, useful
//!   in every project.
//! - **project** (`<workspace>/.squeezy/`): `project` + `reference` — about
//!   this repository.
//!
//! [`Memory`] resolves both bases and routes each operation; the `memory` tool
//! and the auto-extraction pass are thin wrappers over it. See
//! `docs/internal/MEMORY_SCOPE.md`.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use squeezy_core::{Result, SqueezyError};

use crate::fs_util;

/// Largest topic-file body we accept, in bytes. A memory is one paragraph
/// about a single fact; anything larger is a sign it should be split.
pub const MAX_MEMORY_BODY_BYTES: usize = 8_192;

/// Longest slug we accept. Keeps file names tidy and the index scannable.
pub const MAX_MEMORY_NAME_CHARS: usize = 64;

/// Where a memory lives. Derived from [`MemoryType`]; never chosen directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// `~/.squeezy/` — shared across every project for this user.
    Global,
    /// `<workspace>/.squeezy/` — local to the current repository.
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// The four memory kinds, mirroring the system-prompt taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Who the user is — role, expertise, working preferences. (global)
    User,
    /// Guidance on how to approach work — corrections and confirmations. (global)
    Feedback,
    /// Ongoing work, goals, or decisions not derivable from code or git. (project)
    Project,
    /// Pointers to where information lives in external systems. (project)
    Reference,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "user" => Some(MemoryType::User),
            "feedback" => Some(MemoryType::Feedback),
            "project" => Some(MemoryType::Project),
            "reference" => Some(MemoryType::Reference),
            _ => None,
        }
    }

    /// Scope follows type: who-you-are facts (`user`, `feedback`) are global;
    /// this-repo facts (`project`, `reference`) are project-local. This is the
    /// whole "how do we decide global vs project" answer — deterministic, no
    /// per-memory judgment call.
    pub fn scope(self) -> Scope {
        match self {
            MemoryType::User | MemoryType::Feedback => Scope::Global,
            MemoryType::Project | MemoryType::Reference => Scope::Project,
        }
    }
}

/// A saved memory's resolved location and on-disk size.
#[derive(Debug, Clone)]
pub struct SavedMemory {
    pub name: String,
    pub scope: Scope,
    pub path: PathBuf,
    pub bytes: usize,
}

/// One row of [`Memory::list`]: the slug, its scope, and frontmatter.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEntry {
    pub name: String,
    pub scope: Scope,
    pub description: Option<String>,
    pub memory_type: Option<String>,
    pub bytes: u64,
}

/// A two-scope memory store. Holds a global base (`~/.squeezy`) and an optional
/// project base (`<workspace>/.squeezy`); all operations route through here so
/// scope selection, cross-process locking, and the index invariant stay in one
/// place.
#[derive(Debug, Clone)]
pub struct Memory {
    global: Option<PathBuf>,
    project: Option<PathBuf>,
}

impl Memory {
    /// Resolve both bases. The global base is `~/.squeezy`; the project base is
    /// `<workspace_root>/.squeezy` when a workspace is given.
    pub fn new(workspace_root: Option<&Path>) -> Self {
        Self {
            global: fs_util::user_squeezy_dir(),
            project: workspace_root.map(|root| root.join(".squeezy")),
        }
    }

    /// Global-only store (no project scope) — for callers without a workspace.
    pub fn global_only() -> Self {
        Self::new(None)
    }

    fn base(&self, scope: Scope) -> Option<&Path> {
        match scope {
            Scope::Global => self.global.as_deref(),
            Scope::Project => self.project.as_deref(),
        }
    }

    /// Bases to search for reads/deletes/lists, project first so it shadows a
    /// same-named global memory.
    fn search_bases(&self) -> Vec<(Scope, &Path)> {
        let mut bases = Vec::new();
        if let Some(project) = self.project.as_deref() {
            bases.push((Scope::Project, project));
        }
        if let Some(global) = self.global.as_deref() {
            bases.push((Scope::Global, global));
        }
        bases
    }

    /// Write (or overwrite) a memory, routing it to the base its `type` implies.
    /// Overwriting a same-named memory in that scope replaces it wholesale.
    pub fn save(
        &self,
        name: &str,
        ty: MemoryType,
        description: &str,
        body: &str,
        title: Option<&str>,
        hook: Option<&str>,
    ) -> Result<SavedMemory> {
        let scope = ty.scope();
        let Some(base) = self.base(scope) else {
            return Err(missing_base_err("save", scope));
        };
        if scope == Scope::Project {
            // Keep the project's `.squeezy/` out of git so memories never show
            // up as untracked noise in the user's working tree.
            ensure_gitignore(base);
        }
        save_in(base, name, ty, description, body, title, hook)
    }

    /// Delete a memory from every scope it appears in. Returns whether anything
    /// was removed.
    pub fn delete(&self, name: &str) -> Result<bool> {
        let name = canonical_name(name)?;
        let mut removed = false;
        let mut first_error = None;
        for (_, base) in self.search_bases() {
            match delete_in(base, &name) {
                Ok(scope_removed) => {
                    removed |= scope_removed;
                }
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        if removed {
            return Ok(true);
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(removed)
    }

    /// List every memory across both scopes, sorted by scope then name.
    pub fn list(&self) -> Result<Vec<MemoryEntry>> {
        let mut out = Vec::new();
        for (scope, base) in self.search_bases() {
            out.extend(list_in(base, scope)?);
        }
        out.sort_by(|a, b| (a.scope.as_str(), &a.name).cmp(&(b.scope.as_str(), &b.name)));
        Ok(out)
    }

    /// Read one memory's raw contents, searching project then global. Returns
    /// the scope it was found in alongside the body.
    pub fn read(&self, name: &str) -> Result<Option<(Scope, String)>> {
        let name = canonical_name(name)?;
        for (scope, base) in self.search_bases() {
            if let Some(body) = read_in(base, &name)? {
                return Ok(Some((scope, body)));
            }
        }
        Ok(None)
    }

    /// Raw index file for a scope. `None` when never written or the base is
    /// unavailable.
    pub fn index(&self, scope: Scope) -> Result<Option<String>> {
        match self.base(scope) {
            Some(base) => read_index_in(base),
            None => Ok(None),
        }
    }

    pub fn global_index(&self) -> Result<Option<String>> {
        self.index(Scope::Global)
    }

    pub fn project_index(&self) -> Result<Option<String>> {
        self.index(Scope::Project)
    }
}

/// Validate (already-lowercased) slug: non-empty, bounded, and restricted to a
/// traversal-proof charset so a memory can never escape its `memory/` dir.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(SqueezyError::Agent("memory name must not be empty".into()));
    }
    if name.chars().count() > MAX_MEMORY_NAME_CHARS {
        return Err(SqueezyError::Agent(format!(
            "memory name {name:?} is too long (max {MAX_MEMORY_NAME_CHARS} chars)"
        )));
    }
    if name == "." || name == ".." {
        return Err(SqueezyError::Agent(
            "memory name must not be a bare path component".into(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(SqueezyError::Agent(format!(
            "memory name {name:?} may only contain lowercase letters, digits, '-' and '_' \
             (e.g. prefers-bun-over-npm)"
        )));
    }
    Ok(())
}

fn canonical_name(name: &str) -> Result<String> {
    let canonical = name.trim().to_ascii_lowercase();
    validate_name(&canonical)?;
    Ok(canonical)
}

fn memory_subdir(base: &Path) -> PathBuf {
    base.join("memory")
}

fn index_path_in(base: &Path) -> PathBuf {
    base.join("MEMORY.md")
}

fn topic_path_in(base: &Path, name: &str) -> PathBuf {
    memory_subdir(base).join(format!("{name}.md"))
}

/// Acquire an exclusive advisory lock serializing the index read-modify-write
/// for `base` across processes (multiple sessions / forks can save at once).
/// The lock is a sidecar file in `base` and releases when the handle drops.
fn index_lock_in(base: &Path) -> Result<File> {
    fs::create_dir_all(base).map_err(SqueezyError::Io)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(base.join(".memory.lock"))
        .map_err(SqueezyError::Io)?;
    file.lock_exclusive().map_err(SqueezyError::Io)?;
    Ok(file)
}

fn save_in(
    base: &Path,
    name: &str,
    ty: MemoryType,
    description: &str,
    body: &str,
    title: Option<&str>,
    hook: Option<&str>,
) -> Result<SavedMemory> {
    let name = canonical_name(name)?;
    let description = sanitize_one_line(description);
    let body = body.trim();
    if description.is_empty() {
        return Err(SqueezyError::Agent(
            "memory description must not be empty".into(),
        ));
    }
    if body.is_empty() {
        return Err(SqueezyError::Agent("memory body must not be empty".into()));
    }
    if body.len() > MAX_MEMORY_BODY_BYTES {
        return Err(SqueezyError::Agent(format!(
            "memory body is {} bytes; cap is {MAX_MEMORY_BODY_BYTES} \
             (keep one fact per memory and split the rest into separate files)",
            body.len()
        )));
    }
    // Hold the index lock across the whole save so the topic file and its index
    // pointer land together and no concurrent session loses an update.
    let _lock = index_lock_in(base)?;
    let path = topic_path_in(base, &name);
    let contents = render_topic_file(&name, ty, &description, body);
    fs_util::write_bytes_atomically(&path, contents.as_bytes()).map_err(SqueezyError::Io)?;

    let title = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| title_from_name(&name));
    let hook = hook
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&description);
    upsert_index_entry_in(base, &name, &title, hook)?;

    Ok(SavedMemory {
        bytes: contents.len(),
        name,
        scope: ty.scope(),
        path,
    })
}

fn delete_in(base: &Path, name: &str) -> Result<bool> {
    let _lock = index_lock_in(base)?;
    let mut removed = false;

    let topic = topic_path_in(base, name);
    if topic.exists() {
        fs::remove_file(&topic).map_err(SqueezyError::Io)?;
        removed = true;
    }

    let index = index_path_in(base);
    if let Ok(existing) = fs::read_to_string(&index) {
        let marker = index_marker(name);
        let total = existing.lines().count();
        let kept: Vec<&str> = existing.lines().filter(|l| !l.contains(&marker)).collect();
        if kept.len() != total {
            let mut out = kept.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            fs_util::write_bytes_atomically(&index, out.as_bytes()).map_err(SqueezyError::Io)?;
            removed = true;
        }
    }

    Ok(removed)
}

fn list_in(base: &Path, scope: Scope) -> Result<Vec<MemoryEntry>> {
    let dir = memory_subdir(base);
    let read_dir = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(SqueezyError::Io(err)),
    };
    let mut out = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(SqueezyError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let (description, memory_type) =
            parse_frontmatter(&fs::read_to_string(&path).unwrap_or_default());
        out.push(MemoryEntry {
            name: stem.to_string(),
            scope,
            description,
            memory_type,
            bytes,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn read_in(base: &Path, name: &str) -> Result<Option<String>> {
    match fs::read_to_string(topic_path_in(base, name)) {
        Ok(body) => Ok(Some(body)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(SqueezyError::Io(err)),
    }
}

fn read_index_in(base: &Path) -> Result<Option<String>> {
    match fs::read_to_string(index_path_in(base)) {
        Ok(body) => Ok(Some(body)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(SqueezyError::Io(err)),
    }
}

fn render_topic_file(name: &str, ty: MemoryType, description: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: {ty}\n---\n\n{body}\n",
        ty = ty.as_str(),
    )
}

fn index_marker(name: &str) -> String {
    format!("](memory/{name}.md)")
}

fn render_index_line(name: &str, title: &str, hook: &str) -> String {
    format!(
        "- [{title}](memory/{name}.md) — {hook}",
        title = sanitize_index_text(title),
        hook = sanitize_index_text(hook),
    )
}

fn upsert_index_entry_in(base: &Path, name: &str, title: &str, hook: &str) -> Result<()> {
    let path = index_path_in(base);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let marker = index_marker(name);
    let new_line = render_index_line(name, title, hook);

    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.contains(&marker) {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        if lines.is_empty() {
            lines.push("# Memory index".to_string());
            lines.push(String::new());
        }
        lines.push(new_line);
    }

    let mut out = lines.join("\n");
    out.push('\n');
    fs_util::write_bytes_atomically(&path, out.as_bytes()).map_err(SqueezyError::Io)
}

/// Best-effort: drop a `.gitignore` (`*`) into a project's `.squeezy/` so its
/// memory + cache never pollute the user's `git status`. Only written if absent.
fn ensure_gitignore(base: &Path) {
    let path = base.join(".gitignore");
    if !path.exists() {
        let _ = fs::create_dir_all(base);
        let _ = fs::write(&path, "# Squeezy local state — do not commit\n*\n");
    }
}

/// Pull `description` and `type` out of a leading `---` frontmatter block.
fn parse_frontmatter(body: &str) -> (Option<String>, Option<String>) {
    let mut description = None;
    let mut memory_type = None;
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("description:") {
            description = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("type:") {
            memory_type = Some(rest.trim().to_string());
        }
    }
    (description, memory_type)
}

/// Title-case a slug for the index pointer: `prefers-bun` -> `Prefers bun`.
fn title_from_name(name: &str) -> String {
    let words = name.replace(['-', '_'], " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => words,
    }
}

/// Collapse all internal whitespace (including newlines) to single spaces.
fn sanitize_one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sanitize text destined for a single-line index pointer: collapse whitespace
/// AND strip markdown link brackets. Stripping `[` / `]` is load-bearing — it
/// guarantees user-supplied `title` / `hook` text can never forge a
/// `](memory/<other>.md)` link target, so the per-memory marker stays
/// unambiguous and `delete` / upsert can never match the wrong line.
fn sanitize_index_text(text: &str) -> String {
    sanitize_one_line(text).replace(['[', ']'], "")
}

fn missing_base_err(op: &str, scope: Scope) -> SqueezyError {
    match scope {
        Scope::Global => SqueezyError::Agent(format!(
            "memory {op} requires a user profile directory ({})",
            fs_util::user_squeezy_dir_detail()
        )),
        Scope::Project => SqueezyError::Agent(format!(
            "memory {op}: no project directory is available for project-scoped memory"
        )),
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
