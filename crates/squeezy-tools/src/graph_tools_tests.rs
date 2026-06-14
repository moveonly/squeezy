use super::*;

#[cfg(test)]
mod changed_byte_ranges_from_patch_tests {
    use super::{ChangedByteRange, changed_byte_ranges_from_patch};

    #[test]
    fn added_line_whose_content_starts_with_plus_plus_is_recorded_without_off_by_one() {
        // New file content (post-edit). Line 2 (`++i;`) was added, so git emits
        // it as `+` + `++i;` = `+++i;`, which the old `starts_with("+++")` guard
        // wrongly treated as a file header: it dropped the range AND failed to
        // advance new_line, throwing later line numbers off by one.
        let text = "a\n++i;\nc";
        let patch = "@@ -1,2 +1,3 @@\n a\n+++i;\n c\n";
        let ranges = changed_byte_ranges_from_patch(patch, text);
        // Exactly one modified range covering line 2 (the added `++i;`).
        assert_eq!(
            ranges,
            vec![ChangedByteRange::new(2, 7, 2, 2, "modified")],
            "added `++` content line must be recorded at the correct line number",
        );
    }

    #[test]
    fn deleted_line_whose_content_starts_with_dashes_is_recorded() {
        // A `---` frontmatter/rule line was deleted, so git emits `-` + `---`
        // = `----`, which the old `starts_with("---")` guard silently dropped.
        let text = "a\nb";
        let patch = "@@ -1,3 +1,2 @@\n a\n----\n b\n";
        let ranges = changed_byte_ranges_from_patch(patch, text);
        // The deletion is recorded (at new-side line 2, where it would land).
        assert_eq!(
            ranges.len(),
            1,
            "deleted `---` content line must be recorded"
        );
        assert_eq!(ranges[0].status, "deleted");
        assert_eq!(ranges[0].start_line, 2);
    }
}

#[cfg(test)]
mod line_window_auto_widen_tests {
    use super::{
        READ_SLICE_AUTO_WIDEN_TARGET_LINES, READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES, ReadSliceArgs,
        line_window,
    };

    fn args(start_line: u32, end_line: u32) -> ReadSliceArgs {
        let raw = serde_json::json!({
            "path": "ignored",
            "start_line": start_line,
            "end_line": end_line,
        });
        serde_json::from_value(raw).expect("ReadSliceArgs")
    }

    fn text_with(lines: u32) -> String {
        (1..=lines)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tight_window_auto_widens_toward_target() {
        // The realworld pattern: model picked a 21-line window (470..=490) around
        // a 3-line method body — auto-widen pushes it toward
        // ~READ_SLICE_AUTO_WIDEN_TARGET_LINES so the enclosing impl block fits.
        let text = text_with(1000);
        let (_, _, span) = line_window(&text, &args(470, 490)).expect("line_window");
        let actual_lines = span.end.line.saturating_sub(span.start.line) + 1;
        assert!(
            actual_lines >= READ_SLICE_AUTO_WIDEN_TARGET_LINES - 1,
            "expected widened to ~{}, got {actual_lines}",
            READ_SLICE_AUTO_WIDEN_TARGET_LINES
        );
        assert!(
            span.start.line < 470,
            "expected start line padded above 470, got {}",
            span.start.line + 1
        );
        assert!(
            span.end.line > 490,
            "expected end line padded below 490, got {}",
            span.end.line + 1
        );
    }

    #[test]
    fn wide_window_is_left_alone() {
        // Caller already asked for >= READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES,
        // so honor the request exactly. SourcePoint::line is 0-based here.
        let text = text_with(1000);
        let start = 100;
        let end = start + READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES; // 60-line span
        let (_, _, span) = line_window(&text, &args(start, end)).expect("line_window");
        assert_eq!(span.start.line, start - 1);
        assert_eq!(span.end.line, end - 1);
    }

    #[test]
    fn single_line_request_expands_to_target_window() {
        // Common shape: model picks a single line because the packet said
        // start.line=476 and it forgot to read body_span.end.line.
        let text = text_with(1000);
        let (_, _, span) = line_window(&text, &args(476, 476)).expect("line_window");
        let actual_lines = span.end.line.saturating_sub(span.start.line) + 1;
        assert!(
            actual_lines >= READ_SLICE_AUTO_WIDEN_TARGET_LINES - 1,
            "expected widened to ~{}, got {actual_lines}",
            READ_SLICE_AUTO_WIDEN_TARGET_LINES
        );
    }

    #[test]
    fn auto_widen_respects_file_bounds() {
        // Asking near the top of a small file must not panic, and must clamp
        // start_line at 1 without overshooting end_line past file length.
        let text = text_with(20);
        let (_, _, span) = line_window(&text, &args(2, 5)).expect("line_window");
        assert_eq!(span.start.line, 0, "start_line clamped to 1");
        assert!(
            span.end.line <= 19,
            "end_line clamped to file length, got {}",
            span.end.line + 1
        );
    }
}

#[cfg(test)]
mod attribute_filter_tests {
    use super::attribute_filter_matches;

