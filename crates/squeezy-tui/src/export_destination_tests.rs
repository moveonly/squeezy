//! Unit tests for the §12.6.4 export-destination grammar. These exercise the
//! pure parser in isolation (no `TuiApp`, no filesystem): the format token, the
//! destination keywords, the `dir:` form, path-traversal rejection, and the
//! verbatim file-path fallback. The end-to-end wiring (clipboard sink, atomic
//! file write, stdout transcript echo, resize) is covered by the integration
//! tests in `lib_tests.rs`.

use super::*;
use crate::copy::CopyFormat;

#[test]
fn empty_args_is_usage_error() {
    let err = parse_export_request("   ").expect_err("empty args must error");
    assert_eq!(err, EXPORT_USAGE);
}

#[test]
fn unknown_format_is_rejected_before_destination() {
    let err = parse_export_request("yaml clipboard").expect_err("unknown format errors");
    assert!(err.contains("unknown export format"), "{err}");
    assert!(err.contains("yaml"), "{err}");
}

#[test]
fn format_only_defaults_to_session_storage() {
    let req = parse_export_request("md").expect("format-only parses");
    assert_eq!(req.format, CopyFormat::Markdown);
    assert_eq!(req.destination, ExportDestination::Default);
}

#[test]
fn every_format_token_parses() {
    for (token, format) in [
        ("md", CopyFormat::Markdown),
        ("markdown", CopyFormat::Markdown),
        ("txt", CopyFormat::Plain),
        ("text", CopyFormat::Plain),
        ("plain", CopyFormat::Plain),
        ("json", CopyFormat::JsonSlice),
    ] {
        let req = parse_export_request(token).unwrap_or_else(|e| panic!("{token}: {e}"));
        assert_eq!(req.format, format, "token {token}");
        assert_eq!(req.destination, ExportDestination::Default);
    }
}

#[test]
fn clipboard_keyword_is_case_insensitive() {
    for token in ["clipboard", "Clipboard", "CLIP", "clip"] {
        let req = parse_export_request(&format!("txt {token}"))
            .unwrap_or_else(|e| panic!("{token}: {e}"));
        assert_eq!(req.destination, ExportDestination::Clipboard, "{token}");
        assert_eq!(req.format, CopyFormat::Plain);
    }
}

#[test]
fn stdout_keyword_and_dash_alias() {
    for token in ["stdout", "STDOUT", "-"] {
        let req = parse_export_request(&format!("json {token}"))
            .unwrap_or_else(|e| panic!("{token}: {e}"));
        assert_eq!(req.destination, ExportDestination::Stdout, "{token}");
        assert_eq!(req.format, CopyFormat::JsonSlice);
    }
}

#[test]
fn configured_dir_keeps_the_name() {
    let req = parse_export_request("md dir:notes").expect("dir parses");
    assert_eq!(
        req.destination,
        ExportDestination::ConfiguredDir("notes".to_string())
    );
}

#[test]
fn configured_dir_allows_nested_relative_segments() {
    let req = parse_export_request("md dir:exports/archive").expect("nested dir parses");
    assert_eq!(
        req.destination,
        ExportDestination::ConfiguredDir("exports/archive".to_string())
    );
}

#[test]
fn configured_dir_trims_inner_whitespace_around_name() {
    let req = parse_export_request("md dir:   notes  ").expect("dir parses");
    assert_eq!(
        req.destination,
        ExportDestination::ConfiguredDir("notes".to_string())
    );
}

#[test]
fn configured_dir_rejects_parent_escape() {
    let err = parse_export_request("md dir:../escape").expect_err("traversal must be rejected");
    assert!(err.contains(".."), "{err}");
}

#[test]
fn configured_dir_rejects_deep_parent_escape() {
    let err = parse_export_request("md dir:a/../../b").expect_err("nested traversal rejected");
    assert!(err.contains(".."), "{err}");
}

#[test]
fn configured_dir_rejects_absolute_path() {
    // A Unix-absolute name; on Windows the leading slash is a RootDir component
    // and is rejected by the same guard, so this case is portable.
    let err = parse_export_request("md dir:/etc").expect_err("absolute dir must be rejected");
    assert!(
        err.contains("workspace-relative") || err.contains("absolute"),
        "{err}"
    );
}

#[test]
fn configured_dir_rejects_empty_name() {
    let err = parse_export_request("md dir:").expect_err("empty dir name errors");
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn configured_dir_rejects_dot_only_name() {
    let err = parse_export_request("md dir:.").expect_err("dot-only dir errors");
    assert!(err.contains("no directory"), "{err}");
}

#[test]
fn plain_path_is_preserved_verbatim() {
    let req = parse_export_request("md ./out/notes.md").expect("path parses");
    assert_eq!(
        req.destination,
        ExportDestination::File("./out/notes.md".to_string())
    );
}

#[test]
fn path_with_interior_whitespace_is_preserved() {
    let req = parse_export_request("md ./my notes.md").expect("spaced path parses");
    assert_eq!(
        req.destination,
        ExportDestination::File("./my notes.md".to_string())
    );
}

#[test]
fn keyword_lookalike_filename_is_a_file_not_a_keyword() {
    // `clipboard.md` / `stdout.txt` are real files: only the *bare* keyword on
    // the whole tail selects the special destination.
    let req = parse_export_request("md clipboard.md").expect("file parses");
    assert_eq!(
        req.destination,
        ExportDestination::File("clipboard.md".to_string())
    );
    let req = parse_export_request("txt stdout.txt").expect("file parses");
    assert_eq!(
        req.destination,
        ExportDestination::File("stdout.txt".to_string())
    );
}

#[test]
fn destination_labels_are_distinct() {
    let labels = [
        ExportDestination::Default.label(),
        ExportDestination::File("x".to_string()).label(),
        ExportDestination::Clipboard.label(),
        ExportDestination::Stdout.label(),
        ExportDestination::ConfiguredDir("x".to_string()).label(),
    ];
    for (i, a) in labels.iter().enumerate() {
        for b in &labels[i + 1..] {
            assert_ne!(a, b, "labels must be distinct: {a:?} vs {b:?}");
        }
    }
}
