# Conversation & Transcript Surface

> The scrollable transcript that renders assistant prose, reasoning, tool calls, diffs, and folds through a unified row model, plus the outline/index/change-summary overlays that navigate it.

**How it works today:** Incoming assistant text is split by `StreamingController` into committed / held / pending segments and flattened to one styled block per redraw; rich markdown and code render through cached tree-sitter highlighting. Dense entries compress into foldable lanes and duplicate tool outputs collapse into fold spans (exposed today through dedicated overlays). Vertical scroll carries a `follow_tail` intent with a top-right "↑ N from live · End to re-pin" badge, and wide blocks can pan horizontally when soft-wrap is toggled off. A faithful row model backs search, selection, and copy — and incremental search already highlights matches in place.

## Quick wins
- [Streaming patch preview renders unbounded search/replace bodies](#1-streaming-patch-preview-renders-unbounded-searchreplace-bodies)
- [Wide-block horizontal pan is discoverable only via the status line](#2-wide-block-horizontal-pan-is-discoverable-only-via-the-status-line)
- [Code-block syntax fallback is silent](#3-code-block-syntax-fallback-is-silent)
- [Code fences drop their language label](#4-code-fences-drop-their-language-label)
- [Truncated outline titles lose their full text](#5-truncated-outline-titles-lose-their-full-text)

## Findings

### 1. Streaming patch preview renders unbounded search/replace bodies
- **Category · Severity · Effort:** Friction · Medium · M
- **Today:** `render_streaming_preview` paints the in-flight `apply_patch` preview by appending every line of `partial.search` and `partial.replace` as diff rows, with no length cap. A large search or replace body streams in full into the preview frame.
- **Friction:** A multi-hundred-line patch field fills the viewport mid-stream, pushing surrounding context off-screen, before the user ever reaches the real approval prompt that the preview is only previewing.
- **Polish:** Bound the previewed search/replace bodies (e.g. cap rendered lines or chars per field), append an ellipsis row, and add a note like `(full patch shown at approval)` so the user knows the truncation is cosmetic. The structural `Partial` data can stay unbounded; only the rendered rows need clamping.
- **Refs:** `streaming_patch.rs:286` (`render_streaming_preview`), `streaming_patch.rs:323` (`append_diff_lines`)

### 2. Wide-block horizontal pan is discoverable only via the status line
- **Category · Severity · Effort:** Discoverability · Medium · S
- **Today:** `WideBlockView::status_hint()` returns `"soft-wrap on — Alt+w for horizontal scroll"`, but that string is only written to `app.status` when the user *already* toggles wrap (`toggle_soft_wrap`) or attempts a pan (`scroll_wide_block`). A wide diff or long command line renders soft-wrapped by default with no signal that an un-wrapped horizontal view exists.
- **Friction:** The pan feature — valuable precisely for wide diffs and command output — is invisible until the user happens to press the exact keys that surface its own hint, a chicken-and-egg discovery gap.
- **Polish:** When a block whose natural width exceeds the viewport first renders, surface a one-shot inline or gutter hint (`Alt+w to view unwrapped`) for a frame or two, then fall back to the existing status-line hint.
- **Refs:** `wide_block.rs:173` (`status_hint`), `lib.rs:15383` (`toggle_soft_wrap`), `lib.rs:15392` (`scroll_wide_block`)

### 3. Code-block syntax fallback is silent
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `highlight_code` returns unstyled `plain_lines(source)` in three cases with no marker: the fence carried no language hint, the hint didn't resolve to a supported grammar (`HighlightLanguage::from_hint` returned `None`), or the block exceeded `MAX_HIGHLIGHT_BYTES` / `MAX_HIGHLIGHT_LINES`. All three look identical to intentionally-plain text.
- **Friction:** The user can't tell "this is plain text by design" from "highlighting silently gave up" — and for the size-limit case, a large code block losing color looks like a rendering glitch.
- **Polish:** When falling back, paint a faint one-line label at the block top distinguishing the cause (`(no language)` vs `(unsupported: <hint>)` vs `(too large to highlight)`), or a muted left rail, so the plain rendering reads as a deliberate state.
- **Refs:** `render/highlight.rs:70` (`highlight_code`), `render/highlight.rs:138` (`exceeds_highlight_limits`)

### 4. Code fences drop their language label
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `finish_code_block` highlights the body via `highlight_code(block.language.as_deref(), source)` and extends the output with the resulting lines only. The fence's info string (`` ```rust ``) is parsed for the highlighter but never rendered — neither the language name nor any fence delimiter appears in the output.
- **Friction:** Two adjacent code blocks in different languages, or a code block butting against prose, read as one undifferentiated indented region; the user loses the "this is `rust`" / "this is `bash`" cue the author wrote.
- **Polish:** Emit a faint header row with the declared language (and a subtle top/bottom rule) before the highlighted body, mirroring how the diff/patch renderers already title their blocks.
- **Refs:** `render/markdown.rs:607` (`finish_code_block`), `render/markdown.rs:612`

### 5. Truncated outline titles lose their full text
- **Category · Severity · Effort:** Clarity · Low · M
- **Today:** `clean_title` caps each outline node label at `TITLE_CAP` (60) chars and appends an ellipsis when cut; only the truncated string is stored on `OutlineNode.title`, and the overlay paints exactly that (`Span::styled(node.title.clone(), …)`). The full `raw_title` is discarded after cleaning — there is no details pane or focus-time expansion.
- **Friction:** A long section title (e.g. a verbose tool description) ends mid-word with `…`; the user must jump to the entry in the main view to read the rest, defeating the outline's "scan without leaving your place" purpose.
- **Polish:** Retain the full title on the node and, on selection, reveal it — a wrapped details line under the list, or a second row for the focused node — so the cut text is recoverable without navigating away.
- **Refs:** `turn_outline.rs:171` (`clean_title`), `turn_outline.rs:186`, `lib.rs:29527` (overlay row paint)

### 6. Streaming live-tail segment is not visually staged
- **Category · Severity · Effort:** Feedback · Medium · M
- **Today:** `StreamingController` cleanly separates `committed` (newline-flushed), `held` (buffered inside an unclosed fence), and `pending` (the current incomplete line) and exposes them via `segments()` — but the render path consumes only the flattened `text()`. `segments()` has no production caller, so the painted transcript shows committed and live regions identically.
- **Friction:** Watching a long reply stream, the user can't tell whether a truncated fence at the bottom is still being written or whether the stream ended mid-fence — there's no "this region is live" marker, only an undifferentiated wall of text.
- **Polish:** Use the already-separated `held` + `pending` tail to stage the live region — a muted color or a quiet `↳ streaming` affordance on the tail while a turn is in flight — and drop it on settle. The data split exists; only the renderer needs to honor it.
- **Refs:** `streaming.rs:63` (`segments`), `streaming.rs:96` (`tail`), `transcript_surface.rs:415` (flattened `plain_text_of_line` path)

---

### Grounding notes (dropped from the draft)

These draft findings were checked against the code and dropped as inaccurate:

- **Empty outline overlay lacks an affordance** — `render_turn_outline_surface` already paints `"No transcript sections to outline yet."` when the outline is empty (`lib.rs:29446`), and the status line shows `"turn outline (empty) — Esc to close"` (`lib.rs:11474`).
- **Duplicate fold operates silently** — the duplicate-folds overlay already renders an `[+]`/`[-]` open/collapsed glyph, an `x{count} output` count, and a header hint `"… Enter jump/expand · Esc close"` (`lib.rs:29143`). The main-view inline projection is an explicit future follow-up (`duplicate_fold.rs:140`), not a silent regression.
- **Lane fold error flag prevents collapse silently** — false. `Lane::body_visible()` returns `!self.collapsed` for *every* lane, error lanes included; only the header is pinned visible (`always_visible()` is hard-coded `true`). Error-lane bodies collapse like any other; the toggle is honored, not ignored (`lane_fold.rs:163`, `lane_fold.rs:174`).
- **Search match highlighting is unimplemented** — false. Highlighting is implemented via `rows_with_search_highlight` + `search_match_style` / `search_current_match_style`, baked onto rows each draw (`lib.rs:32666`, `lib.rs:33431`). The empty `TranscriptRow::search_match_ranges` field is a dead alternative path, not the live mechanism.
- **Scroll-to-bottom unpins invisibly** — false. `render_scrolled_indicator` paints `↑ N from live · <key> to re-pin` in the top-right whenever scrolled off the tail, and registers a re-pin affordance (`lib.rs:33483`, `lib.rs:33830`).
- **Change summary empty state lacks guidance** — the change-summary overlay status already reads `"what changed since here: nothing observed since this point — m re-mark · Esc close"` (and `"(no transcript to mark)"`) when empty (`lib.rs:12755`).