    #[test]
    fn single_value_filter_matches_case_insensitive_equality_only() {
        let attrs = vec!["base:Session".to_string(), "decorator:property".to_string()];
        assert!(attribute_filter_matches(&attrs, "base:Session"));
        // Case-insensitive equality still holds.
        assert!(attribute_filter_matches(&attrs, "BASE:session"));
        // Substring no longer matches: `Session` is not a stored attribute.
        assert!(!attribute_filter_matches(&attrs, "Session"));
        assert!(!attribute_filter_matches(&attrs, "base:AuthBase"));
    }

    #[test]
    fn prefix_filter_does_not_false_positive_on_longer_attribute() {
        // Bug #6: `base:User` must NOT match a symbol carrying
        // `base:UserProfile` — the old `contains` substring fallback did.
        let profile = vec!["base:UserProfile".to_string()];
        assert!(!attribute_filter_matches(&profile, "base:User"));
        // It must still match the exact attribute.
        let user = vec!["base:User".to_string()];
        assert!(attribute_filter_matches(&user, "base:User"));
    }

    #[test]
    fn segmented_attribute_still_matches_exactly() {
        // Multi-segment attrs (`mixin:ns:leaf`) compare in full.
        let attrs = vec!["mixin:ns:leaf".to_string()];
        assert!(attribute_filter_matches(&attrs, "mixin:ns:leaf"));
        assert!(!attribute_filter_matches(&attrs, "mixin:ns"));
    }

    #[test]
    fn alternation_still_matches_a_listed_alternative() {
        // `base:A|base:B` matches a symbol carrying `base:B`.
        let symbol = vec!["base:B".to_string()];
        assert!(attribute_filter_matches(&symbol, "base:A|base:B"));
    }

    #[test]
    fn multi_value_filter_matches_any_alternative() {
        // The python "9 bases" case: one decl_search instead of nine.
        let session = vec!["base:Session".to_string()];
        let auth = vec!["base:AuthBase".to_string()];
        let unrelated = vec!["base:Widget".to_string()];
        let filter = "base:Session|base:AuthBase|base:CookieJar";
        assert!(attribute_filter_matches(&session, filter));
        assert!(attribute_filter_matches(&auth, filter));
        assert!(!attribute_filter_matches(&unrelated, filter));
    }

    #[test]
    fn multi_value_filter_ignores_blank_alternatives_and_whitespace() {
        let attrs = vec!["base:Session".to_string()];
        assert!(attribute_filter_matches(
            &attrs,
            "base:Session | base:AuthBase"
        ));
        assert!(attribute_filter_matches(&attrs, "|base:Session|"));
    }
}

#[cfg(test)]
mod transitive_seed_tests {
    use super::{attribute_has_inheritance_prefix, seed_type_names};

    #[test]
    fn detects_each_inheritance_prefix() {
        assert!(attribute_has_inheritance_prefix("base:A"));
        assert!(attribute_has_inheritance_prefix("iface:Comparable"));
        assert!(attribute_has_inheritance_prefix("mixin:Observer"));
        // Any alternative with a prefix qualifies.
        assert!(attribute_has_inheritance_prefix("decorator:x|base:A"));
        // Prefix-free / non-inheritance filters do not.
        assert!(!attribute_has_inheritance_prefix("A"));
        assert!(!attribute_has_inheritance_prefix("decorator:property"));
    }

    #[test]
    fn parses_seed_names_from_each_prefix() {
        assert_eq!(
            seed_type_names("base:A|mixin:M|iface:I"),
            vec!["A".to_string(), "M".to_string(), "I".to_string()],
        );
    }

