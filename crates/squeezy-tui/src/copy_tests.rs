use super::*;
use crate::transcript_surface::{
    EntryId, FoldState, RowId, RowKind, TranscriptRow, plain_text_of_line,
};
use ratatui::text::Line;

/// Build a row owned by `entry` (or chrome when `entry` is `None`) carrying
/// `text` as its plain `copy_text`.
fn row(id: usize, entry: Option<u64>, kind: Option<RowKind>, text: &str) -> TranscriptRow {
    let line = Line::from(text.to_string());
    let copy_text = plain_text_of_line(&line);
    let char_len = copy_text.chars().count();
    TranscriptRow {
        row_id: RowId(id),
        entry_id: entry.map(EntryId),
        entry_kind: kind,
        visual_line_index: 0,
        line,
        copy_text,
        text_range: 0..char_len,
        style_spans: Vec::new(),
        fold_state: FoldState::Expanded,
        search_match_ranges: Vec::new(),
        click_targets: Vec::new(),
    }
}

/// `is_assistant` predicate over a fixed set of assistant entry ids.
fn assistant_set(ids: &'static [u64]) -> impl Fn(EntryId) -> bool {
    move |id: EntryId| ids.contains(&id.0)
}

fn never_assistant(_: EntryId) -> bool {
    false
}

// ---------------------------------------------------------------------------
// CopyFormat parsing
// ---------------------------------------------------------------------------

#[test]
fn format_from_token_accepts_aliases() {
    assert_eq!(CopyFormat::from_token("md"), Some(CopyFormat::Markdown));
    assert_eq!(
        CopyFormat::from_token("MARKDOWN"),
        Some(CopyFormat::Markdown)
    );
    assert_eq!(CopyFormat::from_token("txt"), Some(CopyFormat::Plain));
    assert_eq!(CopyFormat::from_token("text"), Some(CopyFormat::Plain));
    assert_eq!(CopyFormat::from_token("plain"), Some(CopyFormat::Plain));
    assert_eq!(CopyFormat::from_token("json"), Some(CopyFormat::JsonSlice));
    assert_eq!(
        CopyFormat::from_token("  json  "),
        Some(CopyFormat::JsonSlice)
    );
    assert_eq!(CopyFormat::from_token("yaml"), None);
}

#[test]
fn format_file_extension_matches_format() {
    assert_eq!(CopyFormat::Plain.file_extension(), "txt");
    assert_eq!(CopyFormat::Markdown.file_extension(), "md");
    assert_eq!(CopyFormat::JsonSlice.file_extension(), "json");
}

// ---------------------------------------------------------------------------
// Gutter stripping (plain format)
// ---------------------------------------------------------------------------

#[test]
fn plain_strips_rail_gutter() {
    // A continuation row carries a bare vertical bar gutter; a header carries
    // an elbow + marker. Both should be stripped to the content.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "│ hello"),
        row(1, Some(1), Some(RowKind::Message), "│ world"),
    ];
    let text = gather(
        &rows,
        Some(RowId(0)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "hello\nworld");
}

// ---------------------------------------------------------------------------
// FocusedEntry
// ---------------------------------------------------------------------------

#[test]
fn focused_entry_copies_whole_entry_run() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "first a"),
        row(1, Some(1), Some(RowKind::Message), "first b"),
        row(2, Some(2), Some(RowKind::Message), "second"),
    ];
    let text = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "first a\nfirst b");
}

#[test]
fn focused_entry_on_chrome_falls_back_to_preceding_entry() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "entry one"),
        row(1, None, None, "── divider ──"),
        row(2, Some(2), Some(RowKind::Message), "entry two"),
    ];
    // Focus on the chrome divider → nearest preceding entry-owned row (entry 1).
    let text = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "entry one");
}

#[test]
fn focused_entry_defaults_to_live_tail_when_no_focus() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "old"),
        row(1, Some(2), Some(RowKind::Message), "newest"),
        row(2, None, None, "pending tail chrome"),
    ];
    // No explicit focus → last entry-owned row (entry 2), skipping the chrome.
    let text = gather(
        &rows,
        None,
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "newest");
}

