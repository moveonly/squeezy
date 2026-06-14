//! Actionable help answers (ITEM 3): turn slash-command references inside a
//! rendered help/assistant answer into click-to-prefill hyperlinks.
//!
//! ## What this does
//!
//! When the assistant (or the local `/help` answerer) writes a slash command in
//! its reply — typically as an inline code span like `` `/theme` `` — a reader
//! has to retype it to act on it. This module detects those exact-command tokens
//! in the *already-rendered* markdown spans and rewraps each one as an OSC 8
//! hyperlink (§11.5 / 11G.5 machinery in [`crate::hyperlinks`]) carrying a
//! **custom internal URI scheme** rather than an `http(s)` address:
//!
//! ```text
//! squeezy:cmd:/theme
//! ```
//!
//! Activating such a link does NOT open a browser and does NOT execute the
//! command — it prefills the composer with the command text so the user can
//! review and press Enter (see [`CommandLinkAction`]). That keeps a click on a
//! help link safe-by-construction: it can never run a destructive `/clear` or
//! `/revert-turn` on its own.
//!
//! ## Why a pure module
//!
//! Like [`crate::hyperlinks`], everything here is pure and unit-testable without
//! a terminal:
//!
//!   1. **Detection** — [`linkify_command_spans`] walks a slice of rendered
//!      [`Line`]s, finds spans whose *visible text is exactly* a registered
//!      slash command (via [`crate::input::lookup_slash_command`], the single
//!      source of truth for the command set), and reports each as a
//!      [`CommandLink`] keyed by `(line, span)` with the encoded URI. Non-command
//!      spans — prose, paths, other code — are left untouched.
//!   2. **URI encoding** — [`command_uri`] builds the `squeezy:cmd:/<cmd>` form;
//!      [`parse_command_uri`] maps it back to a [`CommandLinkAction`] (the
//!      composer-prefill request). Round-tripping is total and lossless for any
//!      registered command, and a foreign scheme (`https://…`, a bare path)
//!      decodes to `None` so the existing URL/file hyperlink routing is never
//!      disturbed.
//!   3. **OSC 8 wrapping** — [`command_hyperlink_span`] reuses
//!      [`crate::hyperlinks::open_sequence`] / [`crate::hyperlinks::CLOSE_SEQUENCE`]
//!      to produce a span whose content is the visible token bracketed by the
//!      OSC 8 open/close escapes, for the raw-writer transcript-mirror path that
//!      already emits OSC 8 out-of-band (the visible glyphs are byte-for-byte the
//!      command text either way).
//!
//! The detection output ([`CommandLink`]) is deliberately span-addressed so the
//! in-app render path can register a frame-local hit-test target over the same
//! span and route a click through [`parse_command_uri`] — see the crate-level
//! wiring notes. This module owns the *what* (which spans, which URI); the
//! render/dispatch layers own the *where on screen* and the *do it*.

use ratatui::text::{Line, Span};

use crate::input::lookup_slash_command;

/// The custom URI scheme prefix for an actionable slash-command link. A link
/// whose target begins with this is routed *in-app* (prefill the composer),
/// never handed to the terminal/OS the way an `http(s)`/`file` link is.
///
/// The trailing form is the command verbatim including its leading slash, so the
/// full URI for `/theme` is `squeezy:cmd:/theme`.
pub(crate) const CMD_URI_SCHEME: &str = "squeezy:cmd:";

/// The decoded intent of activating a [`CMD_URI_SCHEME`] hyperlink: prefill the
/// composer with `command` (and do nothing else — never auto-submit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLinkAction {
    /// The slash command to drop into the composer, leading slash included
    /// (e.g. `/theme`). Always a value that [`lookup_slash_command`] accepts.
    pub(crate) command: String,
}

/// One detected actionable command reference inside a rendered answer, addressed
/// by its position in the `&[Line]` slice so the caller can both rewrap the span
/// for OSC 8 emission and register an in-app click target over the same cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLink {
    /// Index of the [`Line`] in the answer's line slice.
    pub(crate) line: usize,
    /// Index of the [`Span`] within that line's `spans`.
    pub(crate) span: usize,
    /// The command token, leading slash included (e.g. `/theme`).
    pub(crate) command: String,
    /// The encoded `squeezy:cmd:/…` URI for this command.
    pub(crate) uri: String,
}

/// Build the internal hyperlink URI for a slash `command` (leading slash
/// included), e.g. `command_uri("/theme") == "squeezy:cmd:/theme"`.
///
/// Pure string composition; it does not validate that `command` is registered
/// (callers detect via [`is_command_token`] first). The inverse is
/// [`parse_command_uri`].
pub(crate) fn command_uri(command: &str) -> String {
    format!("{CMD_URI_SCHEME}{command}")
}

/// Decode a hyperlink URI into the composer-prefill action it represents, or
/// `None` when it is not one of ours.
///
/// Returns `Some` only when `uri` starts with [`CMD_URI_SCHEME`] *and* its
/// payload is a currently-registered slash command — so a malformed or
/// unknown `squeezy:cmd:/bogus` decodes to `None` and can never prefill a
/// command that does not exist. Any other scheme (`https://…`, `file://…`, a
/// bare path) also returns `None`, leaving the existing URL/file open-routing
/// untouched.
pub(crate) fn parse_command_uri(uri: &str) -> Option<CommandLinkAction> {
    let command = uri.strip_prefix(CMD_URI_SCHEME)?;
    // Validate against the registry so the decoded action is always a real
    // command — a stale or hand-crafted URI for a removed command is rejected.
    lookup_slash_command(command)?;
    Some(CommandLinkAction {
        command: command.to_string(),
    })
}

