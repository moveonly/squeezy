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

/// Which file a save targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsScopeKind {
    /// `~/.squeezy/settings.toml`. Mode 0o600 after write.
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
        _ => {
            let (leaf, parents) = edit.path.split_last().expect("path empty check above");
            apply_scalar(doc.as_table_mut(), parents, leaf, &edit.op)
        }
    }
}

fn apply_scalar(root: &mut Table, parents: &[&str], leaf: &str, op: &EditOp) -> bool {
    let parent = descend_or_create_table(root, parents);
    match op {
        EditOp::SetString(s) => set_leaf(parent, leaf, value(s.as_str())),
        EditOp::SetInteger(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetFloat(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetBool(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetPath(p) => set_leaf(parent, leaf, value(p.display().to_string().as_str())),
        EditOp::SetArrayOfStrings(items) => {
            let mut arr = Array::new();
            for item in items {
                arr.push(item.as_str());
            }
            set_leaf(parent, leaf, Item::Value(Value::Array(arr)))
        }
        EditOp::Unset => {
            if parent.contains_key(leaf) {
                parent.remove(leaf);
                true
            } else {
                false
            }
        }
        EditOp::SetTableEntry { .. }
        | EditOp::RemoveTableEntry { .. }
        | EditOp::AppendArrayOfTables { .. }
        | EditOp::RemoveArrayOfTablesByMatch { .. } => unreachable!("dispatched in apply_one"),
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
    let parent = descend_or_create_table(doc.as_table_mut(), table_path);
    if parent.contains_key(key) {
        parent.remove(key);
        true
    } else {
        false
    }
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
    let parent = descend_or_create_table(doc.as_table_mut(), parents);
    let Some(Item::ArrayOfTables(aot)) = parent.get_mut(leaf) else {
        return false;
    };
    let mut idx_to_remove: Option<usize> = None;
    for (i, row) in aot.iter().enumerate() {
        if row_matches(row, predicate) {
            idx_to_remove = Some(i);
            break;
        }
    }
    match idx_to_remove {
        Some(i) => {
            aot.remove(i);
            true
        }
        None => false,
    }
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
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let tmp = sibling_tempfile(target);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    if matches!(kind, SettingsScopeKind::User) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms)?;
    }
    #[cfg(not(unix))]
    let _ = kind;
    fs::rename(&tmp, target)
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