// ---------------------------------------------------------------------------
// LastAssistant
// ---------------------------------------------------------------------------

#[test]
fn last_assistant_picks_last_assistant_message() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "user q"),
        row(1, Some(2), Some(RowKind::Message), "assistant a1"),
        row(2, Some(3), Some(RowKind::Message), "user q2"),
        row(3, Some(4), Some(RowKind::Message), "assistant a2"),
    ];
    let is_assistant = assistant_set(&[2, 4]);
    let text = gather(
        &rows,
        None,
        CopyScope::LastAssistant,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "assistant a2");
}

#[test]
fn last_assistant_none_when_no_assistant() {
    let rows = vec![row(0, Some(1), Some(RowKind::Message), "only user")];
    assert!(
        gather(
            &rows,
            None,
            CopyScope::LastAssistant,
            CopyFormat::Plain,
            &never_assistant,
            None,
        )
        .is_none()
    );
}

// ---------------------------------------------------------------------------
// CurrentToolOutput
// ---------------------------------------------------------------------------

#[test]
fn tool_output_uses_focused_tool_entry() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "msg"),
        row(1, Some(2), Some(RowKind::ToolResult), "tool line 1"),
        row(2, Some(2), Some(RowKind::ToolResult), "tool line 2"),
    ];
    let text = gather(
        &rows,
        Some(RowId(2)),
        CopyScope::CurrentToolOutput,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "tool line 1\ntool line 2");
}

#[test]
fn tool_output_finds_nearest_above_focus() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::ToolResult), "tool out"),
        row(1, Some(2), Some(RowKind::Message), "later message"),
    ];
    // Focus on the message → nearest tool result above it.
    let text = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::CurrentToolOutput,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "tool out");
}

// ---------------------------------------------------------------------------
// CodeBlockUnderCursor
// ---------------------------------------------------------------------------

#[test]
fn code_block_returns_interior_only() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "intro"),
        row(1, Some(1), Some(RowKind::Message), "```rust"),
        row(2, Some(1), Some(RowKind::Message), "let x = 1;"),
        row(3, Some(1), Some(RowKind::Message), "let y = 2;"),
        row(4, Some(1), Some(RowKind::Message), "```"),
        row(5, Some(1), Some(RowKind::Message), "outro"),
    ];
    // Focus inside the fenced block.
    let text = gather(
        &rows,
        Some(RowId(2)),
        CopyScope::CodeBlockUnderCursor,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "let x = 1;\nlet y = 2;");
}

#[test]
fn code_block_detects_through_rail_gutter() {
    // Fence and code carry a rail gutter; detection must strip it first.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "│ ```py"),
        row(1, Some(1), Some(RowKind::Message), "│ print(1)"),
        row(2, Some(1), Some(RowKind::Message), "│ ```"),
    ];
    let text = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::CodeBlockUnderCursor,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "print(1)");
}

#[test]
fn code_block_none_when_cursor_outside_fence() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "```"),
        row(1, Some(1), Some(RowKind::Message), "code"),
        row(2, Some(1), Some(RowKind::Message), "```"),
        row(3, Some(1), Some(RowKind::Message), "after"),
    ];
    assert!(
        gather(
            &rows,
            Some(RowId(3)),
            CopyScope::CodeBlockUnderCursor,
            CopyFormat::Plain,
            &never_assistant,
            None,
        )
        .is_none()
    );
}

#[test]
fn code_block_none_for_dangling_unclosed_fence() {
    // An opening fence with no closing fence is incomplete: even with the cursor
    // parked inside the would-be block, resolution must find no fenced range
    // rather than running off the end of the rows.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "intro"),
        row(1, Some(1), Some(RowKind::Message), "```rust"),
        row(2, Some(1), Some(RowKind::Message), "let x = 1;"),
        row(3, Some(1), Some(RowKind::Message), "let y = 2;"),
    ];
    // Cursor on the open fence and on a body line both resolve to nothing.
    for focus in [RowId(1), RowId(2), RowId(3)] {
        assert!(
            gather(
                &rows,
                Some(focus),
                CopyScope::CodeBlockUnderCursor,
                CopyFormat::Plain,
                &never_assistant,
                None,
            )
            .is_none(),
            "dangling fence with focus {focus:?} must not resolve a block"
        );
    }
}

