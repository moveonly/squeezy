use super::*;

// ---------------------------------------------------------------------------
// Threshold
// ---------------------------------------------------------------------------

#[test]
fn huge_paste_threshold_sits_above_the_very_large_preview_gate() {
    // The staging gate must be strictly above the §11G.6 preview gate, otherwise
    // the two would fight over the same paste.
    const _: () = {
        assert!(HUGE_PASTE_CHAR_THRESHOLD > crate::paste_preview::VERY_LARGE_PASTE_CHAR_THRESHOLD);
    };
}

#[test]
fn is_huge_paste_counts_characters_not_bytes() {
    let just_under = "x".repeat(HUGE_PASTE_CHAR_THRESHOLD);
    assert!(
        !is_huge_paste(&just_under),
        "exactly the threshold is not huge"
    );

    let just_over = "x".repeat(HUGE_PASTE_CHAR_THRESHOLD + 1);
    assert!(is_huge_paste(&just_over), "one over the threshold is huge");

    // Multi-byte chars count as characters, not their UTF-8 byte weight: a block
    // of N two-byte chars (2N bytes) is judged by N, so it stays under the gate.
    let multibyte = "é".repeat(HUGE_PASTE_CHAR_THRESHOLD);
    assert_eq!(multibyte.chars().count(), HUGE_PASTE_CHAR_THRESHOLD);
    assert!(
        multibyte.len() > HUGE_PASTE_CHAR_THRESHOLD,
        "bytes exceed chars"
    );
    assert!(!is_huge_paste(&multibyte), "judged by chars, so not huge");
}

// ---------------------------------------------------------------------------
// Estimates
// ---------------------------------------------------------------------------

#[test]
fn estimates_report_chars_bytes_lines_and_a_coarse_token_count() {
    let paste = StagedPaste::new("alpha\nbeta\ngamma".to_string());
    let est = paste.estimates();
    assert_eq!(est.chars, 16);
    assert_eq!(est.bytes, 16);
    assert_eq!(est.lines, 3);
    // ~4 chars/token → 16 / 4 = 4.
    assert_eq!(est.tokens, 4);
}

#[test]
fn estimates_count_multibyte_chars_and_bytes_separately() {
    // Two two-byte chars: 2 chars, 4 bytes.
    let paste = StagedPaste::new("éé".to_string());
    let est = paste.estimates();
    assert_eq!(est.chars, 2);
    assert_eq!(est.bytes, 4);
    assert_eq!(est.lines, 1);
}

#[test]
fn trailing_newline_does_not_add_a_phantom_line() {
    let paste = StagedPaste::new("one\n".to_string());
    assert_eq!(paste.estimates().lines, 1);
}

