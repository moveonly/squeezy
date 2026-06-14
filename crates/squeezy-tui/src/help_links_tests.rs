//! Unit tests for the actionable-help command linkifier (ITEM 3).

use super::*;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

// ---------------------------------------------------------------------------
// URI encode / decode round-trip
// ---------------------------------------------------------------------------

#[test]
fn command_uri_prepends_the_internal_scheme() {
    assert_eq!(command_uri("/theme"), "squeezy:cmd:/theme");
    assert_eq!(command_uri("/help"), "squeezy:cmd:/help");
}

#[test]
fn parse_command_uri_maps_back_to_the_prefill_action() {
    let action = parse_command_uri("squeezy:cmd:/theme").expect("known command should decode");
    assert_eq!(
        action,
        CommandLinkAction {
            command: "/theme".to_string(),
        }
    );
}

#[test]
fn command_uri_round_trips_for_every_registered_command() {
    for command in crate::input::SLASH_COMMANDS {
        let uri = command_uri(command.name);
        let action = parse_command_uri(&uri)
            .unwrap_or_else(|| panic!("round-trip failed for {}", command.name));
        assert_eq!(action.command, command.name);
    }
}

#[test]
fn parse_command_uri_rejects_unknown_commands() {
    // Right scheme, but not a registered command — must not prefill a bogus
    // command.
    assert_eq!(
        parse_command_uri("squeezy:cmd:/definitely-not-a-command"),
        None
    );
    assert_eq!(parse_command_uri("squeezy:cmd:/"), None);
    assert_eq!(parse_command_uri("squeezy:cmd:"), None);
}

#[test]
fn parse_command_uri_ignores_foreign_schemes() {
    // The decoder must leave the existing URL/file routing untouched: a real
    // web/file URI is not one of ours.
    assert_eq!(parse_command_uri("https://example.com/theme"), None);
    assert_eq!(parse_command_uri("file:///etc/hosts"), None);
    assert_eq!(parse_command_uri("/theme"), None);
    assert_eq!(parse_command_uri("squeezy:other:/theme"), None);
}

// ---------------------------------------------------------------------------
// Token detection
// ---------------------------------------------------------------------------

#[test]
fn is_command_token_matches_exact_registered_commands() {
    assert!(is_command_token("/theme"));
    assert!(is_command_token("/router"));
    assert!(is_command_token("/help"));
    // Surrounding whitespace inside the span is trimmed.
    assert!(is_command_token(" /theme "));
}

#[test]
fn is_command_token_rejects_non_commands() {
    assert!(!is_command_token("/theme dark")); // has an argument
    assert!(!is_command_token("theme")); // no leading slash
    assert!(!is_command_token("/notacommand")); // not registered
    assert!(!is_command_token("the /theme command")); // embedded in prose
    assert!(!is_command_token("")); // empty
    assert!(!is_command_token("/")); // bare slash
}

// ---------------------------------------------------------------------------
// Span detection over rendered lines
// ---------------------------------------------------------------------------

fn code_style() -> Style {
    Style::default().fg(Color::Cyan)
}

#[test]
fn detect_finds_a_command_code_span_and_leaves_prose_untouched() {
    // Mirrors the markdown renderer's output: a code span `/theme` surrounded by
    // plain prose spans.
    let line = Line::from(vec![
        Span::raw("Try "),
        Span::styled("/theme", code_style()),
        Span::raw(" to switch palettes."),
    ]);
    let lines = vec![line];

    let links = detect_command_links(&lines);
    assert_eq!(links.len(), 1);
    let link = &links[0];
    assert_eq!(link.line, 0);
    assert_eq!(link.span, 1); // the code span, not the prose
    assert_eq!(link.command, "/theme");
    assert_eq!(link.uri, "squeezy:cmd:/theme");
}