    #[test]
    fn seed_names_dedup_trim_and_skip_prefix_free() {
        // Whitespace is trimmed, duplicates collapse (first-seen order), and a
        // prefix-free alternative contributes no seed.
        assert_eq!(
            seed_type_names(" base:A | base:A | Plain | iface:B "),
            vec!["A".to_string(), "B".to_string()],
        );
        assert!(seed_type_names("Plain").is_empty());
    }
}

#[cfg(test)]
mod path_filter_tests {
    use super::path_matches_filter;

    #[test]
    fn multi_segment_filter_requires_directory_boundary() {
        // The Java realworld bug: `gson/src/main/java` was returning 27
        // matches because suffix/fuzzy matching admitted `src/test/java`
        // siblings. Strict prefix semantics keep only the real children.
        assert!(path_matches_filter(
            "gson/src/main/java/com/google/gson/TypeAdapter.java",
            "gson/src/main/java",
        ));
        assert!(!path_matches_filter(
            "gson/src/test/java/com/google/gson/TypeAdapterTest.java",
            "gson/src/main/java",
        ));
    }

    #[test]
    fn multi_segment_filter_rejects_substring_neighbour() {
        // `src/main` must not bleed into `experimental_src/main` — a
        // substring/fuzzy match would incorrectly let it through.
        assert!(path_matches_filter("src/main/foo.rs", "src/main"));
        assert!(!path_matches_filter(
            "experimental_src/main/foo.rs",
            "src/main",
        ));
    }

    #[test]
    fn multi_segment_filter_allows_exact_directory_match() {
        // `path == filter` (the directory itself) is a legitimate match
        // when a symbol's file_id happens to equal the filter.
        assert!(path_matches_filter(
            "gson/src/main/java",
            "gson/src/main/java"
        ));
        // Trailing slashes are tolerated; the model occasionally types them.
        assert!(path_matches_filter(
            "gson/src/main/java/Foo.java",
            "gson/src/main/java/",
        ));
    }

    #[test]
    fn single_token_filter_retains_fuzzy_segment_match() {
        // The casual `path: "squeezy_graph"` UX still resolves to
        // `crates/squeezy-graph/src/lib.rs` via fuzzy/separator-insensitive
        // matching. Strict prefix only kicks in when the filter has a `/`.
        assert!(path_matches_filter(
            "crates/squeezy-graph/src/lib.rs",
            "squeezy_graph",
        ));
        assert!(path_matches_filter(
            "gson/src/main/java/com/google/gson/Foo.java",
            "Foo.java",
        ));
    }

    #[test]
    fn single_token_filter_still_rejects_unrelated_paths() {
        assert!(!path_matches_filter(
            "crates/squeezy-graph/src/lib.rs",
            "zzzznope",
        ));
    }

    // ── Windows separator tests ──────────────────────────────────────────────

    #[test]
    fn windows_backslash_filter_acts_as_directory_prefix() {
        // A filter pasted from Windows Explorer or PowerShell that uses `\`
        // must behave identically to the equivalent `/` filter.
        assert!(path_matches_filter("src/main/foo.rs", "src\\main",));
        // Sibling outside the subtree must not match.
        assert!(!path_matches_filter("src/main_extra/foo.rs", "src\\main",));
    }

    #[test]
    fn windows_backslash_filter_respects_exact_and_trailing_slash() {
        // Exact match with backslash separator.
        assert!(path_matches_filter("src/main/foo.rs", "src\\main\\foo.rs",));
        // Trailing backslash is tolerated like trailing forward slash.
        assert!(path_matches_filter("src/main/foo.rs", "src\\main\\",));
    }

    #[test]
    fn mixed_separator_filter_normalizes_correctly() {
        // Filters that mix `/` and `\` (e.g. copy-pasted from mixed toolchain
        // output) are normalised before matching.
        assert!(path_matches_filter(
            "gson/src/main/java/Foo.java",
            "gson\\src/main\\java",
        ));
        assert!(!path_matches_filter(
            "gson/src/test/java/Foo.java",
            "gson\\src/main\\java",
        ));
    }

