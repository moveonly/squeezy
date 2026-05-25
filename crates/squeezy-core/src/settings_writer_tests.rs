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

#[test]
fn set_table_entry_creates_and_populates_keyed_section() {
    let p = tmp_path("set_table_entry_new.toml");
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["mcp", "servers"],
            key: "filesystem".to_string(),
            fields: vec![
                ("command", EditOp::SetString("docs-mcp".to_string())),
                ("enabled", EditOp::SetBool(true)),
            ],
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert!(text.contains("[mcp.servers.filesystem]"));
    assert!(text.contains("command = \"docs-mcp\""));
    assert!(text.contains("enabled = true"));
    let _ = fs::remove_file(&p);
}

#[test]
fn set_table_entry_preserves_sibling_comments() {
    let p = tmp_path("set_table_entry_sibling.toml");
    fs::write(
        &p,
        "# top\n[mcp.servers.github]\n# kept\ncommand = \"gh-mcp\"\nenabled = true\n",
    )
    .unwrap();
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["mcp", "servers"],
            key: "filesystem".to_string(),
            fields: vec![("command", EditOp::SetString("fs-mcp".to_string()))],
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert!(text.contains("# top"));
    assert!(text.contains("# kept"));
    assert!(text.contains("[mcp.servers.github]"));
    assert!(text.contains("[mcp.servers.filesystem]"));
    assert!(text.contains("command = \"fs-mcp\""));
    let _ = fs::remove_file(&p);
}

#[test]
fn remove_table_entry_removes_section_only() {
    let p = tmp_path("remove_table_entry.toml");
    fs::write(
        &p,
        "[mcp.servers.a]\ncommand = \"x\"\n[mcp.servers.b]\ncommand = \"y\"\n",
    )
    .unwrap();
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::RemoveTableEntry {
            table_path: &["mcp", "servers"],
            key: "a".to_string(),
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert!(!text.contains("[mcp.servers.a]"));
    assert!(text.contains("[mcp.servers.b]"));
    assert!(text.contains("command = \"y\""));
    let _ = fs::remove_file(&p);
}

#[test]
fn append_array_of_tables_inserts_at_end() {
    let p = tmp_path("append_aot.toml");
    fs::write(
        &p,
        "[[permissions.rules]]\ncapability = \"read\"\ntarget = \"src/\"\naction = \"allow\"\n",
    )
    .unwrap();
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::AppendArrayOfTables {
            path: &["permissions", "rules"],
            fields: vec![
                ("capability", EditOp::SetString("shell".to_string())),
                ("target", EditOp::SetString("git*".to_string())),
                ("action", EditOp::SetString("ask".to_string())),
                ("source", EditOp::SetString("user".to_string())),
            ],
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert_eq!(text.matches("[[permissions.rules]]").count(), 2);
    assert!(text.contains("capability = \"read\""));
    assert!(text.contains("capability = \"shell\""));
    assert!(text.contains("target = \"git*\""));
    let _ = fs::remove_file(&p);
}

#[test]
fn append_array_of_tables_creates_when_absent() {
    let p = tmp_path("append_aot_new.toml");
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::AppendArrayOfTables {
            path: &["permissions", "rules"],
            fields: vec![
                ("capability", EditOp::SetString("read".to_string())),
                ("target", EditOp::SetString("src/".to_string())),
                ("action", EditOp::SetString("allow".to_string())),
            ],
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert!(text.contains("[[permissions.rules]]"));
    assert!(text.contains("target = \"src/\""));
    let _ = fs::remove_file(&p);
}

#[test]
fn remove_array_of_tables_by_match_deletes_exact_row() {
    let p = tmp_path("remove_aot.toml");
    fs::write(
        &p,
        "[[permissions.rules]]\ncapability = \"read\"\ntarget = \"src/\"\naction = \"allow\"\nsource = \"user\"\n[[permissions.rules]]\ncapability = \"shell\"\ntarget = \"git*\"\naction = \"ask\"\nsource = \"user\"\n",
    )
    .unwrap();
    let edits = vec![SettingsEdit {
        path: &[],
        op: EditOp::RemoveArrayOfTablesByMatch {
            path: &["permissions", "rules"],
            predicate: ArrayOfTablesMatch {
                columns: &["capability", "target", "source"],
                values: vec!["shell".to_string(), "git*".to_string(), "user".to_string()],
            },
        },
    }];
    apply_edits(&SettingsScope::user(&p), &edits).unwrap();
    let text = fs::read_to_string(&p).unwrap();
    assert_eq!(text.matches("[[permissions.rules]]").count(), 1);
    assert!(text.contains("capability = \"read\""));
    assert!(!text.contains("capability = \"shell\""));
    let _ = fs::remove_file(&p);
}

#[test]
fn remove_array_of_tables_by_match_is_noop_when_no_match() {
    let p = tmp_path("remove_aot_noop.toml");
    fs::write(
        &p,
        "[[permissions.rules]]\ncapability = \"read\"\ntarget = \"src/\"\naction = \"allow\"\n",
    )
    .unwrap();
    let outcome = apply_edits(
        &SettingsScope::user(&p),
        &[SettingsEdit {
            path: &[],
            op: EditOp::RemoveArrayOfTablesByMatch {
                path: &["permissions", "rules"],
                predicate: ArrayOfTablesMatch {
                    columns: &["capability"],
                    values: vec!["shell".to_string()],
                },
            },
        }],
    )
    .unwrap();
    assert_eq!(outcome.edits_applied, 0);
    assert_eq!(outcome.edits_skipped, 1);
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
