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

use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

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
        if edit.path.is_empty() {
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

fn load_document(path: &Path) -> Result<DocumentMut, WriterError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.parse::<DocumentMut>()?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err.into()),
    }
}

fn apply_one(doc: &mut DocumentMut, edit: &SettingsEdit) -> bool {
    let (leaf, parents) = edit.path.split_last().expect("path empty check above");
    let parent = descend_or_create_table(doc.as_table_mut(), parents);
    match &edit.op {
        EditOp::SetString(s) => set_leaf(parent, leaf, value(s.as_str())),
        EditOp::SetInteger(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetFloat(v) => set_leaf(parent, leaf, value(*v)),
        EditOp::SetBool(v) => set_leaf(parent, leaf, value(*v)),
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
    }
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
mod tests {
    use super::*;
    use std::env;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!(
            "squeezy-writer-{}-{}-{}",
            process::id(),
            nanos,
            name
        ));
        p
    }

    #[test]
    fn write_then_load_round_trips_simple_keys() {
        let p = tmp_path("simple.toml");
        let edits = vec![
            SettingsEdit {
                path: &["model", "provider"],
                op: EditOp::SetString("anthropic".to_string()),
            },
            SettingsEdit {
                path: &["budgets", "max_parallel_tools"],
                op: EditOp::SetInteger(16),
            },
            SettingsEdit {
                path: &["telemetry", "enabled"],
                op: EditOp::SetBool(false),
            },
        ];
        let scope = SettingsScope::user(&p);
        let outcome = apply_edits(&scope, &edits).unwrap();
        assert_eq!(outcome.edits_applied, 3);
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("[model]"));
        assert!(text.contains("provider = \"anthropic\""));
        assert!(text.contains("max_parallel_tools = 16"));
        assert!(text.contains("enabled = false"));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn round_trip_preserves_user_comments() {
        let p = tmp_path("comments.toml");
        fs::write(
            &p,
            "# Top-of-file note\n\n[model]\n# explanation\nprovider = \"openai\"\nmodel = \"gpt-5.5\"\n",
        )
        .unwrap();
        let edits = vec![SettingsEdit {
            path: &["model", "model"],
            op: EditOp::SetString("gpt-4".to_string()),
        }];
        apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# Top-of-file note"));
        assert!(text.contains("# explanation"));
        assert!(text.contains("provider = \"openai\""));
        assert!(text.contains("model = \"gpt-4\""));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn unset_removes_leaf_but_keeps_parent_and_comments() {
        let p = tmp_path("unset.toml");
        fs::write(
            &p,
            "[tui]\n# fr comment\nresponse_verbosity = \"verbose\"\nstatus_verbosity = \"compact\"\n",
        )
        .unwrap();
        let edits = vec![SettingsEdit {
            path: &["tui", "response_verbosity"],
            op: EditOp::Unset,
        }];
        apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("[tui]"));
        assert!(!text.contains("response_verbosity"));
        assert!(text.contains("status_verbosity = \"compact\""));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn no_op_edit_reports_skipped() {
        let p = tmp_path("noop.toml");
        fs::write(&p, "[model]\nprovider = \"openai\"\n").unwrap();
        let edits = vec![SettingsEdit {
            path: &["model", "provider"],
            op: EditOp::SetString("openai".to_string()),
        }];
        let outcome = apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        assert_eq!(outcome.edits_applied, 0);
        assert_eq!(outcome.edits_skipped, 1);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn nested_table_paths_are_created() {
        let p = tmp_path("nested.toml");
        let edits = vec![SettingsEdit {
            path: &["mcp", "servers", "docs", "command"],
            op: EditOp::SetString("docs-mcp".to_string()),
        }];
        apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("docs-mcp"));
        let parsed = text.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["mcp"]["servers"]["docs"]["command"]
                .as_str()
                .unwrap(),
            "docs-mcp"
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn array_of_strings_round_trips() {
        let p = tmp_path("array.toml");
        let edits = vec![SettingsEdit {
            path: &["redaction", "custom_patterns"],
            op: EditOp::SetArrayOfStrings(vec!["foo".to_string(), "bar".to_string()]),
        }];
        apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("custom_patterns"));
        assert!(text.contains("\"foo\""));
        assert!(text.contains("\"bar\""));
        let _ = fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn user_scope_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_path("perms.toml");
        let edits = vec![SettingsEdit {
            path: &["telemetry", "enabled"],
            op: EditOp::SetBool(true),
        }];
        apply_edits(&SettingsScope::user(&p), &edits).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "user file should be 0o600 (got {:o})", mode);
        let _ = fs::remove_file(&p);
    }
}
