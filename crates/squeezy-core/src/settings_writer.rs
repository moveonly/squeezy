//! Persists `SettingsEdit` lists to a TOML file via `toml_edit`.
//!
//! Comments and formatting authored by hand survive a save: the writer only
//! mutates the leaves it was asked to mutate, leaving surrounding decor in
//! place. Writes go through a sibling tempfile + `rename` so a crash mid-write
//! cannot leave a half-written settings file.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

use crate::config_schema::SettingsPath;

/// Which file a save targets. Every scope is written `0o600` (and its parent
/// directory created `0o700`) since any settings file may hold inline secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsScopeKind {
    /// `~/.squeezy/settings.toml`.
    User,
    /// `./squeezy.toml` (workspace root).
    Project,
    /// `~/.squeezy/projects/<hash>/settings.toml`.
    Repo,
}

#[derive(Debug, Clone)]
pub struct SettingsScope {
    pub kind: SettingsScopeKind,
    pub path: PathBuf,
}

impl SettingsScope {
    pub fn user(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: SettingsScopeKind::User,
            path: path.into(),
        }
    }
    pub fn project(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: SettingsScopeKind::Project,
            path: path.into(),
        }
    }
    pub fn repo(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: SettingsScopeKind::Repo,
            path: path.into(),
        }
    }
}

/// One mutation to apply at `path`.
#[derive(Debug, Clone)]
pub struct SettingsEdit {
    pub path: SettingsPath,
    pub op: EditOp,
}

#[derive(Debug, Clone)]
pub enum EditOp {
    SetString(String),
    SetInteger(i64),
    SetFloat(f64),
    SetBool(bool),
    SetArrayOfStrings(Vec<String>),
    /// Removes the leaf. Preceding comments stay; the parent table is kept
    /// even if it becomes empty so user-written section headers survive.
    Unset,
    /// Set a filesystem path leaf (serialized as a TOML string).
    SetPath(PathBuf),
    /// Upsert a keyed table entry — `[<table_path>.<key>]` — and apply
    /// per-child edits inside it. Used for `[mcp.servers.<name>]`. If the
    /// entry exists as an inline `{ ... }` value it is promoted to a full
    /// table so nested edits don't lose sibling keys.
    SetTableEntry {
        table_path: SettingsPath,
        key: String,
        /// Per-child (key, op). Op `Unset` removes the child leaf.
        fields: Vec<(&'static str, EditOp)>,
    },
    /// Remove `[<table_path>.<key>]` entirely. Parent table is kept so
    /// surrounding section headers and comments survive.
    RemoveTableEntry {
        table_path: SettingsPath,
        key: String,
    },
    /// Append one row to `[[<path>]]`. Each `(child_key, op)` populates the
    /// new row. Creates the array of tables if missing.
    AppendArrayOfTables {
        path: SettingsPath,
        fields: Vec<(&'static str, EditOp)>,
    },
    /// Remove the row from `[[<path>]]` whose `columns` slice equals
    /// `values` (string equality on each named column). No-op if no row
    /// matches.
    RemoveArrayOfTablesByMatch {
        path: SettingsPath,
        predicate: ArrayOfTablesMatch,
    },
    /// Set one RGB token override under `[tui.themes.<theme>.colors]`.
    SetThemeColor {
        theme: String,
        token: String,
        rgb: [u8; 3],
    },
    /// Set many RGB token overrides under `[tui.themes.<theme>.colors]`.
    SetThemeColors {
        theme: String,
        colors: Vec<(String, [u8; 3])>,
    },
    /// Remove one RGB token override from `[tui.themes.<theme>.colors]`.
    RemoveThemeColor {
        theme: String,
        token: String,
    },
}

#[derive(Debug, Clone)]
pub struct ArrayOfTablesMatch {
    pub columns: &'static [&'static str],
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub path: PathBuf,
    pub edits_applied: usize,
    pub edits_skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("settings path is empty")]
    EmptyPath,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Parse(#[from] toml_edit::TomlError),
}

pub fn apply_edits(
    scope: &SettingsScope,
    edits: &[SettingsEdit],
) -> Result<WriteOutcome, WriterError> {
    let mut doc = load_document(&scope.path)?;
    let mut applied = 0usize;
    let mut skipped = 0usize;
    for edit in edits {
        // Scalar ops use the outer `path`; structural ops (table entries,
        // arrays of tables) encode their own path inside the op variant.
        if edit.path.is_empty() && requires_outer_path(&edit.op) {
            return Err(WriterError::EmptyPath);
        }
        let changed = apply_one(&mut doc, edit);
        if changed {
            applied += 1;
        } else {
            skipped += 1;
        }
    }
    if applied == 0 {
        return Ok(WriteOutcome {
            path: scope.path.clone(),
            edits_applied: applied,
            edits_skipped: skipped,
        });
    }
    write_atomic(&scope.path, doc.to_string().as_bytes(), &scope.kind)?;
    Ok(WriteOutcome {
        path: scope.path.clone(),
        edits_applied: applied,
        edits_skipped: skipped,
    })
}

fn requires_outer_path(op: &EditOp) -> bool {
    !matches!(
        op,
        EditOp::SetTableEntry { .. }
            | EditOp::RemoveTableEntry { .. }
            | EditOp::AppendArrayOfTables { .. }
            | EditOp::RemoveArrayOfTablesByMatch { .. }
            | EditOp::SetThemeColor { .. }
            | EditOp::SetThemeColors { .. }
            | EditOp::RemoveThemeColor { .. }
    )
}

fn load_document(path: &Path) -> Result<DocumentMut, WriterError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.parse::<DocumentMut>()?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err.into()),
    }
}