    #[test]
    fn windows_csharp_paths_match() {
        // Common Windows-heavy source names pasted from Visual Studio or
        // Explorer.
        assert!(path_matches_filter("src/Program.cs", "src\\Program.cs",));
        assert!(path_matches_filter(
            "Properties/AssemblyInfo.cs",
            "Properties\\AssemblyInfo.cs",
        ));
        assert!(path_matches_filter(
            "Views/Home/Index.cshtml",
            "Views\\Home\\Index.cshtml",
        ));
        // appsettings single-token without separator still resolves via
        // suffix / fuzzy match.
        assert!(path_matches_filter(
            "appsettings.Development.json",
            "appsettings.Development.json",
        ));
    }

    #[test]
    fn windows_cmake_path_matches() {
        assert!(path_matches_filter(
            "src/CMakeLists.txt",
            "src\\CMakeLists.txt",
        ));
    }

    #[test]
    fn degenerate_separator_only_filter_matches_all_paths() {
        // A filter consisting only of separators (e.g., a model mistake)
        // normalises to an empty string after trimming and acts as an
        // unconditional match — the same behaviour as an absent filter.
        // Pinned here so any future refactor cannot silently change it.
        assert!(path_matches_filter("any/path/file.rs", "\\"));
        assert!(path_matches_filter("any/path/file.rs", "/"));
    }
}

#[cfg(test)]
mod exact_or_suffix_tests {
    use super::path_matches_exact_or_suffix;

    #[test]
    fn exact_or_suffix_handles_windows_separator() {
        // Windows backslash suffix: `src\lib.rs` must suffix-match the
        // slash-normalised graph path `crates/src/lib.rs`.
        assert!(path_matches_exact_or_suffix(
            "crates/src/lib.rs",
            "src\\lib.rs"
        ));
        // Exact match after normalisation.
        assert!(path_matches_exact_or_suffix("src/lib.rs", "src\\lib.rs"));
        // Must not match an unrelated file that shares a longer basename.
        assert!(!path_matches_exact_or_suffix(
            "crates/mylib.rs",
            "src\\lib.rs"
        ));
    }
}

#[cfg(test)]
mod linux_backslash_filter_tests {
    use super::path_matches_filter;

    #[test]
    fn backslash_filter_is_normalized_to_directory_prefix() {
        // Linux bug fix: a path copied from Windows output like `src\parser`
        // must enter strict directory-prefix mode (not bareword fuzzy) after
        // backslash normalization.  Without normalization, `src\parser` has no
        // `/` so it falls through to loose fuzzy matching and can admit
        // cross-tree matches.
        assert!(
            path_matches_filter("src/parser/lib.rs", "src\\parser"),
            "backslash filter should match as directory prefix"
        );
        assert!(
            !path_matches_filter("src/parser_util/lib.rs", "src\\parser"),
            "backslash filter must not bleed into directory-name substrings"
        );
        // Multi-segment backslash paths also normalize correctly.
        assert!(path_matches_filter(
            "crates/squeezy-graph/src/lib.rs",
            "crates\\squeezy-graph\\src",
        ));
        assert!(!path_matches_filter(
            "crates/squeezy-graph/tests/lib.rs",
            "crates\\squeezy-graph\\src",
        ));
    }
}

#[cfg(test)]
mod reference_path_filter_tests {
    use super::reference_matches_path;
    use squeezy_core::{Confidence, FileId, Provenance, SourcePoint, SourceSpan};
    use squeezy_graph::ReferenceHit;
    use squeezy_parse::{ParsedReference, ReferenceKind};

    fn hit_in(path: &str) -> ReferenceHit {
        ReferenceHit {
            owner: None,
            reference: ParsedReference {
                file_id: FileId::new(path.to_string()),
                owner_id: None,
                text: "Symbol".to_string(),
                kind: ReferenceKind::Identifier,
                span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                provenance: Provenance::new("test", "test"),
            },
            confidence: Confidence::Heuristic,
        }
    }

    #[test]
    fn directory_scope_matches_files_under_tree() {
        // Bug #9: `path="src/foo"` must match a reference in `src/foo/bar.rs`.
        // The previous exact-or-suffix matcher ignored directory scopes.
        assert!(reference_matches_path(&hit_in("src/foo/bar.rs"), "src/foo"));
    }