#[test]
fn detect_skips_non_command_code_spans() {
    // A code span that is NOT a slash command (a path, an identifier) must be
    // left alone so only real commands light up.
    let lines = vec![Line::from(vec![
        Span::raw("Edit "),
        Span::styled("src/lib.rs", code_style()),
        Span::raw(" and call "),
        Span::styled("render()", code_style()),
        Span::raw("."),
    ])];

    assert!(detect_command_links(&lines).is_empty());
}

#[test]
fn detect_finds_multiple_commands_across_lines_in_order() {
    let lines = vec![
        Line::from(vec![
            Span::raw("Open "),
            Span::styled("/router", code_style()),
            Span::raw(" then "),
            Span::styled("/model", code_style()),
        ]),
        Line::from(vec![
            Span::raw("See "),
            Span::styled("/help providers", code_style()), // has an arg → not a token
            Span::raw(" or "),
            Span::styled("/config", code_style()),
        ]),
    ];

    let links = detect_command_links(&lines);
    let commands: Vec<&str> = links.iter().map(|l| l.command.as_str()).collect();
    // `/help providers` is excluded (it carries an argument); the three bare
    // command tokens are detected in reading order.
    assert_eq!(commands, vec!["/router", "/model", "/config"]);
    assert_eq!((links[0].line, links[0].span), (0, 1));
    assert_eq!((links[1].line, links[1].span), (0, 3));
    assert_eq!((links[2].line, links[2].span), (1, 3));
}

// ---------------------------------------------------------------------------
// OSC 8 span rewrap
// ---------------------------------------------------------------------------

#[test]
fn command_hyperlink_span_wraps_visible_text_in_osc8_with_internal_uri() {
    let span = command_hyperlink_span("/theme", code_style());
    let content = span.content.as_ref();

    // Visible text is preserved verbatim inside the escapes.
    assert!(content.contains("/theme"));
    // The OSC 8 open carries our internal URI, and the close terminates it.
    assert!(content.contains("squeezy:cmd:/theme"));
    assert!(content.starts_with(&crate::hyperlinks::open_sequence("squeezy:cmd:/theme")));
    assert!(content.ends_with(crate::hyperlinks::CLOSE_SEQUENCE));
    // Style is carried through unchanged.
    assert_eq!(span.style, code_style());
}

#[test]
fn linkify_rewraps_only_command_spans_in_place() {
    let mut lines = vec![Line::from(vec![
        Span::raw("Try "),
        Span::styled("/theme", code_style()),
        Span::raw(" or edit "),
        Span::styled("src/lib.rs", code_style()),
    ])];

    let prose_before = lines[0].spans[0].content.clone();
    let path_before = lines[0].spans[3].content.clone();

    let links = linkify_command_spans(&mut lines);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].command, "/theme");

    // The command span now carries the OSC 8 escapes + internal URI.
    let cmd_span = lines[0].spans[1].content.as_ref();
    assert!(cmd_span.contains("squeezy:cmd:/theme"));
    assert!(cmd_span.contains("/theme"));

    // Prose and the non-command path span are byte-for-byte unchanged.
    assert_eq!(lines[0].spans[0].content, prose_before);
    assert_eq!(lines[0].spans[3].content, path_before);
}

#[test]
fn linkified_command_span_decodes_back_through_parse_command_uri() {
    // End-to-end: a `/theme` code span, once linkified, embeds a URI that the
    // click decoder maps back to the composer-prefill action — the contract the
    // in-app click handler relies on.
    let mut lines = vec![Line::from(vec![Span::styled("/theme", code_style())])];
    linkify_command_spans(&mut lines);

    let content = lines[0].spans[0].content.as_ref();
    // Recover the URI from between the OSC 8 introducer and the ST, exactly as a
    // terminal/click router would read the OSC 8 target.
    let open = crate::hyperlinks::open_sequence("squeezy:cmd:/theme");
    assert!(content.starts_with(&open));
    let action = parse_command_uri("squeezy:cmd:/theme").expect("decodes");
    assert_eq!(action.command, "/theme");
}