fn apply_one(doc: &mut DocumentMut, edit: &SettingsEdit) -> bool {
    match &edit.op {
        EditOp::SetTableEntry {
            table_path,
            key,
            fields,
        } => apply_set_table_entry(doc, table_path, key, fields),
        EditOp::RemoveTableEntry { table_path, key } => {
            apply_remove_table_entry(doc, table_path, key)
        }
        EditOp::AppendArrayOfTables { path, fields } => {
            apply_append_array_of_tables(doc, path, fields)
        }
        EditOp::RemoveArrayOfTablesByMatch { path, predicate } => {
            apply_remove_array_of_tables_by_match(doc, path, predicate)
        }
        EditOp::SetThemeColor { theme, token, rgb } => {
            apply_set_theme_color(doc, theme, token, *rgb)
        }
        EditOp::SetThemeColors { theme, colors } => apply_set_theme_colors(doc, theme, colors),
        EditOp::RemoveThemeColor { theme, token } => apply_remove_theme_color(doc, theme, token),
        _ => {
            let (leaf, parents) = edit.path.split_last().expect("path empty check above");
            apply_scalar(doc.as_table_mut(), parents, leaf, &edit.op)
        }
    }
}

fn apply_scalar(root: &mut Table, parents: &[&str], leaf: &str, op: &EditOp) -> bool {
    if matches!(op, EditOp::Unset) {
        let Some(parent) = descend_existing_table(root, parents) else {
            return false;
        };
        return parent.remove(leaf).is_some();
    }

    let parent = descend_or_create_table(root, parents);
    match op {
        EditOp::SetString(s) => set_leaf(parent, leaf, value(s.as_str())),
        EditOp::SetInteger(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetFloat(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetBool(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetPath(p) => set_leaf(parent, leaf, value(p.display().to_string().as_str())),
        EditOp::SetArrayOfStrings(items) => {
            let arr = items.iter().map(String::as_str).collect::<Array>();
            set_leaf(parent, leaf, Item::Value(Value::Array(arr)))
        }
        EditOp::Unset => unreachable!("handled before parent table creation"),
        EditOp::SetTableEntry { .. }
        | EditOp::RemoveTableEntry { .. }
        | EditOp::AppendArrayOfTables { .. }
        | EditOp::RemoveArrayOfTablesByMatch { .. }
        | EditOp::SetThemeColor { .. }
        | EditOp::SetThemeColors { .. }
        | EditOp::RemoveThemeColor { .. } => unreachable!("dispatched in apply_one"),
    }
}

fn apply_set_table_entry(
    doc: &mut DocumentMut,
    table_path: SettingsPath,
    key: &str,
    fields: &[(&'static str, EditOp)],
) -> bool {
    let parent = descend_or_create_table(doc.as_table_mut(), table_path);
    // Get-or-create the entry as a full table (promote from inline if needed).
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    } else if let Some(Item::Value(Value::InlineTable(inline))) = parent.get(key) {
        let mut promoted = Table::new();
        for (k, v) in inline.iter() {
            promoted.insert(k, Item::Value(v.clone()));
        }
        parent.insert(key, Item::Table(promoted));
    }
    let entry_table = parent
        .get_mut(key)
        .and_then(|item| match item {
            Item::Table(t) => Some(t),
            _ => None,
        })
        .expect("entry was just inserted/promoted as a table");

    let mut changed_any = false;
    for (child_key, child_op) in fields {
        let changed = apply_scalar(entry_table, &[], child_key, child_op);
        if changed {
            changed_any = true;
        }
    }
    changed_any
}

fn apply_remove_table_entry(doc: &mut DocumentMut, table_path: SettingsPath, key: &str) -> bool {
    let Some(parent) = descend_existing_table(doc.as_table_mut(), table_path) else {
        return false;
    };
    parent.remove(key).is_some()
}

fn apply_append_array_of_tables(
    doc: &mut DocumentMut,
    path: SettingsPath,
    fields: &[(&'static str, EditOp)],
) -> bool {
    let (leaf, parents) = path
        .split_last()
        .expect("AppendArrayOfTables requires a non-empty path");
    let parent = descend_or_create_table(doc.as_table_mut(), parents);
    if !parent.contains_key(leaf) {
        parent.insert(leaf, Item::ArrayOfTables(ArrayOfTables::new()));
    } else if !matches!(parent.get(leaf), Some(Item::ArrayOfTables(_))) {
        // Existing key but not an array of tables — overwrite. Loses prior
        // decor but this is a misuse case.
        parent.insert(leaf, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let aot = match parent.get_mut(leaf) {
        Some(Item::ArrayOfTables(aot)) => aot,
        _ => unreachable!("just inserted/promoted as array of tables"),
    };
    let mut new_row = Table::new();
    for (child_key, child_op) in fields {
        apply_scalar(&mut new_row, &[], child_key, child_op);
    }
    aot.push(new_row);
    true
}

fn apply_remove_array_of_tables_by_match(
    doc: &mut DocumentMut,
    path: SettingsPath,
    predicate: &ArrayOfTablesMatch,
) -> bool {
    let (leaf, parents) = path
        .split_last()
        .expect("RemoveArrayOfTablesByMatch requires a non-empty path");
    let Some(parent) = descend_existing_table(doc.as_table_mut(), parents) else {
        return false;
    };
    let Some(Item::ArrayOfTables(aot)) = parent.get_mut(leaf) else {
        return false;
    };
    let idx_to_remove = aot.iter().position(|row| row_matches(row, predicate));
    match idx_to_remove {
        Some(i) => {
            aot.remove(i);
            true
        }
        None => false,
    }
}

fn apply_set_theme_color(doc: &mut DocumentMut, theme: &str, token: &str, rgb: [u8; 3]) -> bool {
    let colors = theme_colors_table(doc, theme);
    set_leaf(colors, token, rgb_item(rgb))
}

fn apply_set_theme_colors(
    doc: &mut DocumentMut,
    theme: &str,
    colors: &[(String, [u8; 3])],
) -> bool {
    let existed = doc
        .as_table()
        .get("tui")
        .and_then(Item::as_table)
        .and_then(|tui| tui.get("themes"))
        .and_then(Item::as_table)
        .is_some_and(|themes| themes.contains_key(theme));
    let table = theme_colors_table(doc, theme);
    let mut changed = !existed;
    for (token, rgb) in colors {
        if set_leaf(table, token, rgb_item(*rgb)) {
            changed = true;
        }
    }
    changed
}

fn apply_remove_theme_color(doc: &mut DocumentMut, theme: &str, token: &str) -> bool {
    let Some(colors) = existing_theme_colors_table(doc, theme) else {
        return false;
    };
    colors.remove(token).is_some()
}

fn existing_theme_colors_table<'a>(doc: &'a mut DocumentMut, theme: &str) -> Option<&'a mut Table> {
    let tui = doc.as_table_mut().get_mut("tui")?.as_table_mut()?;
    let themes = tui.get_mut("themes")?.as_table_mut()?;

    if matches!(themes.get(theme), Some(Item::Value(Value::InlineTable(_)))) {
        let promoted = match themes.get(theme) {
            Some(Item::Value(Value::InlineTable(inline))) => {
                let mut table = Table::new();
                for (k, v) in inline.iter() {
                    table.insert(k, Item::Value(v.clone()));
                }
                table
            }
            _ => return None,
        };
        themes.insert(theme, Item::Table(promoted));
    }

    let theme_table = themes.get_mut(theme)?.as_table_mut()?;
    if matches!(
        theme_table.get("colors"),
        Some(Item::Value(Value::InlineTable(_)))
    ) {
        let promoted = match theme_table.get("colors") {
            Some(Item::Value(Value::InlineTable(inline))) => {
                let mut table = Table::new();
                for (k, v) in inline.iter() {
                    table.insert(k, Item::Value(v.clone()));
                }
                table
            }
            _ => return None,
        };
        theme_table.insert("colors", Item::Table(promoted));
    }

    theme_table.get_mut("colors")?.as_table_mut()
}

fn theme_colors_table<'a>(doc: &'a mut DocumentMut, theme: &str) -> &'a mut Table {
    let themes = descend_or_create_table(doc.as_table_mut(), &["tui", "themes"]);
    if !themes.contains_key(theme) {
        themes.insert(theme, Item::Table(Table::new()));
    } else if let Some(Item::Value(Value::InlineTable(inline))) = themes.get(theme) {
        let mut promoted = Table::new();
        for (k, v) in inline.iter() {
            promoted.insert(k, Item::Value(v.clone()));
        }
        themes.insert(theme, Item::Table(promoted));
    }
    let theme_table = themes
        .get_mut(theme)
        .and_then(|item| match item {
            Item::Table(t) => Some(t),
            _ => None,
        })
        .expect("theme entry was just inserted/promoted as a table");
    descend_or_create_table(theme_table, &["colors"])
}

fn rgb_item(rgb: [u8; 3]) -> Item {
    let arr = rgb.into_iter().map(i64::from).collect::<Array>();
    Item::Value(Value::Array(arr))
}

fn row_matches(row: &Table, predicate: &ArrayOfTablesMatch) -> bool {
    if predicate.columns.len() != predicate.values.len() {
        return false;
    }
    for (col, expected) in predicate.columns.iter().zip(&predicate.values) {
        let actual = row
            .get(col)
            .and_then(|item| item.as_value())
            .and_then(Value::as_str);
        match actual {
            Some(s) if s == expected.as_str() => {}
            _ => return false,
        }
    }
    true
}

fn set_leaf(table: &mut Table, key: &str, new_item: Item) -> bool {
    if let Some(existing) = table.get(key)
        && items_equal(existing, &new_item)
    {
        return false;
    }
    table.insert(key, new_item);
    true
}

fn items_equal(a: &Item, b: &Item) -> bool {
    a.to_string().trim() == b.to_string().trim()
}

fn descend_existing_table<'a>(root: &'a mut Table, parents: &[&str]) -> Option<&'a mut Table> {
    let mut current = root;
    for seg in parents {
        let entry = current.get_mut(seg)?;
        match entry {
            Item::Table(t) => {
                current = t;
            }
            Item::Value(Value::InlineTable(inline)) => {
                let mut promoted = Table::new();
                for (k, v) in inline.iter() {
                    promoted.insert(k, Item::Value(v.clone()));
                }
                *entry = Item::Table(promoted);
                match entry {
                    Item::Table(t) => current = t,
                    _ => unreachable!(),
                }
            }
            _ => return None,
        }
    }
    Some(current)
}

fn descend_or_create_table<'a>(root: &'a mut Table, parents: &[&str]) -> &'a mut Table {
    let mut current = root;
    for seg in parents {
        let entry = current
            .entry(seg)
            .or_insert_with(|| Item::Table(Table::new()));
        match entry {
            Item::Table(t) => {
                current = t;
            }
            Item::Value(Value::InlineTable(inline)) => {
                let mut promoted = Table::new();
                for (k, v) in inline.iter() {
                    promoted.insert(k, Item::Value(v.clone()));
                }
                *entry = Item::Table(promoted);
                match entry {
                    Item::Table(t) => current = t,
                    _ => unreachable!(),
                }
            }
            _ => {
                *entry = Item::Table(Table::new());
                match entry {
                    Item::Table(t) => current = t,
                    _ => unreachable!(),
                }
            }
        }
    }
    current
}