// ---------------------------------------------------------------------------
// Viewport / FullTranscript
// ---------------------------------------------------------------------------

#[test]
fn viewport_copies_supplied_range() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "a"),
        row(1, Some(2), Some(RowKind::Message), "b"),
        row(2, Some(3), Some(RowKind::Message), "c"),
        row(3, Some(4), Some(RowKind::Message), "d"),
    ];
    let text = gather(
        &rows,
        None,
        CopyScope::Viewport,
        CopyFormat::Plain,
        &never_assistant,
        Some((RowId(1), RowId(2))),
    )
    .unwrap();
    assert_eq!(text, "b\nc");
}

#[test]
fn full_transcript_copies_everything() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "a"),
        row(1, None, None, "chrome"),
        row(2, Some(2), Some(RowKind::Message), "b"),
    ];
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "a\nchrome\nb");
}

#[test]
fn empty_rows_resolve_to_none() {
    let rows: Vec<TranscriptRow> = Vec::new();
    assert!(
        gather(
            &rows,
            None,
            CopyScope::FullTranscript,
            CopyFormat::Plain,
            &never_assistant,
            None,
        )
        .is_none()
    );
}

// ---------------------------------------------------------------------------
// Markdown formatter
// ---------------------------------------------------------------------------

#[test]
fn markdown_prefixes_role_heading() {
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "question?"),
        row(1, Some(2), Some(RowKind::Message), "answer."),
    ];
    let is_assistant = assistant_set(&[2]);
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Markdown,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "**User**\nquestion?\n\n**Assistant**\nanswer.");
}

#[test]
fn markdown_does_not_treat_fence_as_entry_change() {
    // A code fence inside one message must not emit a spurious heading.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "see:"),
        row(1, Some(1), Some(RowKind::Message), "```"),
        row(2, Some(1), Some(RowKind::Message), "code"),
        row(3, Some(1), Some(RowKind::Message), "```"),
    ];
    let is_assistant = assistant_set(&[1]);
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Markdown,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "**Assistant**\nsee:\n```\ncode\n```");
}

// ---------------------------------------------------------------------------
// JSON slice formatter
// ---------------------------------------------------------------------------

#[test]
fn json_slice_groups_rows_by_entry_and_skips_chrome() {
    let rows = vec![
        row(0, Some(7), Some(RowKind::Message), "line one"),
        row(1, Some(7), Some(RowKind::Message), "line two"),
        row(2, None, None, "chrome divider"),
        row(3, Some(8), Some(RowKind::ToolResult), "ran ok"),
    ];
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::JsonSlice,
        &never_assistant,
        None,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], 7);
    assert_eq!(arr[0]["kind"], "message");
    assert_eq!(arr[0]["text"], "line one\nline two");
    assert_eq!(arr[1]["id"], 8);
    assert_eq!(arr[1]["kind"], "tool_result");
    assert_eq!(arr[1]["text"], "ran ok");
}

#[test]
fn row_kind_slug_is_stable() {
    assert_eq!(RowKind::Message.slug(), "message");
    assert_eq!(RowKind::ToolResult.slug(), "tool_result");
    assert_eq!(RowKind::Log.slug(), "log");
    assert_eq!(RowKind::PlanCard.slug(), "plan_card");
    assert_eq!(RowKind::Diff.slug(), "diff");
    assert_eq!(RowKind::Reasoning.slug(), "reasoning");
    assert_eq!(RowKind::SlashEcho.slug(), "slash_echo");
}

// ===========================================================================
// Phase 5a — copy-range correctness across wrapped lines, wide/CJK glyphs,
// rail/gutter stripping, box-drawing-free fenced code, and golden formatters.
//
// These exercise the SAME public surface (`gather` / `resolve_scope` /
// `format_rows`) the integration path uses, but at the row-model granularity
// `copy.rs` owns: one `TranscriptRow` == one already-wrapped visual line, so a
// long answer arrives here as several rows sharing an `entry_id`. The
// end-to-end test that the real wrapper actually splits wide/CJK content into
// these rows lives in `lib_tests.rs` (it needs `build_transcript_rows`); here
// we pin the join/strip/format semantics the wrapper feeds into.
// ===========================================================================