/// True when `text` is *exactly* a registered slash command token (e.g.
/// `/theme`), with no surrounding whitespace or trailing argument.
///
/// This is the gate the linkifier applies to a span's visible text: a code span
/// rendered as `/theme` matches; one rendered as `/theme dark`, `theme`, or
/// `the /theme command` does not. Leading/trailing ASCII whitespace is trimmed
/// first so a `` `/theme ` `` code span (a stray space inside the backticks)
/// still resolves to `/theme`.
pub(crate) fn is_command_token(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && lookup_slash_command(trimmed).is_some()
}

/// Detect every span across `lines` whose visible text is exactly a registered
/// slash command, returning one [`CommandLink`] per match in line→span reading
/// order.
///
/// This is the read-only detector: it inspects the rendered spans but does not
/// mutate them, so a caller can decide independently whether to register an
/// in-app click target, rewrap the span for OSC 8 emission
/// ([`command_hyperlink_span`]), or both. Spans that are not exact command
/// tokens (prose, file paths, other inline code, the `(url)` suffix the
/// markdown renderer appends to real links) never appear in the result.
pub(crate) fn detect_command_links(lines: &[Line<'static>]) -> Vec<CommandLink> {
    let mut links = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        for (si, span) in line.spans.iter().enumerate() {
            let text = span.content.as_ref();
            if is_command_token(text) {
                let command = text.trim().to_string();
                let uri = command_uri(&command);
                links.push(CommandLink {
                    line: li,
                    span: si,
                    command,
                    uri,
                });
            }
        }
    }
    links
}

/// Rewrap a command span's content as an OSC 8 hyperlink span carrying the
/// internal `squeezy:cmd:` URI, preserving its style.
///
/// The returned span's `content` is `OPEN(uri) + visible + CLOSE`, reusing the
/// exact escape encoder the URL/file hyperlinks use
/// ([`crate::hyperlinks::open_sequence`] / [`crate::hyperlinks::CLOSE_SEQUENCE`])
/// so the bytes are identical in shape to a normal link — only the scheme
/// differs. This is for the **raw-writer** transcript-mirror path that emits
/// span content straight to the terminal out-of-band (where the escapes are
/// invisible). It is NOT for the ratatui `Buffer` render path, which measures
/// display width per cell and would mis-place the escape bytes; that path
/// instead registers an in-app click target over the plain span (see the
/// crate-level wiring notes).
///
/// `visible` is the command text the user sees (e.g. `/theme`); `style` is the
/// span's existing style, carried through unchanged.
///
/// REMAINING GAP: the live in-Buffer affordance (detect + register an interaction
/// hit target over each command span; see [`crate::register_command_link_targets`])
/// is wired, but the *OSC 8 scrollback-mirror* variant for command links is not:
/// the exit-mirror writer linkifies URLs by scanning the rendered `Buffer` cells
/// (`hyperlinks::find_links` + `open_sequence`), a different mechanism than rewrapping
/// `Line` spans. So this span-rewrap helper (and [`linkify_command_spans`]) stays a
/// pure, fully-tested substrate for that mirror variant when it lands; only the
/// unit tests reach it today, hence the targeted `dead_code` allow.
#[allow(dead_code)]
pub(crate) fn command_hyperlink_span(visible: &str, style: ratatui::style::Style) -> Span<'static> {
    let command = visible.trim();
    let uri = command_uri(command);
    let content = format!(
        "{}{visible}{}",
        crate::hyperlinks::open_sequence(&uri),
        crate::hyperlinks::CLOSE_SEQUENCE
    );
    Span::styled(content, style)
}

/// Detect command spans across `lines` and rewrap each matching span in place as
/// an OSC 8 `squeezy:cmd:` hyperlink (via [`command_hyperlink_span`]), returning
/// the [`CommandLink`] list for the spans that were rewrapped.
///
/// Mutates only the spans whose visible text is an exact command token; every
/// other span is left byte-for-byte unchanged, so unrelated link rendering and
/// prose are untouched. Intended for the raw-writer emission path; the returned
/// list lets a caller cross-reference which spans now carry an internal link.
///
/// See [`command_hyperlink_span`] for the remaining-gap note: the OSC 8
/// scrollback-mirror variant for command links is not yet wired (the live
/// in-Buffer path uses interaction hit targets instead), so this is reached only
/// from the unit tests today.
#[allow(dead_code)]
pub(crate) fn linkify_command_spans(lines: &mut [Line<'static>]) -> Vec<CommandLink> {
    let detected = detect_command_links(lines);
    for link in &detected {
        if let Some(line) = lines.get_mut(link.line)
            && let Some(span) = line.spans.get_mut(link.span)
        {
            let visible = span.content.clone().into_owned();
            *span = command_hyperlink_span(&visible, span.style);
        }
    }
    detected
}

#[cfg(test)]
#[path = "help_links_tests.rs"]
mod tests;
