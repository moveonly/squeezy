use std::fs;

use crate::{report::SymbolKey, util::temp_dir};

use super::collect_csharp_oracle_symbol_scan;

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
            name: "N:Demo".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let helpers_module = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.Helpers.cs".to_string(),
            kind: "Module".to_string(),
            name: "N:Demo".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let runner_in_first = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.cs".to_string(),
            kind: "Class".to_string(),
            name: "T:Demo.Runner".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let runner_in_second = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.Helpers.cs".to_string(),
            kind: "Class".to_string(),
            name: "T:Demo.Runner".to_string(),
        })
        .copied()
        .unwrap_or(0);
    let run_method = scan
        .symbols
        .counts
        .get(&SymbolKey {
            file: "Runner.cs".to_string(),
            kind: "Method".to_string(),
            name: "M:Demo.Runner.Run".to_string(),
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