/// The box-drawing / rail glyphs a clean copy must never carry through: the
/// gutter bars, elbows, dashes, and the message-marker coins. Asserting their
/// absence is the "copied text is clean prose/code, no box-drawing" contract.
const BOX_DRAWING_GLYPHS: &[char] = &[
    '│', '├', '╰', '─', '☽', '☾', '◐', '◑', '◔', '◕', '●', '○', '▌',
];

fn assert_no_box_drawing(text: &str) {
    for ch in text.chars() {
        assert!(
            !BOX_DRAWING_GLYPHS.contains(&ch),
            "copied text must be clean prose/code, but carried box-drawing/rail glyph {ch:?} in:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Copy-range across wrapped lines
// ---------------------------------------------------------------------------

#[test]
fn wrapped_entry_rejoins_visual_rows_with_newlines() {
    // One logical answer the wrapper split into three visual rows: a leading
    // header row with the elbow+coin gutter, then two continuation rows under a
    // bare `│` gutter. A copy of the entry must rejoin them one-per-line with
    // the gutter stripped from every row — i.e. reconstruct the wrapped block.
    let rows = vec![
        row(
            0,
            Some(1),
            Some(RowKind::Message),
            "╰─☽ the quick brown fox",
        ),
        row(1, Some(1), Some(RowKind::Message), "│ jumps over the lazy"),
        row(
            2,
            Some(1),
            Some(RowKind::Message),
            "│ dog and keeps running",
        ),
    ];
    let text = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(
        text,
        "the quick brown fox\njumps over the lazy\ndog and keeps running"
    );
    assert_no_box_drawing(&text);
}

#[test]
fn wrapped_viewport_range_copies_only_the_visible_rows() {
    // A four-row wrapped block; the viewport shows only the middle two rows.
    // The copy must be exactly those rows, gutter-stripped, not the whole entry.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "│ line one"),
        row(1, Some(1), Some(RowKind::Message), "│ line two"),
        row(2, Some(1), Some(RowKind::Message), "│ line three"),
        row(3, Some(1), Some(RowKind::Message), "│ line four"),
    ];
    let text = gather(
        &rows,
        None,
        CopyScope::Viewport,
        CopyFormat::Plain,
        &never_assistant,
        Some((RowId(1), RowId(2))),
    )
    .unwrap();
    assert_eq!(text, "line two\nline three");
    assert_no_box_drawing(&text);
}

// ---------------------------------------------------------------------------
// Wide and CJK glyphs
// ---------------------------------------------------------------------------

#[test]
fn cjk_content_is_preserved_verbatim_across_rows() {
    // Wide CJK runs survive the gutter strip and row-join byte-for-byte: copy
    // must not transcode, truncate at a wide-cell boundary, or drop a char.
    let rows = vec![
        row(
            0,
            Some(1),
            Some(RowKind::Message),
            "╰─☽ 你好世界，这是一行中文",
        ),
        row(1, Some(1), Some(RowKind::Message), "│ 第二行也是中文内容"),
    ];
    let text = gather(
        &rows,
        Some(RowId(0)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "你好世界，这是一行中文\n第二行也是中文内容");
    assert_no_box_drawing(&text);
}

#[test]
fn wide_emoji_and_combining_glyphs_survive_gutter_strip() {
    // A mix of wide emoji (🦀, two cells), a ZWJ family emoji (one grapheme,
    // several scalars), and a combining accent must all round-trip unchanged.
    // `café` is written with an explicit combining acute (e + U+0301) so the
    // base glyph and its mark must stay together through the strip.
    let content = "ship it 🦀 — cafe\u{0301} 👨\u{200d}👩\u{200d}👧 done";
    let rows = vec![row(
        0,
        Some(1),
        Some(RowKind::Message),
        &format!("╰─☽ {content}"),
    )];
    let text = gather(
        &rows,
        Some(RowId(0)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, content);
    // The em-dash inside content is NOT a rail dash and must be preserved; only
    // gutter chrome is stripped.
    assert!(text.contains('—'), "em-dash content must survive: {text}");

    // No wide-cell boundary truncation mid-grapheme: the copied scalar sequence
    // is byte-identical to the source, so every multi-cell glyph (🦀, the family
    // emoji) and every multi-scalar grapheme (the ZWJ sequence, e + combining
    // acute) is intact — never sliced at a wide-cell boundary.
    assert_eq!(text.as_bytes(), content.as_bytes());
    // The ZWJ joiners survive in place (a truncation at a wide cell would drop a
    // member emoji or a joiner and leave a dangling U+200D), and the combining
    // mark stays bound to its base char.
    assert_eq!(
        text.matches('\u{200d}').count(),
        2,
        "both ZWJ joiners must survive intact: {text:?}"
    );
    assert!(
        text.contains("e\u{0301}"),
        "the combining acute must stay attached to its base: {text:?}"
    );
    // The terminal cell width is preserved end-to-end (a mid-glyph cut would
    // change the rendered width), confirming no wide cell was halved.
    let cell_width = |s: &str| {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum::<usize>()
    };
    assert_eq!(cell_width(&text), cell_width(content));
}

#[test]
fn cjk_in_a_fenced_code_block_copies_clean_interior() {
    // Code-block resolution + clean copy with CJK code/comments. The fence and
    // its rail gutter must be detected and excluded; the interior (with wide
    // glyphs) copied verbatim and box-drawing-free.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "│ ```py"),
        row(1, Some(1), Some(RowKind::Message), "│ 名前 = \"世界\""),
        row(2, Some(1), Some(RowKind::Message), "│ print(名前)"),
        row(3, Some(1), Some(RowKind::Message), "│ ```"),
    ];
    let text = gather(
        &rows,
        Some(RowId(2)),
        CopyScope::CodeBlockUnderCursor,
        CopyFormat::Plain,
        &never_assistant,
        None,
    )
    .unwrap();
    assert_eq!(text, "名前 = \"世界\"\nprint(名前)");
    assert_no_box_drawing(&text);
}

// ---------------------------------------------------------------------------
// Box-drawing-free guarantee for every format
// ---------------------------------------------------------------------------

#[test]
fn every_format_strips_rail_chrome_from_a_railed_transcript() {
    // A transcript where every row carries rail chrome: a user message, then an
    // assistant message wrapping a fenced block, all under `│`/`╰─☽` gutters.
    // Each of the three formats must emit clean text with no box-drawing.
    let rows = railed_transcript();
    let is_assistant = assistant_set(&[2]);
    for format in [
        CopyFormat::Plain,
        CopyFormat::Markdown,
        CopyFormat::JsonSlice,
    ] {
        let text = gather(
            &rows,
            None,
            CopyScope::FullTranscript,
            format,
            &is_assistant,
            None,
        )
        .unwrap();
        assert_no_box_drawing(&text);
    }
}

/// A small fixed transcript whose every row carries rail/gutter chrome, used by
/// the box-drawing-free guard and the golden formatter assertions. Entry 1 is a
/// user question; entry 2 is an assistant answer that embeds a fenced code
/// block. The chrome here (`╰─☽`, `│`) is exactly what `wrap_entries` paints, so
/// stripping it proves the copy substrate undoes the renderer's decoration.
fn railed_transcript() -> Vec<TranscriptRow> {
    vec![
        row(0, Some(1), Some(RowKind::Message), "╰─☽ how do I print?"),
        row(1, Some(2), Some(RowKind::Message), "╰─☽ like this:"),
        row(2, Some(2), Some(RowKind::Message), "│ ```py"),
        row(3, Some(2), Some(RowKind::Message), "│ print(\"hi\")"),
        row(4, Some(2), Some(RowKind::Message), "│ ```"),
    ]
}

// ---------------------------------------------------------------------------
// Golden formatter outputs on a small fixed transcript
// ---------------------------------------------------------------------------

#[test]
fn golden_plain_format_of_fixed_transcript() {
    let rows = railed_transcript();
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Plain,
        &assistant_set(&[2]),
        None,
    )
    .unwrap();
    // Plain: gutter-stripped content, one row per line, no headings, no fences
    // re-emitted (the fence rows are content here and pass through verbatim).
    assert_eq!(
        text,
        "how do I print?\nlike this:\n```py\nprint(\"hi\")\n```"
    );
}

#[test]
fn golden_markdown_format_of_fixed_transcript() {
    let rows = railed_transcript();
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Markdown,
        &assistant_set(&[2]),
        None,
    )
    .unwrap();
    // Markdown: each message entry gets a role heading; the fence inside the
    // assistant entry must NOT spawn a spurious heading mid-entry.
    assert_eq!(
        text,
        "**User**\nhow do I print?\n\n**Assistant**\nlike this:\n```py\nprint(\"hi\")\n```"
    );
}

#[test]
fn golden_json_slice_format_of_fixed_transcript() {
    let rows = railed_transcript();
    let text = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::JsonSlice,
        &assistant_set(&[2]),
        None,
    )
    .unwrap();
    // JSON event slice: one object per entry, rows grouped by entry_id, text
    // gutter-stripped and newline-joined. Assert the exact decoded shape rather
    // than a brittle whitespace-sensitive string.
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], 1);
    assert_eq!(arr[0]["kind"], "message");
    assert_eq!(arr[0]["text"], "how do I print?");
    assert_eq!(arr[1]["id"], 2);
    assert_eq!(arr[1]["kind"], "message");
    assert_eq!(arr[1]["text"], "like this:\n```py\nprint(\"hi\")\n```");
    // The raw JSON string carries no box-drawing either.
    assert_no_box_drawing(&text);
}