#[test]
fn summary_is_singular_plural_aware_and_groups_thousands() {
    let paste = StagedPaste::new("x".repeat(12_345));
    let summary = paste.summary();
    assert!(summary.contains("12,345 chars"), "{summary}");
    assert!(
        summary.contains("1 line"),
        "single line is singular: {summary}"
    );
    assert!(summary.contains("~3,086 tokens"), "{summary}");
    // The kind label leads the summary.
    assert!(summary.starts_with("text"), "{summary}");
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

#[test]
fn clean_text_has_no_warnings() {
    let paste = StagedPaste::new("plain harmless text\nsecond line".to_string());
    assert!(paste.warnings().is_empty());
    assert!(paste.warnings_summary().is_none());
}

#[test]
fn ansi_escapes_warn_about_terminal_controls() {
    let paste = StagedPaste::new("\x1b[31mred\x1b[0m".to_string());
    assert!(paste.has_ansi());
    assert!(paste.warnings().contains(&StagingWarning::TerminalControls));
    let summary = paste.warnings_summary().expect("warnings present");
    assert!(summary.contains("terminal control bytes"), "{summary}");
}

#[test]
fn nul_bytes_warn() {
    let paste = StagedPaste::new("before\0after".to_string());
    assert!(paste.warnings().contains(&StagingWarning::NulBytes));
}

#[test]
fn a_very_long_line_warns() {
    let long = "a".repeat(LONG_LINE_CHAR_THRESHOLD + 1);
    let paste = StagedPaste::new(format!("short\n{long}"));
    assert!(paste.warnings().contains(&StagingWarning::LongLines));
}

#[test]
fn many_short_lines_do_not_trip_the_long_line_warning() {
    let many = "a\n".repeat(LONG_LINE_CHAR_THRESHOLD);
    let paste = StagedPaste::new(many);
    assert!(!paste.warnings().contains(&StagingWarning::LongLines));
}

// ---------------------------------------------------------------------------
// Sanitized preview
// ---------------------------------------------------------------------------

#[test]
fn preview_strips_escapes_even_for_the_as_is_path() {
    // The preview is ALWAYS sanitized so a control-byte dump can never inject
    // sequences while staged, regardless of which action is selected.
    let paste = StagedPaste::new("\x1b[31mred line\x1b[0m\nplain".to_string());
    let lines = paste.preview_lines(80);
    assert!(
        lines.iter().all(|line| !line.contains('\x1b')),
        "no preview line may carry an escape byte: {lines:?}"
    );
    assert_eq!(lines[0], "red line", "escapes stripped, text preserved");
}

#[test]
fn preview_drops_nul_bytes() {
    let paste = StagedPaste::new("a\0b\0c".to_string());
    let lines = paste.preview_lines(80);
    assert_eq!(lines[0], "abc");
}

#[test]
fn preview_clips_long_lines_to_width_with_an_ellipsis() {
    let paste = StagedPaste::new("abcdefghij".to_string());
    let lines = paste.preview_lines(5);
    assert_eq!(lines[0].chars().count(), 5);
    assert!(lines[0].ends_with('…'));
}

#[test]
fn preview_bounds_the_head_window_and_summarizes_the_rest() {
    let body: String = (0..STAGING_PREVIEW_MAX_LINES + 5)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let paste = StagedPaste::new(body);
    let lines = paste.preview_lines(80);
    // Head window + one "+N more lines" marker.
    assert_eq!(lines.len(), STAGING_PREVIEW_MAX_LINES + 1);
    let last = lines.last().unwrap();
    assert!(last.contains("more lines"), "{last}");
    assert!(last.contains('5'), "five lines past the window: {last}");
}

#[test]
fn preview_of_a_zero_width_modal_never_panics() {
    let paste = StagedPaste::new("anything\nat all".to_string());
    let lines = paste.preview_lines(0);
    // A non-empty line collapses to the ellipsis marker; never a panic.
    assert!(lines.iter().all(|line| line.chars().count() <= 1));
}

// ---------------------------------------------------------------------------
// Action text production
// ---------------------------------------------------------------------------

#[test]
fn insert_and_queue_hand_back_the_text_verbatim() {
    let paste = StagedPaste::new("alpha\nbeta".to_string());
    assert_eq!(
        paste.prompt_text_for(StagingAction::Insert).as_deref(),
        Some("alpha\nbeta")
    );
    assert_eq!(
        paste.prompt_text_for(StagingAction::Queue).as_deref(),
        Some("alpha\nbeta")
    );
}

#[test]
fn quote_prefixes_every_line() {
    let paste = StagedPaste::new("alpha\nbeta".to_string());
    assert_eq!(
        paste.prompt_text_for(StagingAction::Quote).as_deref(),
        Some("> alpha\n> beta")
    );
}

#[test]
fn code_block_wraps_in_a_fence() {
    let paste = StagedPaste::new("let x = 1;".to_string());
    let fenced = paste.prompt_text_for(StagingAction::CodeBlock).unwrap();
    assert!(fenced.starts_with("```\n"), "{fenced}");
    assert!(fenced.ends_with("\n```"), "{fenced}");
    assert!(fenced.contains("let x = 1;"));
}

#[test]
fn strip_ansi_removes_escapes_from_the_attached_text() {
    let paste = StagedPaste::new("\x1b[31mred\x1b[0m text".to_string());
    assert_eq!(
        paste.prompt_text_for(StagingAction::StripAnsi).as_deref(),
        Some("red text")
    );
}

#[test]
fn side_actions_have_no_prompt_text() {
    let paste = StagedPaste::new("alpha\nbeta".to_string());
    assert!(paste.prompt_text_for(StagingAction::TempFile).is_none());
    assert!(paste.prompt_text_for(StagingAction::CopyPreview).is_none());
    assert!(paste.prompt_text_for(StagingAction::Cancel).is_none());
}

#[test]
fn copy_preview_text_carries_summary_warnings_and_sanitized_body() {
    let paste = StagedPaste::new("\x1b[31mred\x1b[0m line\nplain".to_string());
    let copied = paste.copy_preview_text();
    assert!(copied.contains("chars"), "summary present: {copied}");
    assert!(
        copied.contains("terminal control bytes"),
        "warnings present: {copied}"
    );
    assert!(
        !copied.contains('\x1b'),
        "copied preview body is sanitized: {copied:?}"
    );
    assert!(copied.contains("red line"));
}

#[test]
fn enters_prompt_groups_the_attach_actions() {
    for action in [
        StagingAction::Insert,
        StagingAction::Quote,
        StagingAction::CodeBlock,
        StagingAction::StripAnsi,
        StagingAction::Queue,
    ] {
        assert!(action.enters_prompt(), "{action:?} attaches to the prompt");
    }
    for action in [
        StagingAction::TempFile,
        StagingAction::CopyPreview,
        StagingAction::Cancel,
    ] {
        assert!(
            !action.enters_prompt(),
            "{action:?} is a side action, not a prompt attach"
        );
    }
}

// ---------------------------------------------------------------------------
// Overlay state / cursor
// ---------------------------------------------------------------------------

#[test]
fn clean_paste_offers_the_full_action_set_without_strip_ansi() {
    let staging = PasteStaging::new(StagedPaste::new("alpha\nbeta".to_string()));
    let actions = staging.actions();
    assert!(!actions.contains(&StagingAction::StripAnsi));
    // Insert / Quote / Code block / Temp file / Queue / Copy preview / Cancel.
    assert_eq!(actions.len(), 7);
    assert_eq!(staging.selected_action(), StagingAction::Insert);
    assert_eq!(staging.selected(), 0);
}

#[test]
fn ansi_paste_offers_strip_ansi_and_preselects_it() {
    let staging = PasteStaging::new(StagedPaste::new("\x1b[31mred\x1b[0m".to_string()));
    let actions = staging.actions();
    assert!(actions.contains(&StagingAction::StripAnsi));
    assert_eq!(
        staging.selected_action(),
        StagingAction::StripAnsi,
        "an escape-laden paste pre-selects Strip ANSI"
    );
}

#[test]
fn cursor_moves_and_wraps_in_both_directions() {
    let mut staging = PasteStaging::new(StagedPaste::new("alpha\nbeta".to_string()));
    let count = staging.actions().len();
    assert_eq!(staging.selected(), 0);

    // Up from the top wraps to the bottom.
    staging.move_up();
    assert_eq!(staging.selected(), count - 1);

    // Down from the bottom wraps to the top.
    staging.move_down();
    assert_eq!(staging.selected(), 0);

    // Down then up returns to the start.
    staging.move_down();
    assert_eq!(staging.selected(), 1);
    staging.move_up();
    assert_eq!(staging.selected(), 0);
}

#[test]
fn select_moves_to_a_valid_index_and_rejects_out_of_range() {
    let mut staging = PasteStaging::new(StagedPaste::new("alpha\nbeta".to_string()));
    let last = staging.actions().len() - 1;
    assert!(staging.select(last));
    assert_eq!(staging.selected(), last);
    assert!(
        !staging.select(staging.actions().len()),
        "out of range rejected"
    );
    assert_eq!(
        staging.selected(),
        last,
        "rejected select leaves the cursor"
    );
}

#[test]
fn into_paste_returns_the_captured_text() {
    let staging = PasteStaging::new(StagedPaste::new("verbatim\nbody".to_string()));
    assert_eq!(staging.into_paste().into_text(), "verbatim\nbody");
}
