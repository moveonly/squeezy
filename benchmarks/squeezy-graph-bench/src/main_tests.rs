use std::{fs, path::Path};

use serde_json::json;

use super::*;

#[test]
fn select_scenarios_spreads_capped_samples() {
    let sample = select_scenarios(100, 10);

    assert_eq!(sample.len(), 10);
    assert!(sample.windows(2).all(|pair| pair[0] < pair[1]));
    assert_ne!(sample, (0..10).collect::<Vec<_>>());
}

#[test]
fn select_scenarios_zero_means_exhaustive() {
    assert_eq!(select_scenarios(5, 0), vec![0, 1, 2, 3, 4]);
}

#[test]
fn byte_to_lsp_position_counts_utf16_characters() {
    let source = "a\néx\n";

    assert_eq!(
        byte_to_lsp_position(source, 4),
        LspPosition {
            line: 1,
            character: 1
        }
    );
}

#[test]
fn parse_lsp_locations_relativizes_locations_and_location_links() {
    let value = json!([
        {
            "uri": "file:///repo/src/lib.rs",
            "range": {"start": {"line": 2, "character": 4}}
        },
        {
            "targetUri": "file:///repo/src/main.rs",
            "targetSelectionRange": {"start": {"line": 5, "character": 8}}
        }
    ]);

    let locations = parse_lsp_locations(&value, Path::new("/repo")).unwrap();

    assert_eq!(
        locations,
        vec![
            LocationKey {
                file: "src/lib.rs".to_string(),
                line: 2,
                character: 4
            },
            LocationKey {
                file: "src/main.rs".to_string(),
                line: 5,
                character: 8
            }
        ]
    );
}

#[test]
fn find_dotnet_build_target_prefers_root_solution_over_nested_slnx() {
    let root = temp_dir("dotnet-build-target-priority").unwrap();
    fs::create_dir_all(root.join("nested/very/deep")).unwrap();
    fs::write(root.join("App.sln"), "").unwrap();
    fs::write(root.join("nested/very/deep/Inner.slnx"), "").unwrap();
    fs::write(root.join("nested/very/deep/Inner.csproj"), "").unwrap();

    assert_eq!(
        find_dotnet_build_target(&root),
        Some(PathBuf::from("App.sln"))
    );
}

#[test]
fn find_dotnet_build_target_prefers_slnx_over_sln_at_same_depth() {
    let root = temp_dir("dotnet-build-target-extension-priority").unwrap();
    fs::write(root.join("App.sln"), "").unwrap();
    fs::write(root.join("App.slnx"), "").unwrap();
    fs::write(root.join("App.csproj"), "").unwrap();

    assert_eq!(
        find_dotnet_build_target(&root),
        Some(PathBuf::from("App.slnx"))
    );
}

#[test]
fn csharp_roslyn_oracle_emits_partial_record_declarations_once_per_file() {
    if !std::process::Command::new("dotnet")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping: dotnet SDK not installed");
        return;
    }
    let root = temp_dir("csharp-oracle-partial-record").unwrap();
    fs::write(
        root.join("Runner.cs"),
        "namespace Demo;\npublic partial class Runner { public string Run(string input) => input; }\n",
    )
    .unwrap();
    fs::write(
        root.join("Runner.Helpers.cs"),
        "namespace Demo;\npublic partial class Runner { public string Helper(string input) => input; }\n",
    )
    .unwrap();

    let scan = collect_csharp_oracle_symbol_scan(&root).unwrap();
    let module = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.cs".to_string(),
            kind: "Module".to_string(),
            name: "Demo".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let helpers_module = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.Helpers.cs".to_string(),
            kind: "Module".to_string(),
            name: "Demo".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let runner_in_first = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.cs".to_string(),
            kind: "Class".to_string(),
            name: "Runner".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let runner_in_second = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.Helpers.cs".to_string(),
            kind: "Class".to_string(),
            name: "Runner".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let run_method = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.cs".to_string(),
            kind: "Method".to_string(),
            name: "Run".to_string(),
        })
        .copied()
        .unwrap_or(0);

    assert_eq!(module, 1, "namespace recorded once per file");
    assert_eq!(helpers_module, 1);
    assert_eq!(runner_in_first, 1, "partial class counted once per file");
    assert_eq!(runner_in_second, 1);
    assert_eq!(run_method, 1);
    assert!(scan.unparseable_files.is_empty());
}

#[test]
fn python_ast_oracle_reports_unparseable_files_separately() {
    let root = temp_dir("python-ast-oracle-unparseable").unwrap();
    fs::write(root.join("valid.py"), "def ok():\n    pass\n").unwrap();
    fs::write(root.join("invalid.py"), "def broken(:\n    pass\n").unwrap();

    let scan = collect_python_ast_symbol_scan(&root).unwrap();

    assert_eq!(scan.unparseable_files, vec!["invalid.py".to_string()]);
    assert_eq!(scan.symbols.raw_total, 1);
    assert!(scan.symbols.counts.contains_key(&SymbolKey {
        file: "valid.py".to_string(),
        kind: "Function".to_string(),
        name: "ok".to_string()
    }));
}