// ---------------------------------------------------------------------------
// Semantic-unit selection (each command picks the right unit) over the fixed
// railed transcript — proves scope resolution and the clean copy compose.
// ---------------------------------------------------------------------------

#[test]
fn semantic_units_select_distinct_clean_slices_of_one_transcript() {
    // Build a transcript with a user msg, a tool result, and an assistant msg
    // embedding a fenced block, each with rail chrome. Then drive every
    // semantic scope and assert it copied exactly its unit, gutter-free.
    let rows = vec![
        row(0, Some(1), Some(RowKind::Message), "╰─☽ run the tests"),
        row(
            1,
            Some(2),
            Some(RowKind::ToolResult),
            "│ test result: 5 passed",
        ),
        row(2, Some(3), Some(RowKind::Message), "╰─☽ all green:"),
        row(3, Some(3), Some(RowKind::Message), "│ ```sh"),
        row(4, Some(3), Some(RowKind::Message), "│ cargo test"),
        row(5, Some(3), Some(RowKind::Message), "│ ```"),
    ];
    let is_assistant = assistant_set(&[3]);

    // Last assistant answer: the whole entry-3 block, fences included.
    let last = gather(
        &rows,
        None,
        CopyScope::LastAssistant,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(last, "all green:\n```sh\ncargo test\n```");
    assert_no_box_drawing(&last);

    // Current tool output: focus on the tool row, get just that entry.
    let tool = gather(
        &rows,
        Some(RowId(1)),
        CopyScope::CurrentToolOutput,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(tool, "test result: 5 passed");
    assert_no_box_drawing(&tool);

    // Code block under cursor: focus inside the fence, get the interior only.
    let code = gather(
        &rows,
        Some(RowId(4)),
        CopyScope::CodeBlockUnderCursor,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(code, "cargo test");
    assert_no_box_drawing(&code);

    // Focused entry: focus on the user row, get just the user message.
    let entry = gather(
        &rows,
        Some(RowId(0)),
        CopyScope::FocusedEntry,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(entry, "run the tests");
    assert_no_box_drawing(&entry);

    // Full transcript: everything, gutter-free.
    let full = gather(
        &rows,
        None,
        CopyScope::FullTranscript,
        CopyFormat::Plain,
        &is_assistant,
        None,
    )
    .unwrap();
    assert_eq!(
        full,
        "run the tests\ntest result: 5 passed\nall green:\n```sh\ncargo test\n```"
    );
    assert_no_box_drawing(&full);
}