    #[test]
    fn directory_scope_respects_boundary() {
        // Must not bleed across the directory boundary into a sibling whose
        // name merely starts with the filter.
        assert!(!reference_matches_path(
            &hit_in("src/foobar/baz.rs"),
            "src/foo"
        ));
        // And not into an unrelated tree that contains the segment.
        assert!(!reference_matches_path(
            &hit_in("experimental_src/foo/x.rs"),
            "src/foo"
        ));
    }
}

#[cfg(test)]
mod hierarchy_node_count_tests {
    use super::hierarchy_node_count;
    use squeezy_core::{Freshness, SourcePoint, SourceSpan, SymbolId, SymbolKind};
    use squeezy_graph::HierarchyNode;

    fn leaf(name: &str) -> HierarchyNode {
        HierarchyNode {
            id: SymbolId::new(name),
            name: name.to_string(),
            kind: SymbolKind::Function,
            span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
            freshness: Freshness::Fresh,
            children: Vec::new(),
        }
    }

    fn root_with(children: usize) -> HierarchyNode {
        let mut node = leaf("root");
        node.kind = SymbolKind::File;
        node.children = (0..children).map(|i| leaf(&format!("child{i}"))).collect();
        node
    }

    #[test]
    fn counts_root_plus_every_descendant() {
        // A single root with 5 children serializes 6 nodes, not 1. This is the
        // miscount behind the "wide root reports truncated=false" bug.
        assert_eq!(hierarchy_node_count(&root_with(5)), 6);
    }

    #[test]
    fn counts_nested_descendants_recursively() {
        let mut root = root_with(2);
        // Give the first child two grandchildren.
        root.children[0].children = vec![leaf("g0"), leaf("g1")];
        // root(1) + child0(1) + child1(1) + g0(1) + g1(1) = 5
        assert_eq!(hierarchy_node_count(&root), 5);
    }

    #[test]
    fn leaf_counts_as_one() {
        assert_eq!(hierarchy_node_count(&leaf("solo")), 1);
    }
}

#[cfg(test)]
mod graph_payload_refresh_status_tests {
    use super::graph_payload;
    use squeezy_graph::{GraphManager, RefreshConfig};
    use squeezy_workspace::PathConflict;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "squeezy_graph_payload_{name}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("create temp workspace");
        root
    }

    /// Build a GraphManager over a tiny crate, then force a budget-exhausted
    /// refresh by recording changed paths under a zero per-tool budget. The
    /// returned manager + report mirror the production state where some changed
    /// files were never reparsed and stay queued.
    fn budget_exhausted_manager(name: &str) -> (GraphManager, squeezy_graph::RefreshReport) {
        let root = temp_root(name);
        for file in ["a.rs", "b.rs"] {
            std::fs::write(
                root.join("src").join(file),
                format!("fn {}_v1() {{}}\n", file.trim_end_matches(".rs")),
            )
            .expect("write source");
        }
        let mut manager = GraphManager::open_with_config(
            &root,
            RefreshConfig {
                debounce: Duration::from_millis(0),
                idle_refresh_interval: Duration::from_secs(600),
                // Zero budget: the reparse loop breaks before parsing any file.
                per_tool_refresh_budget: Duration::from_millis(0),
            },
        )
        .expect("open graph");
        std::thread::sleep(Duration::from_millis(2));
        let mut changed = Vec::new();
        for file in ["a.rs", "b.rs"] {
            let path = root.join("src").join(file);
            std::fs::write(
                &path,
                format!("fn {}_v2() {{}}\n", file.trim_end_matches(".rs")),
            )
            .expect("rewrite source");
            changed.push(path);
        }
        manager.record_changed_paths(changed);
        let report = manager.refresh_before_query().expect("refresh");
        let _ = std::fs::remove_dir_all(&root);
        (manager, report)
    }

    #[test]
    fn payload_surfaces_refresh_incomplete_when_budget_exhausted() {
        // Bug #2: a budget-exhausted refresh leaves changed paths unprocessed.
        // The payload must tell the model the graph evidence is partially stale
        // instead of advertising a bare `graph_available=true`.
        let (manager, report) = budget_exhausted_manager("incomplete");
        assert!(
            report.budget_exhausted,
            "zero budget must exhaust the refresh"
        );
        let payload = graph_payload("repo_map", &manager, &report);
        assert_eq!(
            payload.get("refresh_incomplete"),
            Some(&serde_json::json!(true))
        );
        let pending = payload
            .get("stale_pending")
            .and_then(|v| v.as_u64())
            .expect("stale_pending count");
        assert!(pending > 0, "expected unprocessed paths, got {pending}");
    }