fn write_atomic(target: &Path, bytes: &[u8], kind: &SettingsScopeKind) -> std::io::Result<()> {
    let _ = kind;
    write_settings_atomic(target, bytes)
}

/// Write `bytes` to a settings file at `target` using the hardened
/// path: create parent dirs `0o700`, write to a sibling tempfile,
/// `sync_all`, chmod `0o600`, and rename into place.
///
/// Exposed for callers that hold a `DocumentMut` they've edited in
/// memory (e.g. the `/mcp` config page's `mcp_settings_edit`
/// helper) and need the same secret-safe write semantics as
/// `apply_edits`. Settings files may contain inline provider keys
/// and MCP env / HTTP headers, so the chmod isn't optional.
pub fn write_settings_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        create_dir_all_private(parent)?;
    }
    let tmp = sibling_tempfile(target);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // Settings files are per-user config and may hold inline secrets (provider
    // API keys), so every scope is tightened to owner-only before the rename.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms)?;
    }
    fs::rename(&tmp, target)
}

/// Like `fs::create_dir_all` but creates any missing components `0o700` on unix
/// so the directory chain is not world-traversable.
fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn sibling_tempfile(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.toml".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!(".{}.{}.tmp", process::id(), nanos);
    let new_name = format!("{name}{suffix}");
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(new_name),
        _ => PathBuf::from(new_name),
    }
}

#[cfg(test)]
#[path = "settings_writer_tests.rs"]
mod tests;