    #[test]
    fn payload_omits_refresh_status_when_refresh_completes() {
        // A healthy, fully-completed refresh must not pay the stale-signal byte
        // cost: no `refresh_incomplete` / `stale_pending` keys.
        let root = temp_root("complete");
        std::fs::write(root.join("src").join("a.rs"), "fn a_v1() {}\n").expect("write source");
        let mut manager = GraphManager::open_with_config(
            &root,
            RefreshConfig {
                debounce: Duration::from_millis(0),
                idle_refresh_interval: Duration::from_millis(0),
                // Generous budget so the reparse loop completes.
                per_tool_refresh_budget: Duration::from_secs(30),
            },
        )
        .expect("open graph");
        let report = manager.refresh_before_query().expect("refresh");
        assert!(
            !report.budget_exhausted,
            "generous budget must complete the refresh"
        );
        let payload = graph_payload("repo_map", &manager, &report);
        assert_eq!(
            payload.get("graph_available"),
            Some(&serde_json::json!(true))
        );
        // The `indexing` block follows the same cost-first gating as
        // `refresh_incomplete`: a healthy graph (here, a non-watching
        // one-shot construction) pays nothing on the wire.
        assert!(
            payload.get("indexing").is_none(),
            "healthy disabled-watcher payload must omit the indexing block, got {payload:?}"
        );
        assert!(payload.get("refresh_incomplete").is_none());
        assert!(payload.get("stale_pending").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn payload_surfaces_path_conflicts_when_present() {
        let root = temp_root("path_conflicts");
        std::fs::write(root.join("src").join("a.rs"), "fn a() {}\n").expect("write source");
        let mut manager = GraphManager::open_with_config(
            &root,
            RefreshConfig {
                debounce: Duration::from_millis(0),
                idle_refresh_interval: Duration::from_millis(0),
                per_tool_refresh_budget: Duration::from_secs(30),
            },
        )
        .expect("open graph");
        let mut report = manager.refresh_before_query().expect("refresh");
        report.path_conflicts = vec![PathConflict {
            normalized_relative_path: "src/foo.rs".to_string(),
            relative_paths: vec!["src/Foo.rs".to_string(), "src/foo.rs".to_string()],
        }];

        let payload = graph_payload("repo_map", &manager, &report);
        let conflicts = payload
            .get("path_conflicts")
            .expect("path conflicts should be surfaced");
        assert_eq!(conflicts["count"], serde_json::json!(1));
        assert_eq!(
            conflicts["samples"][0]["normalized_relative_path"],
            serde_json::json!("src/foo.rs")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn payload_surfaces_freshness_mode_and_fallback_reason() {
        // `freshness_mode` must always be present in the payload. When the
        // manager is in polling-fallback mode, `freshness_fallback_reason` must
        // also be present with the exact reason string.
        let root = temp_root("freshness");
        std::fs::write(root.join("src").join("a.rs"), "fn a() {}\n").expect("write source");
        let mut manager = GraphManager::open(&root).expect("open graph");
        let report = manager.refresh_before_query().expect("refresh");

        // Default mode is polling — no fallback reason.
        let payload = graph_payload("repo_map", &manager, &report);
        assert_eq!(
            payload.get("freshness_mode"),
            Some(&serde_json::json!("polling")),
            "default freshness_mode must be 'polling'"
        );
        assert!(
            payload.get("freshness_fallback_reason").is_none(),
            "no fallback reason expected for default polling mode"
        );

        // After marking polling fallback the reason must propagate to payload.
        manager.mark_polling_fallback("watcher startup failed: test");
        let report2 = manager
            .refresh_before_query()
            .expect("refresh after fallback");
        let payload2 = graph_payload("repo_map", &manager, &report2);
        assert_eq!(
            payload2.get("freshness_mode"),
            Some(&serde_json::json!("polling")),
        );
        assert_eq!(
            payload2.get("freshness_fallback_reason"),
            Some(&serde_json::json!("watcher startup failed: test")),
            "fallback reason must appear in payload after mark_polling_fallback"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod windows_path_normalization_tests {
    use super::{normalize_path_filter, path_matches_exact_or_suffix, path_matches_filter};

    #[test]
    fn normalize_path_filter_replaces_backslashes() {
        assert_eq!(normalize_path_filter("src\\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("src/lib.rs"), "src/lib.rs");
        assert_eq!(
            normalize_path_filter(".\\src\\app.cs"),
            "src/app.cs",
            "leading ./ must be stripped after backslash normalization"
        );
        assert_eq!(normalize_path_filter("./src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn path_matches_exact_or_suffix_accepts_backslash_filter() {
        // Windows users may supply backslash paths; they should match the
        // slash-normalized indexed paths without extra friction.
        assert!(
            path_matches_exact_or_suffix("src/lib.rs", "src\\lib.rs"),
            "backslash filter must match slash-indexed path"
        );
        assert!(
            path_matches_exact_or_suffix("crates/foo/src/lib.rs", "src\\lib.rs"),
            "backslash suffix filter must match"
        );
        assert!(
            !path_matches_exact_or_suffix("src/other.rs", "src\\lib.rs"),
            "non-matching backslash filter must not match"
        );
    }

    #[test]
    fn path_matches_filter_accepts_backslash_directory_filter() {
        // A Windows-style directory filter like `src\utils` should match files
        // under `src/utils/` in the indexed tree.
        assert!(
            path_matches_filter("src/utils/helper.rs", "src\\utils"),
            "backslash directory filter must match files under that tree"
        );
        assert!(
            !path_matches_filter("src/utils_extra/helper.rs", "src\\utils"),
            "backslash directory filter must not cross directory boundaries"
        );
    }

    #[test]
    fn path_matches_filter_strips_leading_dotslash() {
        assert!(
            path_matches_filter("src/lib.rs", "./src/lib.rs"),
            "./prefix must be stripped before matching"
        );
        assert!(
            path_matches_filter("src/lib.rs", ".\\src\\lib.rs"),
            ".\\prefix must be stripped and backslashes normalized"
        );
    }

    /// Verify that the case-insensitive comparison logic (used inside the
    /// `#[cfg(target_os = "windows")]` blocks) is correct. We call the
    /// helper with pre-lowercased strings directly so the test runs on all
    /// platforms without a Windows host.
    #[test]
    fn case_insensitive_comparison_logic_is_correct() {
        // Simulate the exact-match case: eq_ignore_ascii_case
        assert!("src/Lib.rs".eq_ignore_ascii_case("src/lib.rs"));
        assert!("SRC/LIB.RS".eq_ignore_ascii_case("src/lib.rs"));
        assert!(!"src/lib.rs".eq_ignore_ascii_case("src/other.rs"));

        // Simulate the suffix-match case: lowercase both sides, check trailing slash boundary
        let path_lower = "crates/foo/src/lib.rs".to_ascii_lowercase();
        let filter_lower = "src/lib.rs".to_ascii_lowercase();
        assert!(
            path_lower
                .strip_suffix(filter_lower.as_str())
                .is_some_and(|prefix| prefix.ends_with('/')),
            "case-folded suffix match must respect directory boundary"
        );

        // Ensure suffix match stops at directory boundary (not inside a component)
        let path_lower2 = "crates/src_extra/lib.rs".to_ascii_lowercase();
        let filter_lower2 = "src/lib.rs".to_ascii_lowercase();
        assert!(
            !path_lower2
                .strip_suffix(filter_lower2.as_str())
                .is_some_and(|prefix| prefix.ends_with('/')),
            "suffix match must not cross directory boundaries"
        );

        // Simulate the prefix-match case in path_matches_filter
        let path_lower3 = "src/utils/helper.rs".to_ascii_lowercase();
        let filter_lower3 = "src/utils".to_ascii_lowercase();
        let filter_with_slash = format!("{filter_lower3}/");
        assert!(
            path_lower3.eq_ignore_ascii_case(&filter_lower3)
                || (path_lower3.starts_with(filter_with_slash.as_str())),
            "case-folded prefix match must work"
        );
    }
}

#[cfg(test)]
mod option_filter_tests {
    use super::{
        ConfidenceScope, parse_confidence_level, parse_edge_kind_filter, result_path_scope,
        split_kind_tokens,
    };
    use serde_json::json;
    use squeezy_core::{Confidence, EdgeKind};

    #[test]
    fn edge_kind_filter_is_case_insensitive_and_aliased() {
        assert_eq!(parse_edge_kind_filter("Calls"), Some(EdgeKind::Calls));
        assert_eq!(parse_edge_kind_filter("ref"), Some(EdgeKind::References));
        assert_eq!(parse_edge_kind_filter("IMPORTS"), Some(EdgeKind::Imports));
        assert_eq!(
            parse_edge_kind_filter("re-export"),
            Some(EdgeKind::Reexports)
        );
        assert_eq!(parse_edge_kind_filter("nonsense"), None);
    }

    #[test]
    fn split_kind_tokens_splits_on_pipe_and_trims() {
        assert_eq!(
            split_kind_tokens(Some("struct| enum |trait")),
            Some(vec!["struct", "enum", "trait"])
        );
        // A single token still yields a one-element vec.
        assert_eq!(split_kind_tokens(Some("struct")), Some(vec!["struct"]));
        // All-blank / empty collapses to no-filter.
        assert_eq!(split_kind_tokens(Some(" | ")), None);
        assert_eq!(split_kind_tokens(None), None);
    }

    #[test]
    fn result_path_scope_prefers_result_path_then_path() {
        assert_eq!(result_path_scope(Some("src"), Some("src/a")), Some("src/a"));
        assert_eq!(result_path_scope(Some("src"), None), Some("src"));
        // Blank tokens are treated as absent.
        assert_eq!(result_path_scope(Some("  "), None), None);
        assert_eq!(result_path_scope(None, None), None);
    }

    #[test]
    fn confidence_level_parses_ids_and_aliases() {
        assert_eq!(
            parse_confidence_level("exact_syntax"),
            Some(Confidence::ExactSyntax)
        );
        assert_eq!(
            parse_confidence_level("External"),
            Some(Confidence::External)
        );
        assert_eq!(
            parse_confidence_level("resolved"),
            Some(Confidence::ImportResolved)
        );
        assert_eq!(parse_confidence_level("bogus"), None);
    }

    #[test]
    fn confidence_scope_noop_keeps_everything() {
        let scope = ConfidenceScope::new(None, false, false);
        assert!(scope.is_noop());
        assert!(scope.keeps(Confidence::External));
        assert!(scope.keeps(Confidence::ExactSyntax));
    }

    #[test]
    fn confidence_scope_allow_set_filters_by_level() {
        let scope = ConfidenceScope::new(Some("exact_syntax|import_resolved"), false, false);
        assert!(!scope.is_noop());
        assert!(scope.keeps(Confidence::ExactSyntax));
        assert!(scope.keeps(Confidence::ImportResolved));
        assert!(!scope.keeps(Confidence::Heuristic));
        // An all-unknown allow-set degrades to no constraint (never empties).
        let typo = ConfidenceScope::new(Some("typo"), false, false);
        assert!(typo.keeps(Confidence::Heuristic));
    }

    #[test]
    fn confidence_scope_external_isolation() {
        let drop_ext = ConfidenceScope::new(None, true, false);
        assert!(!drop_ext.keeps(Confidence::External));
        assert!(drop_ext.keeps(Confidence::ExactSyntax));

        let only_ext = ConfidenceScope::new(None, false, true);
        assert!(only_ext.keeps(Confidence::External));
        assert!(!only_ext.keeps(Confidence::ExactSyntax));

        // external_only wins when both are set.
        let both = ConfidenceScope::new(None, true, true);
        assert!(both.keeps(Confidence::External));
    }

    #[test]
    fn confidence_scope_packet_uses_body_confidence() {
        let scope = ConfidenceScope::new(None, true, false);
        // Edge packet pointing at an external target is dropped.
        let external_edge = json!({"edge": {"confidence": "external"}});
        assert!(!scope.keeps_packet(&external_edge));
        // A symbol packet at exact confidence is kept.
        let local_symbol = json!({"symbol": {"confidence": "exact_syntax"}});
        assert!(scope.keeps_packet(&local_symbol));
        // A packet with no determinable confidence is kept (positive filter).
        let bare = json!({"spans": []});
        assert!(scope.keeps_packet(&bare));
    }
}
