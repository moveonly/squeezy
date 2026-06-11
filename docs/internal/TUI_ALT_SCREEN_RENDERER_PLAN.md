# TUI Alt-Screen Renderer — Implementation Plan & Backlog

> Status: **Approved, not yet implemented.** Owner: TUI. Branch of record: `feat/append-only-renderer`.
> This document is the durable design, verification strategy, and feature backlog for moving the
> squeezy main view from an inline native-scrollback renderer to one always-on alternate-screen
> fullscreen renderer. The goal is not a quick workaround. The goal is a solid terminal
> application model that can be made rich, clickable, searchable, copyable, and reliably testable.

---

## 1. TL;DR

The squeezy main view tears and **stacks footer dividers on resize** in the VS Code terminal
(xterm.js). We exhausted every *inline* renderer (original `insert_before`, absolute-paint
footer band, pure cargo/indicatif print model, plain-text-anchor) and proved — with a real
`@xterm/headless` reproduction of a user drag log — that none can work. The fix is to render
the main view in the **alternate screen** (a fixed grid, fully redrawn each frame), exactly
like the Ctrl+T overlay that has been flawless throughout.

The happy surprise: the fullscreen renderer **already exists** (`render()`), and is already the
path every unit test and the eval harness exercise. So the core migration is deletion + lifecycle:
enter alt-screen at startup, draw the main view with the existing fullscreen renderer, and remove
the inline append/flush machinery. The new work is the product quality around that decision:
first-class in-app scrollback, search, copy, selection, exit persistence, robust teardown, and
terminal-emulator regression tests.

There should be **one supported renderer**. `SQUEEZY_FULLSCREEN` is only a temporary implementation
switch while this lands; it must not become a long-lived user-facing mode. Once fullscreen is
stable enough to dogfood, fullscreen becomes the default test oracle and the inline path is deleted
quickly. Two renderers mean two bug surfaces and hidden blindspots.

---

## 2. The problem

Symptom: in VS Code's integrated terminal, dragging the window (especially **width**) leaves a
stack of `☽────…` composer dividers of varying widths, and/or history "disappears" on shrink.
The Ctrl+T transcript overlay is always clean.

### 2.1 Root cause (settled empirically)

On resize, **xterm.js moves the cursor by the wrap-delta of the content *below* it**, not with
its own logical line. Concretely (reproduced in `@xterm/headless`):

```
before shrink 140→90:   row7 = "history line 8"   (cursor parked here)
                        row8 = ☽──────…(138 wide)
after shrink (reflow):  row7 = "history line 8"   (unchanged, short line)
                        row8 = ☽──────…(top of the divider, now wrapped to 2 rows)
                        row9 = ────…(continuation)
                        cursor → row8   ← DRIFTED DOWN BY 1 (the wrap-delta below it)
```

Because the divider is full-width by design, it re-wraps on any shrink; the parked cursor then
drifts by the unpredictable number of rows the wrap added below it. Every erase strategy —
absolute `MoveTo(remembered_row)`, cursor-relative `Clear(FromCursorDown)`, count-based
`MoveToPreviousLine(n)`, even a post-reflow `DSR` query — lands on the wrong row, so the previous
footer is stranded and the next one prints below it. Repeat per drag step → 22 stacked dividers
in the captured log.

This is **fundamental to any footer that lives in the normal buffer**. It is why codex's
hand-tuned DECSTBM streamer still has open VS Code resize bugs (openai/codex #8810, #18658,
#14277). The alternate screen is immune: it has no scrollback to reflow, and the app fully
redraws a fixed grid every frame, so there is no cursor to drift.

### 2.2 What we tried (all disproven inline)

| Attempt | Mechanism | Why it failed |
|---|---|---|
| Original | ratatui `Viewport::Inline` + `insert_before` | autoresize `append_lines` strands frames; cursor math desyncs vs xterm.js |
| Absolute band | footer painted at `MoveTo(h-fh+y)`, history `\r\n` | remembered row stale after reflow → stacks; band history lost on shrink |
| Pure print (indicatif) | history `\r\n`; footer printed; erase `FromCursorDown` from parked footer-top | cursor drifts on reflow → stacks (22 in real log) |
| Plain-text-anchor | park cursor on last *history* line, erase footer via down-1 | cursor still drifts by wrap-delta below it → stacks (controlled repro) |

The verification harness (`/tmp/xtermcheck`, `@xterm/headless`) is the oracle: it correctly
predicted the 16–22 stack on the inline binaries while the naive "events at byte offset" replay
falsely reported "clean." Trust the **per-frame width reconstruction** replay.

---

## 3. Chosen architecture: always-on alternate screen

Render the main view in the alternate screen, fullscreen, reusing the overlay's proven path. Treat
Squeezy as a real terminal application, not a line printer with a live footer attached.

- Composer/footer pinned at the bottom; scrollable transcript above.
- Mouse-wheel, scrollbar dragging, PgUp/PgDn/Home/End, search jumps, and turn/tool/error jumps
  all scroll the same in-app transcript model.
- Mouse capture is normal in fullscreen. Native terminal selection is no longer the primary
  mid-session copy path; Squeezy must provide selection/copy itself.
- On clean exit, the full conversation is **mirrored** into the terminal's native scrollback after
  leaving alt-screen so it persists and is selectable after quit.
- Mid-session copy of off-screen history is core scope: visual selection + copy-current-block +
  copy-last-answer + copy-transcript, using OSC 52 when available and durable fallbacks otherwise.
- Ctrl+T remains a detail/read mode, but not a separate renderer. It must share the same transcript
  row source, wrapping, cache, scrollbar, search, and selection primitives as the main view.

### 3.1 De-risking finding (the reason this is tractable)

`render()` (`crates/squeezy-tui/src/lib.rs:6867`) already computes the full-screen layout
(transcript + task + attachments + plan indicator + input + approval + status + subagent +
toasts) and already drives:
- unit tests via `render_to_string` (`lib_tests.rs:11289`), and
- the eval harness via `TuiHarness::render_frame` (`testing.rs:358`).

`render()` also already short-circuits to `render_transcript_overlay_surface` (`lib.rs:6870`)
when the overlay/config/status-line modes are active, so they layer on the same terminal for
free. The inline/append-only apparatus is a **production-only delta** we will delete. The
overlay already proves alt-screen survives resize on every terminal.

> Net: deletion of the append-only footer/flush machinery + a lifecycle move (enter alt-screen
> at startup, not lazily on Ctrl+T) + an exit-time mirror. Two terminals collapse into one.

### 3.2 Product contract

This migration is successful only if the fullscreen app replaces the parts of native scrollback
that users actually depended on:

- **Scroll:** wheel, trackpad, PgUp/PgDn, Home/End, scrollbar, jump-to-latest, and jump-to-turn are
  predictable across VS Code, iTerm2, tmux, Windows Terminal, and SSH.
- **Copy:** current visible selection, off-screen transcript ranges, code blocks, last assistant
  answer, tool output, and full transcript can be copied without leaving the app.
- **Search:** long sessions are navigable with incremental search, next/previous match, and
  highlighted matches.
- **Persistence:** a clean quit leaves a readable transcript in native scrollback, and every
  session can also be exported to a file without relying on terminal history.
- **Recovery:** crash, panic, Ctrl+C, SIGTERM, suspend/resume, and terminal resize never leave raw
  mode, hidden cursor, stuck mouse capture, or alt-screen active.
- **Parity:** tests and evals render the same surface users see. No production-only renderer path.

### 3.3 Renderer invariants

- The terminal normal buffer is not used for live UI. No main-view footer, divider, cursor anchor,
  or scroll region may live in the normal buffer during an active session.
- Every live frame is a fullscreen draw into alt-screen. Resizes are handled by recomputing layout
  from model state and redrawing the grid.
- The transcript model is the source of truth; rendered rows are cacheable projections keyed by
  entry revision, width, detail level, folds, search highlights, and theme.
- Scroll state is logical, not terminal-derived. It stores a position in the transcript row model
  and clamps against the current viewport on every render.
- Copy/search/navigation operate on the same wrapped-row projection the renderer uses, so copied
  text and highlighted ranges match what the user sees.
- Exit mirroring is an explicit shutdown operation, never `Drop` magic, because it needs `app` and
  must run after `LeaveAlternateScreen`.

### 3.4 Clean shutdown order

The exit mirror must write into the normal buffer, not the alt buffer. The intended clean shutdown
sequence is:

```text
stop accepting input
finish/cancel foreground terminal UI state
disable mouse reporting and alternate-scroll
leave alternate screen
write mirrored transcript to normal-buffer scrollback with CRLF rows
write the resume/exit hint
restore title, cursor, focus, bracketed paste, keyboard enhancement flags, and raw mode
flush stdout/stderr
```

`Drop` remains a best-effort emergency teardown only. A new explicit method such as
`TerminalGuard::finish_fullscreen(&mut self, app: &TuiApp)` should own the normal successful exit
path. Signal/panic handlers should call a smaller idempotent emergency path that leaves alt-screen
and restores terminal modes without trying to render or mirror.

### 3.5 Copy and clipboard model

Native terminal selection becomes a fallback, not the main in-session workflow. The app-owned copy
model should include:

- **Visual select:** keyboard and mouse range selection over wrapped transcript rows.
- **Semantic copy:** copy current entry, current code block, current tool output, last assistant
  answer, selected range, or full transcript.
- **Clean text:** strip rail/gutter glyphs, prompts, ANSI, spinners, and status-only decorations
  unless the user chooses "copy styled."
- **Format choices:** plain text, Markdown, JSON event slice, and HTML/styled transcript export.
- **Clipboard providers:** OSC 52 first when supported, platform clipboards where available
  (`pbcopy`, `wl-copy`, `xclip`, `clip.exe`), and temp-file fallback for large or unsupported
  payloads.
- **Privacy controls:** configurable max OSC 52 payload, confirmation for huge clipboard writes,
  and clear status feedback on success/failure.

### 3.6 Unified transcript surface

The main view and Ctrl+T must be two detail policies over one surface:

- Main view: collapsed, dense, follows tail by default, shows active work and recent context.
- Ctrl+T: expanded, deep-read, preserves position, exposes filters/folds/search, can inspect old
  content without disturbing main follow-tail state.
- Both views reuse the same row cache, row ids, entry ids, search index, selection engine, scrollbar
  geometry, and copy commands.
- Any bug fixed in wrapping, wide glyphs, folding, or copy should automatically fix both surfaces.

---

## 4. Decisions (confirmed with user)

1. **One supported renderer.** Fullscreen alt-screen is the product renderer. Inline exists only as
   a temporary migration/repro switch and is deleted as soon as fullscreen has copy, scroll,
   teardown, and persistence covered.
2. **Mouse: capture by default in fullscreen.** Reliable wheel scroll + clickable UI everywhere;
   native selection via Shift+drag where terminals support it. Escape hatch
   `SQUEEZY_MOUSE_CAPTURE=0` may exist during rollout, but the app must not rely on native
   selection for core copy workflows.
3. **Copy is core, not polish.** OSC 52 select→copy reuses `Osc52Clipboard` (`lib.rs:15548`), but
   the product contract also includes copy-last-answer, copy-code-block, copy-entry, export, and
   fallback providers.
4. **Ctrl+T stays as expanded/detail view**, distinct from the collapsed auto-following main view,
   but both are backed by one transcript row model. Do not duplicate wrapping/copy/search logic.
5. **Per-turn live native scrollback mirror is dropped.** You cannot append to native scrollback
   while displaying the alt buffer. Persistence = exit-time mirror + real-time side log/export +
   in-app copy while running.
6. **Exit mirror writes after leaving alt-screen.** Writing the mirror before `LeaveAlternateScreen`
   writes into the alternate buffer and is lost. The shutdown order is a first-class contract.
7. **`/clear` means a clean app surface.** It resets the conversation model and clears the
   fullscreen grid; any confirmation should live in status/toast, not as old transcript content
   that defeats "start from scratch."

---

## 5. Phased implementation

Every phase must compile and keep `cargo test -p squeezy-tui` green. The order below is about risk,
not speed. If we take our time, the standard is: no behavior becomes the default until the app has
credible replacements for native scrollback, copy, resize, and crash recovery.

Line numbers are approximate — they drift; use function names as anchors.

### Phase 0A — Minimal failing oracle

Land the smallest reproducible terminal-emulator check before changing the renderer.

- Bring the `/tmp/xtermcheck` node + `@xterm/headless` resize replay into the repo under
  `crates/squeezy-tui/tools/xtermcheck/` or `tools/termsim/xtermcheck/`.
- Capture the real inline bug: VS Code/xterm.js width drag, stacked `☽────` composer dividers,
  and history disappearing on shrink.
- Record per-frame width/height, not just byte offsets. The previous naive byte-offset replay was
  false confidence.
- Add a single command (`cargo xtask tui-xterm-replay` or equivalent) that runs the reproduction
  against a captured ANSI log.
- **Acceptance:** current inline renderer fails the check with >1 composer horizon; existing
  Ctrl+T/alt-screen path passes the same resize storm.

### Phase 0B — Full terminal simulation framework

Build the broader matrix described in §8. This can continue in parallel with the renderer work, but
the minimal xterm.js oracle from Phase 0A is required first.

- Add `TerminalWriter::Capture` (`terminal_writer.rs`) to tee emitted bytes for tests.
- Add a deterministic `SizeSource` for tests so resize frames do not require a live PTY.
- Extend `TuiHarness` with scenario steps:
  `Key`, `Mouse`, `Paste`, `Resize(w,h)`, `Tick`, `AssistantDelta`, `ToolOutput`,
  `SettleTurn`, `OpenOverlay`, `CloseOverlay`, `Frame`.
- Add Rust-native backends (`vt100`, `alacritty_terminal`) and keep xterm.js as the VS Code oracle.
- Add snapshot support for viewport grids and plain-text projections.
- **Acceptance:** term-matrix can prove inline fails and fullscreen passes on the stacked-divider
  invariant, plus no duplicated turn divider and no lost latest assistant response.

### Phase 1 — Fullscreen terminal lifecycle

Make fullscreen the real terminal lifecycle. A flag may exist only as a temporary migration switch.

- Add `RenderMode` only if it reduces patch risk. If practical, move directly to fullscreen.
  (A short-lived `SQUEEZY_INLINE_REPRO=1` path was considered but never shipped: the inline renderer
  was deleted outright in Phase 10 and no such flag exists in product code at HEAD.)
- In `TerminalGuard::enter` (`lib.rs:~18036`), build `Terminal::new` / `Viewport::Fullscreen`.
- Emit `EnterAlternateScreen`, `EnableAlternateScroll`, `Clear(All)`, `MoveTo(0,0)`,
  `EnableBracketedPaste`, `EnableFocusChange`, `Hide`, and keyboard enhancement setup.
- Make mouse capture default-on for fullscreen unless `SQUEEZY_MOUSE_CAPTURE=0`.
- Remove lazy overlay-terminal ownership from the main path. Ctrl+T becomes a state rendered by the
  same terminal, not an alt-screen terminal swap.
- `draw_app` (`~18152`) always runs `render(frame, app)` for the active surface.
- `Drop` becomes emergency-only: restore terminal modes and leave alt-screen if still active, but
  do not mirror transcript or write user-facing content from `Drop`.
- **Acceptance:** fullscreen boots, draws, resizes, and tears down cleanly; Ctrl+T/config/status-line
  overlays render on the same terminal; xterm.js resize replay shows zero stacked dividers.

### Phase 2 — Explicit shutdown and exit mirror

Implement clean exit as a first-class state transition before deleting inline.

- Add `TerminalGuard::finish_fullscreen(&mut self, app: &TuiApp) -> Result<()>`.
- On normal loop exit in `run_inner_with_terminal`, call `finish_fullscreen` before returning.
- Shutdown order:
  1. disable mouse reporting and alternate-scroll;
  2. leave alt-screen;
  3. render transcript mirror rows into the normal buffer;
  4. write the exit/resume hint;
  5. restore cursor/title/focus/bracketed paste/keyboard flags/raw mode;
  6. flush.
- Use the current terminal width for wrapping; if width is unavailable, use a conservative fallback
  such as 80 columns.
- Mirror collapsed-by-default because it matches the main view, but include a header that names the
  session and points to `/resume`/export for the expanded record.
- Never emit `\x1b[3J` during normal fullscreen exit. Preserve pre-launch terminal scrollback.
- Add idempotence guards so `finish_fullscreen` and `Drop` can both run without double-leaving
  alt-screen or double-restoring raw mode.
- **Acceptance:** byte tests prove mirrored rows appear after `LeaveAlternateScreen`; manual quit
  leaves pre-launch scrollback + mirrored conversation + exit hint visible.

### Phase 3 — Unified transcript row model

Extract the row projection that both main view and Ctrl+T use.

- Introduce a `TranscriptSurface` or equivalent pure model that turns `TuiApp` transcript state into
  stable logical rows.
- Each row should have:
  `row_id`, `entry_id`, `entry_kind`, `detail_policy`, `visual_line_index`, `text_range`,
  `copy_range`, `style_spans`, `fold_state`, `search_match_ranges`, and optional click targets.
- Widen `transcript_scroll_from_bottom` from `u16` to `usize`. Convert to `u16` only at render time.
- Replace overlay-only wrapping helpers with shared helpers:
  `build_transcript_rows(app, width, detail_policy)` and
  `visible_transcript_rows(rows, viewport_height, scroll_state)`.
- Main policy: collapsed, dense, follow-tail unless scrolled up.
- Ctrl+T policy: expanded, deep-read, independent scroll position.
- Cache rows by `(render_cache_session, entry_id, entry_revision, width, detail_policy, theme,
  fold_state, search_query)`.
- **Acceptance:** main and Ctrl+T produce consistent wrapping/copy ranges from one source; scroll
  math handles >65k visual rows; property tests clamp scroll positions under resize.

### Phase 4 — Main fullscreen render and scroll UX

Make the existing `render()` path the only user-visible main view.

- Composer/footer pinned at bottom, transcript above, status/task/subagent panels stable under
  height changes.
- Add a main-view scrollbar using the shared scrollbar geometry. It should support click-to-jump and
  drag when mouse capture is enabled.
- Add follow-tail behavior:
  - newest output auto-scrolls only when pinned to bottom;
  - user scroll-up freezes position;
  - `End`/click latest re-pins;
  - a visible compact indicator shows "scrolled" vs "live".
- Add scroll commands: wheel/trackpad, Shift-wheel horizontal or soft-wrap toggle, PgUp/PgDn,
  Home/End, previous/next user turn, previous/next assistant answer, previous/next tool call,
  previous/next error.
- Add smooth-scroll primitives:
  - accumulate wheel/trackpad deltas instead of dropping fast small events;
  - coalesce scroll storms to the frame budget;
  - animate large jumps only when reduced-motion is off;
  - keep the logical anchor stable throughout animation;
  - cancel animation immediately on new user input;
  - expose a config switch for instant scrolling.
- Add resize behavior: preserve logical anchor row when scrolled up, pin to latest when following.
- **Acceptance:** unit tests for every scroll command; resize while scrolled up keeps the same
  logical content visible; resize while following stays at latest.

### Phase 5 — Selection, copy, and export

Treat copy as part of the renderer migration, not a later nicety.

- Add visual selection mode over `TranscriptSurface` rows:
  - keyboard anchor/cursor movement;
  - Shift+Up/Down extends by visual row;
  - Shift+PgUp/PgDn extends by page;
  - Shift+Home/End extends to entry/session boundary depending on focus;
  - mouse drag start/extend/end;
  - double-click word, triple-click row/entry where feasible;
  - visible highlight in main and Ctrl+T.
- Add natural focused-row actions:
  - `Ctrl+O` expands/collapses the focused entry inline;
  - Enter opens the primary action for the focused row (expand, open link, or inspect);
  - `Ctrl+Enter` opens the focused entry in Ctrl+T/detail view;
  - Esc clears selection/search/detail focus before leaving the app;
  - copy acts on the selection if present, otherwise the focused semantic unit.
- Add semantic copy commands:
  - copy selection;
  - copy current entry;
  - copy code block under cursor;
  - copy last assistant answer;
  - copy current tool output;
  - copy visible viewport;
  - copy full transcript.
- Add output formats: plain, Markdown, JSON event slice, and optionally ANSI/styled HTML export.
- Add clipboard provider chain:
  1. OSC 52 if supported and payload is under configured limit;
  2. platform clipboard command if available;
  3. write temp file and surface path/status;
  4. failure toast with exact reason.
- Add `/export md|txt|json|html [path]` using the same copy formatter.
- Add privacy and safety:
  - configurable OSC 52 max bytes;
  - confirmation for large clipboard writes;
  - no automatic clipboard writes except explicit copy commands;
  - redact/omit secrets only if an existing redaction pipeline exists; otherwise never pretend.
- **Acceptance:** copy range tests across wrapped lines, wide glyphs, rails, code fences, and tool
  output; manual OSC52 + platform clipboard + temp-file fallback checks.

### Phase 6 — `/clear`, startup, resume, and pickers

Make lifecycle screens use fullscreen deliberately.

- Pickers (`resume_picker::run_picker`, `startup_model_picker::run_picker`) draw as modal/fullscreen
  surfaces on the same terminal. Clear once after they close.
- `draw_startup_placeholder` stays fullscreen and cannot leave stale rows behind.
- `/clear` resets app state and clears the grid. Confirmation should be status/toast text, not a
  transcript entry, if "start from scratch" is the desired contract.
- Resume first frame renders prior transcript from the persisted app model immediately; no native
  scrollback replay is involved.
- Switching sessions, `/fork`, `/resume`, `/compact`, and plan-mode overlays must all redraw from
  model state without depending on historical terminal bytes.
- **Acceptance:** picker/resume/clear/session-switch tests use the same fullscreen render path; no
  ghost rows after modal close; `/clear` leaves a clean transcript surface.

### Phase 7 — Search, filters, folds, and Ctrl+T detail mode

Make long-session navigation good enough to replace terminal scrollback habits.

- Add incremental search:
  - `/` or configured key opens search;
  - matches highlighted in main and Ctrl+T;
  - next/previous match scrolls to stable row ids;
  - search can include/exclude tool output and reasoning.
- Add detail filters in Ctrl+T:
  - all;
  - user/assistant only;
  - tool calls;
  - errors;
  - subagent;
  - specific tool;
  - current turn.
- Add fold controls:
  - per-entry expand/collapse in main;
  - expand/collapse all in Ctrl+T;
  - remember fold state across overlay opens;
  - preserve fold state through resize.
- Add clickable affordances:
  - disclosure caret per foldable entry;
  - copy buttons for code/tool blocks;
  - jump-to-latest;
  - queue strip and status widgets.
- **Acceptance:** search/fold/filter state is represented in the shared row model and does not fork
  renderer code.

### Phase 7B — Direct manipulation for transcript and queue

Make common actions feel like a native app, not a command-only TUI.

- Transcript cards:
  - click disclosure/caret to expand or collapse an entry inline;
  - click the card header to focus/select it;
  - double-click a collapsed card to expand it;
  - `Shift+Up/Down` extends selection across rows;
  - `Ctrl+O` toggles focused entry expansion;
  - `Ctrl+Enter` opens the focused entry in Ctrl+T/detail view;
  - copy command copies the selected row range or focused entry;
  - hover reveals small action targets only when mouse capture is active.
- Prompt queue:
  - click the queue strip to expand/collapse it;
  - each queued prompt has a delete `x` target;
  - each queued prompt has a drag handle for reorder;
  - drag/drop reorders with a live insertion marker;
  - Shift+Up/Down remains the keyboard reorder equivalent;
  - Delete/Backspace removes the focused queued item;
  - Enter closes the queue or runs the selected item when idle, depending on final policy;
  - Undo restores the last deleted/reordered queue item if feasible.
- Shared primitives:
  - all clickable regions are registered through one frame-local hit-test registry;
  - every mouse operation has a keyboard equivalent;
  - drag state is model state, not terminal cursor position;
  - click targets are stable across resize because they derive from row ids and rects.
- **Acceptance:** unit tests for hit-testing, keyboard parity, queue reorder/delete, drag cancel,
  resize while dragging, and inline expand/collapse without opening Ctrl+T.

### Phase 8 — Performance, memory, and rendering quality

Optimize for huge sessions before declaring the renderer done.

- Add revision-keyed row caches for transcript entries and wrapped rows.
- Virtualize rendering: only materialize expensive styled rows near the viewport when possible.
- Keep an LRU for old rendered rows keyed by width/detail/theme; never allow unbounded memory growth.
- Add frame budget instrumentation:
  - render time;
  - bytes emitted;
  - rows built;
  - cache hit/miss;
  - longest entry wrap time.
- Keep idle redraw at zero. Redraw only on state change, resize, animation, or input.
- Confirm DEC 2026 synchronized output brackets every fullscreen frame when enabled.
- Wide-glyph audit:
  - CJK;
  - emoji;
  - moon/spinner glyphs;
  - combining marks;
  - ZWJ sequences;
  - ambiguous-width terminals.
- Add density modes only if needed: compact/default/expanded, backed by the same layout primitives.
- **Acceptance:** stress tests with huge tool output, thousands of transcript entries, wide glyphs,
  fast streaming, and resize storms stay responsive and bounded.

### Phase 9 — Signals, crash safety, suspend/resume, and platform hardening

Own terminal recovery completely.

- Add a panic hook that leaves alt-screen, disables mouse, shows cursor, disables bracketed paste,
  restores keyboard flags, and disables raw mode before printing panic output.
- Handle Ctrl+C/cancel paths separately from process teardown; cancelling a turn should not tear
  down the TUI unless it is the user's explicit exit gesture.
- Handle SIGTERM/SIGHUP best-effort with emergency teardown.
- Handle Ctrl+Z/SIGTSTP:
  - restore terminal before suspend;
  - on resume, re-enter alt-screen, clear, and redraw from model state.
- Windows/ConPTY:
  - verify alt-screen enter/leave;
  - mouse reporting;
  - clipboard fallback;
  - resize events;
  - panic teardown.
- tmux/screen/SSH:
  - alternate-scroll behavior;
  - OSC52 passthrough limits;
  - slow-link frame coalescing.
- **Acceptance:** manual and automated teardown tests never leave the terminal unusable after panic,
  Ctrl+C, SIGTERM, suspend/resume, or crash simulation.

### Phase 10 — Delete inline and make tests reflect reality

Delete the second renderer, not just hide it.

- **Delete:** `paint_main`, `prepare_history`, `render_footer_to_buffer`, `render_inline`,
  `live_settling_lines`, `settling_flush_boundary`, `inline_history_lines_for_flush*`,
  `capped_footer_height`, `footer_content_height`, `emit_buffer_as_lines`,
  `clear_scrollback_and_visible`, the `sync_overlay_screen`/`enter_overlay_screen`/
  `leave_overlay_screen` trio, `INLINE_VIEWPORT_HEIGHT`, `RESET_AND_CLEAR_VISIBLE`,
  `overlay_terminal`, `overlay_screen_active`, `startup_flushed`, `transcript_flushed_len`,
  `turn_divider_flushed_generation`, and `footer_painted`.
- **Keep/refactor:** `render_lines_to_owned_buffer`, `emit_buffer_row_styled`,
  `render_lines_to_buffer`, `term_display_width`, and `is_wide_rendered_glyph` only if the mirror,
  export, or copy formatters still need them.
- Delete inline-contract tests:
  `append_only_history_emits_plain_newline_terminated_text`, `emit_buffer_as_lines_*`,
  `idle_footer_repaint_produces_no_diff`, `capped_footer_height_reserves_one_row_for_history`,
  hard-clear reflush tests, and `render_inline_to_string` tests.
- Convert any valuable content assertions to `render_to_string` / `TranscriptSurface` tests.
- Eval/Wave-2 should remain unchanged if it already drives `render()`.
- **Acceptance:** zero dead-code warnings; no product flag for inline; no separate overlay terminal;
  CI and dogfooding exercise the same renderer users get.

---

## 6. Risks & mitigations

1. **Native scrollback no longer exists mid-session.** This is the main UX cost. Mitigation:
   in-app scroll/search/copy must be first-class; clean exit mirrors the transcript after leaving
   alt-screen; `/export` and real-time side logs cover long-running sessions and crashes.
2. **Copy regressions feel severe.** Users expect drag/select/copy to work somewhere. Mitigation:
   visual selection, semantic copy commands, OSC 52, platform clipboard fallback, temp-file fallback,
   and explicit status feedback ship before fullscreen is considered complete.
3. **Mouse capture changes muscle memory.** Wheel and clicks improve, but native drag selection
   changes. Mitigation: Squeezy-owned selection/copy, Shift+drag where terminal supports it,
   documented escape hatch during rollout, and keyboard-only equivalents for every mouse action.
4. **Exit mirror ordering can be wrong.** Writing before `LeaveAlternateScreen` loses the transcript.
   Mitigation: explicit `finish_fullscreen(app)` owns clean exit; tests assert the byte order:
   leave alt-screen before CRLF mirror rows.
5. **Terminal can be left broken on panic/signal.** Fullscreen raises the cost of bad teardown.
   Mitigation: idempotent emergency teardown, panic hook, signal handling, suspend/resume support,
   and manual crash tests.
6. **Two renderers create blindspots.** Mitigation: inline is a temporary repro switch only; tests,
   evals, and dogfooding move to fullscreen quickly; inline code is deleted, not kept as fallback.
7. **Huge transcript performance.** Fullscreen makes Squeezy responsible for scrollback. Mitigation:
   `usize` scroll model, revision-keyed row cache, virtualization, LRU limits, frame limiter, and
   perf counters for rows built/bytes emitted/render time.
8. **Ctrl+T becomes a second hidden surface.** Mitigation: Ctrl+T shares the exact transcript row
   model, wrapping, search, selection, cache, and copy formatter with the main view.
9. **Terminal capability variance.** OSC 52, DEC 2026, alternate-scroll, focus events, and truecolor
   vary. Mitigation: capability probing, clear fallbacks, emulator matrix, and platform-specific
   tests for VS Code/xterm.js, tmux, iTerm2, Windows Terminal, and SSH.
10. **`/clear` semantics drift.** Mitigation: define it as model reset + clean app grid; surface
    confirmation via status/toast if the product contract is "start from scratch."

---

## 7. Verification

- `cargo build -p squeezy-tui && cargo test -p squeezy-tui` green every phase.
- `cargo test -p squeezy-eval` green; eval screenshots/frames exercise the same renderer as users.
- `cargo xtask term-matrix` or `cargo test -p squeezy-tui --features term-matrix` runs the
  emulator-backed scenario matrix.
- xterm.js replay of fresh `SQUEEZY_TUI_WRITE_LOG=/tmp/sq.ansi` VS Code resize-drag captures →
  **0 stacked dividers**. Use per-frame width reconstruction, not byte-offset replay.
- Snapshot tests for plain and styled fullscreen grids across representative widths/heights:
  40x10, 80x24, 120x40, 200x60, and very narrow widths.
- Pure row-model tests for wrapping, folds, search highlights, copy ranges, wide glyphs, and scroll
  anchoring.
- Clipboard tests for OSC 52 payload encoding, platform fallback selection, payload limits,
  temp-file fallback, and copy formatter output.
- Shutdown byte-order tests: `LeaveAlternateScreen` precedes mirrored CRLF transcript rows; normal
  exit omits `\x1b[3J`; emergency teardown does not attempt mirror.
- Signal/crash tests where feasible: panic hook, Ctrl+C path, SIGTERM best effort, Ctrl+Z suspend
  and resume.
- Manual matrix — VS Code (xterm.js), iTerm2, Apple Terminal, Windows Terminal (ConPTY), tmux,
  screen, SSH/remote shell — each:
  startup picker, resume picker, first frame, resize drag mid-stream, wheel/trackpad scroll,
  scrollbar drag, PgUp/PgDn/Home/End, search, visual select/copy, Ctrl+T round-trip, `/clear`,
  copy large code block, quit-and-inspect-scrollback, panic simulation.

### 7.1 The reproduction harness (the seed of §8)
Located today at `/tmp/xtermcheck` (node + `@xterm/headless@6`). Capture: a `pty.fork` harness
(`/tmp/ptyturn2.py`, `/tmp/ptywidth.py`) runs `target/debug/squeezy` with `SQUEEZY_TUI_WRITE_LOG`,
answers DSR, and drives resizes via `TIOCSWINSZ`, recording `(byteOffset, cols, rows)` events.
Replay: split the byte log on DEC-2026 BEGIN markers into frames; recover each frame's width from
the `☽`-divider dash count (+margin) and height from max CUP row; `term.resize(w,h)` per frame;
count `/☽\s*[─╌┈]/` lines in the viewport. >1 = stacking. This becomes the **xterm.js leg** of the
framework in §8. After the fullscreen migration, the same harness should verify the absence of
stacking, no lost latest response, stable cursor bounds, and stable visible anchor during resize.

---

## 8. Terminal-emulator simulation test framework

> **Why this exists:** the entire bug saga came down to *emulators behaving differently on resize*
> (xterm.js reflows + drifts the cursor; others don't). A single emulator's "looks fine" is not
> proof. This framework replays squeezy's real output through **several emulator models** and
> asserts invariants across the matrix, so a render regression on *any* common terminal is caught
> in CI — not by a user dragging a VS Code window.

### 8.1 Architecture (capture once, replay through many emulators)

```
                       ┌─ scripted scenario (keys + resize events) ─┐
                       │                                            │
  TuiApp + Agent ───────► render() ───────► TerminalWriter::Capture ──► byte log + frame marks
                       │  (deterministic SizeSource injects W×H)    │
                       └────────────────────────────────────────────┘
                                          │
            ┌──────────────┬──────────────┼───────────────┬───────────────┐
            ▼              ▼              ▼               ▼               ▼
        xterm.js      alacritty_term     vt100         wezterm-term       tmux
        (@xterm/        (Rust, reflow)   (Rust,        (Rust, optional)   (real, opt)
         headless,                        fixed grid)
         node)
            └──────────────┴──────────────┴───────────────┴───────────────┘
                                          │
                                  invariant assertions + grid snapshots (insta)
```

The key separations: **capture is deterministic and emulator-free** (drive the guard with an
injected size, tee the bytes); **replay is pluggable** across emulator backends, each modelling a
class of real terminal.

### 8.2 Capture seam (deterministic, node-free)

1. **`TerminalWriter::Capture`** variant (`terminal_writer.rs`) that tees every emitted byte into an
   `Arc<Mutex<Vec<u8>>>` (and optionally a real sink). Exposed via the `testing` feature.
2. **`SizeSource`** — replace direct `terminal_size()` calls in lifecycle/render helpers with a
   small injectable source (a closure or trait on `TerminalGuard`, default = real
   `crossterm::terminal::size`). Tests set it to a scripted `(w,h)` timeline so resizes are
   deterministic — **no PTY, no real terminal, no flakiness.**
3. **Scenario driver** — extend `TuiHarness` (`testing.rs`) with `drive_scenario(&[Step])` where
   `Step` ∈ { `Key(KeyEvent)`, `Mouse(MouseEvent)`, `Paste(String)`, `Resize(w,h)`, `Tick`,
   `AssistantDelta(text)`, `ToolOutput(text)`, `SettleTurn`, `OpenOverlay`, `CloseOverlay`,
   `CopyCommand(kind)`, `Search(query)`, `Frame` }. Each `Frame` step renders and records a
   `(byte_offset, w, h)` mark. Emits a `CaptureLog { bytes, frames: Vec<FrameMark> }`.
   - Bonus: keep the PTY path (`tools/xtermcheck/pty_capture.py`) as an *end-to-end* capture that
     exercises the real binary + real stdin/DSR, for the cases the harness can't model.

### 8.3 Emulator backends (the "different common ones")

Each backend takes a `CaptureLog`, replays it frame-by-frame applying the recorded resizes, and
exposes a uniform `Grid { viewport: Vec<String>, alt_screen: Vec<String>, scrollback: Vec<String>,
cursor: (u16,u16), base_y: u16 }` for assertions. Backends are trait objects
(`trait Emulator { fn replay(&self, log: &CaptureLog) -> Grid; fn profile(&self) -> EmulatorProfile; }`).

| Backend | Models | Reflow on resize | Cursor-on-reflow | Runtime | CI |
|---|---|---|---|---|---|
| **xterm.js** (`@xterm/headless@6`, node) | **VS Code**, code-server, Hyper, web terminals | yes | **drifts by below-content wrap-delta** (the bug) | node subprocess | gated on node present (required leg on a node runner) |
| **alacritty_terminal** (Rust crate) | Alacritty, Kitty-ish, GPU terms | yes | tracks logical line | in-process | always |
| **vt100** (Rust crate) | fixed-grid / legacy / conservative emulators, screen capture | no (fixed grid) | n/a | in-process | always |
| **wezterm-term / termwiz** (Rust, optional) | WezTerm | yes | tracks | in-process | optional |
| **tmux** (real, optional) | tmux/screen multiplexers | yes (own model) | own | tmux subprocess | gated on tmux present; note width-1 glyph caveat |

> Rationale: the two **always-on Rust** backends (alacritty_terminal reflow + vt100 fixed-grid)
> bracket the behavior space and run node-free in CI. The **xterm.js** leg is the definitive VS
> Code oracle (it *is* VS Code's engine) and is the required gate on a runner that has node. tmux
> + wezterm are bonus coverage. Encode each backend's `EmulatorProfile { reflows, cursor_tracking,
> ambiguous_glyph_width }` so the matrix documents *why* a terminal differs.

### 8.4 Scenario matrix

A fixed set of scripted scenarios, each run against every applicable backend:
- `startup` (picker dismissed → first frame)
- `single_turn` (a multi-line streaming response that settles)
- `shrink_then_grow` (W: 140→90→140, H stable)
- `width_drag_storm` (W oscillates 250↔195 over ~30 frames — the real trigger)
- `height_storm` (H oscillates 64↔12)
- `combined_storm` (both dims)
- `overlay_round_trip` (Ctrl+T open → scroll → Esc)
- `clear` (`/clear` mid-session)
- `long_transcript_scroll` (PgUp/Home/End/wheel over a 500-entry transcript)
- `resume` (load a saved session → first frame)
- `visual_select_copy` (select wrapped rows → copy)
- `search_resize` (search match active while resizing)
- `copy_large_block` (OSC52 limit → fallback)
- `panic_teardown` (emergency exit sequence)
- `clean_exit_mirror` (leave alt-screen → mirror transcript)
- `suspend_resume` (leave terminal clean → re-enter and redraw)

### 8.5 Invariants (asserted per scenario × backend)

Hard fails:
- **≤ 1 live composer horizon** (`/☽\s*[─╌┈]/`) in the viewport at every settled frame (the
  stacked-divider bug).
- **No duplicated turn divider** (`Worked for …`) beyond the legitimate count.
- **Latest assistant response present** after any resize (history not lost).
- **Cursor within `[0, h)`**, never orphaned below content.
- **No normal-buffer live UI writes while active** — after `EnterAlternateScreen`, all live frames
  affect the alternate buffer until clean exit.
- **Selection/copy ranges are stable** — copying a range before and after resize yields the same
  logical text when the same row ids are selected.
- **Search anchors survive resize** — active match stays selected/logically visible after width and
  height changes.
- **Clean exit byte order** — `LeaveAlternateScreen` precedes mirrored transcript rows; normal exit
  does not purge scrollback.
- **Emergency teardown restores modes** — disable mouse, disable bracketed paste/focus, show cursor,
  leave alt-screen, and raw mode off.

Soft / snapshot:
- **`insta` grid snapshots** of the final viewport per (scenario, backend) — catches layout drift
  the contains-assertions miss. Styled + plain variants.
- **Frame-count / byte-count budget** (perf guard) — a storm shouldn't emit unbounded bytes.
- **Cache metrics** — row build count and render time should remain bounded for repeated redraws.
- **Clipboard provider trace** — tests can assert OSC 52 vs platform fallback vs temp-file fallback
  without touching the real clipboard.

### 8.6 Layout & CI

- Repo home: `crates/squeezy-tui/tools/termsim/` (node xterm.js leg + scenario JSON) and a Rust
  module `crates/squeezy-tui/src/termsim/` (capture seam, `Emulator` trait, alacritty/vt100
  backends, scenarios, assertions), behind a `term-matrix` feature so it doesn't bloat the release
  build.
- `cargo xtask term-matrix` runs everything available; a thin `#[test]` wrapper (feature-gated)
  runs the Rust backends so `cargo test` covers them by default on contributors' machines.
- CI: one Linux job installs node + tmux and runs the **full** matrix as a required gate; the
  default test job runs the Rust-native backends (no node) so PRs are still guarded without the
  heavyweight leg.
- Determinism: no real PTY in the unit path (the `SizeSource` + `Capture` writer make it pure);
  the PTY/real-binary capture is a separate, opt-in end-to-end test.
- Snapshots: store small, curated fixtures only. Huge logs live as generated artifacts, not checked
  into the repo.
- Failure output: save the final viewport, scrollback tail, byte-range around the failing frame,
  scenario steps, terminal profile, and terminal dimensions to make failures debuggable.

### 8.7 What it proves for THIS migration
- **Before:** run the current inline binary through it → reproduces the 22-stack on the xterm.js +
  alacritty (reflow) backends, clean on vt100 (fixed grid). That asymmetry *is* the bug, now a
  test.
- **After (alt-screen):** the emitted byte stream is absolute cell writes over a fixed grid, so
  **all** backends — reflow and fixed-grid alike — produce identical, single-footer, no-loss grids
  across every storm. The matrix going green across all backends is the durable proof the fix
  holds on every terminal class, and the guard against ever regressing to a reflow-dependent path.
- **After (product completeness):** selection, copy, search, clean exit mirror, and emergency
  teardown are validated through the same byte-stream and row-model fixtures. The renderer is not
  "done" just because the divider stopped stacking.

---

## 9. Rollout & flag lifecycle

This is not a plan to maintain two renderers.

1. Keep an inline repro switch only until the xterm.js harness can prove the old bug and the
   fullscreen path can be dogfooded.
2. Move local dogfooding and CI snapshots to fullscreen as soon as Phase 1 + Phase 2 are stable.
3. Treat copy/selection, clean exit mirror, `/clear`, and emergency teardown as blockers for calling
   fullscreen complete.
4. Delete inline once fullscreen has passed the resize, copy, exit, and signal gates. Do not keep an
   end-user inline fallback after that point.
5. ~~If an emergency kill switch remains temporarily, name it like a temporary escape hatch
   (`SQUEEZY_INLINE_REPRO=1`), document that it is unsupported, and keep it out of normal CI except
   for the legacy-failure harness.~~ *(Not done: no kill switch was kept. Inline was deleted in
   Phase 10 and `SQUEEZY_INLINE_REPRO` was never wired into product code, so there is no inline
   fallback to gate.)*
6. Update docs/help to describe Squeezy as an app-owned fullscreen TUI with in-app scroll/search/copy,
   not as a native scrollback append-mode terminal program.

---

## 10. Critical files (anchors)

- `crates/squeezy-tui/src/lib.rs` — `TerminalGuard`, `enter`, `draw_app`, `Drop`, `render`
  (`6867`), scroll model (`8685`/`9240`), alt-screen helpers (`240`/`251`), mirror pipeline
  (`8311`/`7955`/`18598`/`18514`), `Osc52Clipboard` (`15548`), end of `run_inner_with_terminal`.
- `crates/squeezy-tui/src/resume_picker.rs` (`run_picker` `558`).
- `crates/squeezy-tui/src/commands.rs` — `/clear` model reset.
- `crates/squeezy-tui/src/lib_tests.rs` — delete inline tests, add fullscreen/mirror tests.
- `crates/squeezy-tui/src/testing.rs` (`render_frame` `358`) — already fullscreen.
- `crates/squeezy-tui/src/terminal_writer.rs` — add `Capture` writer variant for the test framework.
- New/extracted modules to consider:
  - `terminal_guard.rs` — lifecycle, enter, clean finish, emergency teardown.
  - `transcript_surface.rs` — shared row model, wrapping, row ids, copy ranges.
  - `scroll.rs` — follow-tail, anchor preservation, viewport math, scrollbar geometry.
  - `selection.rs` — visual selection, row/character range math, semantic copy targets.
  - `clipboard.rs` — OSC 52, platform providers, temp-file fallback, payload limits.
  - `search.rs` — incremental transcript search and match navigation.
  - `interaction.rs` — focus, gestures, click targets, hover state, direct manipulation commands.
  - `queue_surface.rs` — prompt queue rows, drag/drop, delete buttons, and keyboard/mouse parity.
  - `termsim/` — capture seam, emulator adapters, scenario fixtures, invariants.

---

## 11. Capability backlog

Grouped by theme. Some items are core to replacing native scrollback/copy; others are longer-horizon
quality improvements. Rough size: S/M/L/XL.

### 11.1 Selection & clipboard
- **(M) Minimal copy commands before default flip** — copy last assistant answer, copy selected
  range, copy current code block, copy current tool output, copy full transcript.
- **(M) Rich visual-select mode** — beyond row-range: character-granular selection with anchor +
  cursor cells, line-wrapped correctly, with a visible highlight. Reuse the wrapped-row model.
- **(M) Mouse selection parity** — click-drag ranges, Shift+click extend, double-click word,
  triple-click entry/paragraph where terminal event fidelity allows it.
- **(M) Keyboard-native selection** — Shift+Up/Down, Shift+PgUp/PgDn, Shift+Home/End, select entry,
  select code block, select tool output, clear selection, and copy selection. This should feel like
  a text surface even though Squeezy owns the grid.
- **(M) Smart copy** — strip rail-gutter glyphs (`│ ├ ╰ ─`) and ANSI from copied text so pasted
  content is clean prose/code, not box-drawing. Add a "copy as markdown" variant.
- **(S) Copy code block under cursor** — detect fenced code in the transcript and a one-key
  "copy this block" affordance (we already parse fences in `streaming.rs`).
- **(M) OSC 52 chunking / fallback** — payloads > 8 KiB: chunk per the spec where supported;
  otherwise fall back to writing a temp file and showing its path (or `pbcopy`/`wl-copy`/`clip`
  detection). Probe terminal capability once.
- **(M) Clipboard provider chain** — explicit providers for OSC 52, macOS `pbcopy`, Wayland
  `wl-copy`, X11 `xclip`/`xsel`, Windows `clip.exe`/PowerShell, and temp-file fallback.
- **(S) Clipboard status toasts** — "copied 37 lines", "wrote /tmp/squeezy-copy.md", or exact
  failure reason; never silently fail.
- **(S) Copy visible viewport** — useful when the user wants exactly what is on screen.
- **(M) Copy with provenance** — include entry metadata, tool name, timestamps, and file paths in
  Markdown/JSON copy modes.
- **(S) Quote-to-compose** — select transcript text → `>` quotes it into the composer.
- **(M) Redaction hook** — if a redaction engine exists elsewhere, route copy/export through it;
  otherwise keep copy exact and do not create fake safety.

### 11.2 Scrolling & navigation
- **(M) Scrollbar in the main view** — reuse `transcript_overlay_scrollbar_geometry`/render; show
  position + draggable thumb (gated on mouse capture).
- **(S) Search in transcript** — `/`-style incremental find with next/prev, highlight matches,
  scroll-to-match. Big usability win for long sessions.
- **(M) Precision scroll pipeline** — accumulate wheel events, support high-resolution trackpads
  where terminal events expose them, coalesce storms, and apply acceleration/decay without moving
  the logical anchor unexpectedly.
- **(S) Jump-to navigation** — keys to jump to previous/next user turn, previous/next tool call,
  previous/next error.
- **(S) Jump marks** — set mark at current row, jump back, and expose recent jump history.
- **(S) Scroll anchoring choices** — preserve top visible row when scrolled up; preserve bottom when
  following; preserve search match when search is active.
- **(S) "Follow tail" indicator** — a subtle marker when scrolled up (not following), and a key
  to re-pin (already `End`); surface it visually.
- **(M) Smooth/animated scroll** — optional eased wheel scroll for large jumps, with reduced-motion
  support, instant-scroll config, and immediate cancellation on new input.
- **(S) Minimap/turn rail** — a compact rail with user turns, tool calls, errors, and current
  viewport, clickable when mouse capture is on.
- **(M) Horizontal navigation for wide blocks** — soft-wrap toggle, horizontal scroll inside code
  blocks, or Shift-wheel horizontal scroll. Avoid hiding long command output.

### 11.3 Rendering quality & performance
- **(M) Revision-keyed main render cache** — mirror `transcript_overlay_render_cache` for the main
  transcript line build; only rebuild changed entries. Profile first (idle cost is already zero).
- **(M) Incremental line cache per entry** — cache wrapped rows per `TranscriptEntry` keyed by
  `(revision, width)`; on resize only re-wrap, never re-render content.
- **(L) Row virtualization** — for very long sessions, build only row metadata globally and render
  styled spans near the viewport.
- **(M) Cache memory bounds** — LRU old widths/detail modes; expose debug counters so a huge session
  cannot grow memory without limit.
- **(M) Streaming coalescing** — batch fast deltas into frame-budgeted redraws while preserving
  perceived responsiveness.
- **(S) Synchronized-output everywhere** — confirm `?2026` brackets every frame in fullscreen;
  measure tearing on non-DEC-2026 terminals; consider a double-buffer diff fallback.
- **(M) Wide-glyph audit** — now that alt-screen owns the grid, re-verify moon/spinner/CJK widths
  against `unicode-width`; the `is_wide_rendered_glyph` hack may be removable or need extending.
- **(M) Grapheme-aware cursor/selection** — selection and cursor movement should respect grapheme
  clusters, combining marks, and ZWJ sequences.
- **(S) Layout budget assertions** — no widget may render outside its rect; no text overlaps status,
  composer, scrollbars, or modals at narrow widths.
- **(L) GPU/throughput stress** — `cat`-a-huge-file and fast tool spew: ensure the frame limiter +
  cache keep us at steady FPS without unbounded line growth in memory.
- **(M) Render metrics overlay** — hidden debug panel showing frame ms, emitted bytes, cache stats,
  row count, terminal dimensions, and active capability flags.

### 11.4 Persistence & history
- **(M) Richer exit mirror** — option to mirror **expanded** (full detail) vs collapsed; a config
  toggle; include a session header/footer (model, cost, duration) in the mirrored block.
- **(M) Export transcript** — `/export md|txt|html|json` writing the conversation to a file
  (reuse the line pipeline). HTML with preserved colors for sharing.
- **(S) Session resume visual** — when resuming, render the prior transcript into the alt-screen
  scrollback on first frame so the user sees context immediately (already in `app.transcript`).
- **(M) Optional inline-mirror-as-you-go via a side log** — since live native-scrollback mirroring
  is impossible, optionally tee the settled transcript to a file (`~/.squeezy/sessions/*.md`) in
  real time so external tooling/editors can tail it.
- **(M) Crash-resilient side log** — append settled transcript entries to a durable per-session
  Markdown/text file so a crash still leaves readable output outside the terminal.
- **(S) Exit mirror footer** — include session id, resume command, export path, and whether mirror
  is collapsed or expanded.
- **(M) Resume scroll restoration** — optionally restore last scroll/search/fold state for resumed
  sessions while defaulting to follow-tail for active work.

### 11.5 Mouse & input
- **(S) Mouse hover affordances** — hover to reveal disclosure carets; click to expand/collapse a
  specific entry (the user explicitly wants per-entry expand in the collapsed main view).
- **(M) Per-entry expand/collapse in main view** — a key + click to toggle one entry's
  `collapsed`/`settle` without opening Ctrl+T. Directly addresses the user's "expand something
  specific" note; further differentiates main vs Ctrl+T.
- **(M) Focused-row action model** — focused transcript row has primary/secondary actions:
  `Ctrl+O` expand/collapse, Enter inspect/open, `Ctrl+Enter` open detail, copy focused unit, and Esc
  clear focus/selection before closing the app.
- **(M) Inline transcript card toolbar** — hover/focus exposes expand, copy, pin, jump, and open
  detail targets without adding visual noise when idle.
- **(S) Click-to-copy affordances** — small copy icon/target on code blocks, tool output, and
  assistant entries when mouse capture is enabled.
- **(S) Click-to-jump paths** — file paths, diagnostics, and tool-generated locations register
  click targets; fallback command copies the path when opening is unavailable.
- **(S) Link handling** — OSC 8 hyperlinks for URLs/file paths; click-to-open where supported.
- **(S) Shift-wheel = horizontal** for very wide code/diff blocks (or soft-wrap toggle instead).
- **(M) Focus model** — keyboard focus rings for transcript, composer, modals, queue, status-line
  setup, and Ctrl+T; every mouse action must have a keyboard equivalent.
- **(S) Paste safety** — bracketed paste already exists; add visible paste preview/confirmation for
  very large pasted blocks.

### 11.6 Prompt queue direct manipulation
- **(M) Queue row model** — render queued prompts as stable rows with ids, focus, preview text,
  drag handle, delete target, and insertion positions.
- **(S) Click strip to expand/collapse** — preserve the current clickable queue strip but make it a
  normal disclosure target in the shared hit-test registry.
- **(M) Clickable delete `x`** — remove a queued prompt directly; keyboard equivalent remains
  Delete/Backspace.
- **(M) Drag/drop reorder** — drag handle starts reorder, live insertion marker shows target, drop
  commits, Esc cancels, resize during drag recomputes rects from row ids.
- **(S) Queue undo** — undo the last delete or reorder while the queue overlay is open.
- **(M) Multi-select queued prompts** — select several queued prompts, delete as a group, move as a
  group, or merge into the composer.
- **(S) Edit queued prompt** — Enter or `e` opens the selected queued prompt in the composer for
  editing; saving updates the queue item.
- **(S) Run selected next** — when idle, run selected queued prompt immediately; when busy, move it
  to the front.
- **(S) Queue status affordances** — show which prompt is next, paused, running soon, or blocked by
  an active turn.
- **(M) Queue tests** — hit-testing, reorder math, drag cancel/drop, deletion, undo, resize while
  dragging, keyboard parity, and no auto-drain surprises.

### 11.7 Ctrl+T overlay (the detail view)
- **(S) Expand-all / collapse-all** toggles in the overlay.
- **(S) Per-entry fold controls** + remember fold state across opens.
- **(M) Filter view** — show only errors / only tool calls / only a given tool, in the overlay.
- **(S) Overlay search** — same incremental find as §11.2, scoped to the expanded view.
- **(M) Diff/detail panes** — when an entry has a large diff, file excerpt, or tool output, open it
  in a right-side pane while preserving transcript context.
- **(S) Breadcrumbs** — show current turn/entry kind and match index while deep-reading.
- **(M) Pin entry** — pin one transcript entry/tool output while continuing to follow live output.

### 11.8 Robustness & platform
- **(M) Terminal capability probing** — detect DEC 2026, OSC 52, OSC 8, alternate-scroll, and
  truecolor at startup; adapt (e.g., disable sync-output bracket where unsupported to avoid a
  no-op cost). Cache results.
- **(M) Windows/ConPTY hardening** — explicit tests on Windows Terminal + legacy conhost; verify
  alt-screen enter/leave, mouse, and the mirror behave; the `#[cfg(windows)]` no-ops are already
  in place but untested for the fullscreen path.
- **(S) SSH/latency mode** — coalesce frames more aggressively over slow links; measure the
  alt-screen enter flicker and consider a one-frame delay to mask it.
- **(M) Graceful degradation** — if `terminal_size()` returns 0×0 or the terminal lacks alt-screen,
  fall back to a minimal line-printer mode rather than corrupting the screen.
- **(S) Panic/crash safety** — ensure a panic still runs the `Drop` teardown (leave alt-screen,
  show cursor) so a crash never leaves the user in a broken alt-screen. Add a panic hook.
- **(M) Signal-state machine** — explicit terminal states (`RawAlt`, `Suspended`, `Finishing`,
  `EmergencyRestored`) so teardown is idempotent and observable.
- **(M) Ctrl+Z support** — restore terminal before suspend; redraw from model on resume.
- **(M) Terminal bug database** — encode known quirks for VS Code/xterm.js, tmux, Windows Terminal,
  Apple Terminal, iTerm2, and SSH/OSC52 passthrough.
- **(S) Capability debug command** — `/terminal` shows detected terminal, dimensions, capabilities,
  mouse mode, clipboard providers, and sync-output status.

### 11.9 Architecture & tech debt
- **(L) Promote the simulation framework (§8) + CI gate** — first-class `crates/squeezy-tui/tools/`
  + `xtask`, required on a node-enabled runner.
- **(M) Split `lib.rs`** (~19k lines) — extract `terminal_guard.rs`, `render/`, `scroll.rs`,
  `clipboard.rs`, `mouse.rs`. The alt-screen refactor is a natural moment to start.
- **(M) Unify scroll state** — use a single `usize`-backed logical scroll model for main and Ctrl+T;
  convert to terminal coordinates only at render boundaries.
- **(S) Replace `TestBackend`-in-prod concerns** — the off-screen footer buffer used `TestBackend`;
  after Phase 8 most of that is gone, but audit any remaining test-type usage in the shipped path.
- **(M) Snapshot/golden tests** — adopt `insta` for fullscreen frame snapshots (plain + styled) to
  catch layout regressions the string-contains tests miss.
- **(M) Renderer state machine tests** — model enter/draw/resize/overlay/finish/emergency teardown
  transitions with property tests.
- **(M) Remove terminal-write side effects from render** — render should describe widgets; lifecycle
  owns escape sequences; copy/export owns text serialization.
- **(L) Transcript model extraction** — make transcript rows independently testable outside the TUI
  event loop.

### 11.10 UX polish (post-migration)
- **(S) Resize affordance** — on resize, briefly show the new dimensions (like tmux) as a toast.
- **(S) Empty-state / welcome** — a nicer first-frame welcome now that we own the full screen.
- **(S) Status bar density modes** — compact vs full; the full screen gives room for more context.
- **(M) Split panes** — a long-horizon idea: transcript + a pinned diff/file viewer side-by-side,
  now feasible since we own the grid.
- **(M) Command palette** — discover copy/search/jump/export/fold/status commands without memorized
  shortcuts.
- **(S) Keymap help overlay** — generated from the keymap resolver so docs and bindings do not
  drift.
- **(M) Notification center** — persistent, searchable list of toasts/status events instead of
  transient messages only.
- **(S) Theme audit** — verify colors in dark/light/low-contrast terminals; provide a high-contrast
  mode.
- **(M) Accessibility mode** — reduced animation, ASCII-safe glyphs, higher contrast, explicit text
  labels for icon-only controls, and no reliance on color alone.

### 11.11 Observability & bug reports
- **(M) TUI diagnostics bundle** — command to write terminal capabilities, recent frame metrics,
  last N input events, last N resize events, and sanitized grid snapshots.
- **(M) Repro script generation** — from a captured bad session, emit a term-matrix scenario that can
  be committed as a regression test.
- **(S) Runtime renderer assertions** — in debug builds, assert one composer, valid rectangles,
  cursor visible state, and no stale click targets.
- **(S) Frame capture hotkey** — write current styled/plain grid to a temp artifact for bug reports.
- **(M) User-visible degraded-mode notices** — if OSC52/DEC2026/mouse/focus are unsupported, show it
  once in `/terminal`, not as noisy startup text.

### 11.12 Security & privacy
- **(S) Clipboard confirmation threshold** — prompt before copying/exporting very large transcripts
  or content likely to contain secrets when a detector exists.
- **(M) Export location policy** — default exports under session storage or explicit user path;
  never surprise-write into the repo unless requested.
- **(S) Side-log opt-in/config** — if real-time side logs are enabled, make the path and retention
  visible.
- **(M) Redaction integration** — reusable formatting path so copy/export/mirror/bug-report bundles
  can share redaction when available.

### 11.13 Testing extras
- **(M) Fuzz row wrapping** — random Unicode, ANSI spans, narrow widths, folds, and selections.
- **(M) Golden copy fixtures** — verify plain/Markdown/JSON/HTML output from the same transcript.
- **(M) Input stress** — thousands of mouse wheel events, paste storms, resize storms, and streaming
  deltas under frame limiter.
- **(M) Windows CI leg** — ConPTY smoke tests for enter/leave, copy fallback, resize, and teardown.
- **(S) Terminal fixture minimizer** — shrink failing ANSI logs/scenarios to small repros.

---

## 12. Future Improvements — Perfection Backlog

These are not required to prove the alt-screen migration. They are the long-horizon features that
make Squeezy feel like a polished terminal-native workbench rather than a chat box.

### 12.1 Interaction polish
- **(M) Universal command palette** — fuzzy-search every command, copy action, jump action, queue
  action, layout action, and setting. Show current keybinding and let Enter run it.
- **(M) Contextual action palette** — focused transcript entry exposes only relevant actions:
  expand, copy block, copy output, open file, pin, quote, retry, inspect raw event, export entry.
- **(M) Mouse hover intent** — delay/reveal controls based on stable hover, not every mouse move, so
  card action buttons do not flicker during scroll.
- **(S) Clickable breadcrumbs** — current session/turn/tool/search-match breadcrumbs are clickable
  jump targets.
- **(M) Multi-cursor-like transcript selection** — select multiple non-contiguous entries or blocks
  for copy/export.
- **(S) Inline rename labels** — let the user label important turns, pinned outputs, or queue items.
- **(S) Gentle first-run interaction hints** — small, dismissible hints only after the user appears
  stuck; never permanent instructional chrome.

### 12.2 Reading and comprehension
- **(M) Semantic turn outline** — side rail or overlay summarizing user prompts, assistant answers,
  tool calls, errors, plans, and decisions.
- **(M) Collapsible reasoning/tool lanes** — separate assistant prose, reasoning summaries, tool
  input/output, and system notices into independently foldable lanes.
- **(M) Pinned compare view** — pin an old response/tool output beside the live transcript to compare
  before/after edits, diffs, or reruns.
- **(S) Reading position bookmarks** — save named bookmarks in long sessions and jump back later.
- **(S) Entry annotations** — lightweight notes attached to a turn or tool output, persisted with the
  session.
- **(M) Session timeline** — chronological timeline of prompts, tool calls, approvals, edits,
  checkpoints, costs, and errors.
- **(S) "What changed since here?"** — from a selected turn, summarize later edits/tool results and
  jump to relevant entries.

### 12.3 Workflow acceleration
- **(M) Actionable tool outputs** — failed command rows offer retry, copy command, edit command,
  open log, jump to file, or create follow-up prompt.
- **(M) Prompt snippets from selection** — select transcript text and turn it into a queued prompt,
  macro, bug report note, or plan refinement.
- **(S) Scratchpad pane** — temporary notes/composition area that can quote transcript selections
  and survives across turns.
- **(M) Queue groups** — group queued prompts, reorder groups, pause/resume groups, and mark a group
  as "run after current turn."
- **(M) Conditional queue items** — queued prompt runs only if prior turn succeeds, fails, edits, or
  produces a matching status.
- **(S) Prompt templates as queue cards** — queue item can be a template with editable slots.
- **(M) Replayable interaction macros** — record a sequence of UI commands (search, select, copy,
  queue, export) and replay it in the same session.

### 12.4 Layout intelligence
- **(M) Adaptive density** — automatically switch compact/default/expanded layout based on terminal
  height and active workflow, while preserving explicit user preference.
- **(M) Smart split panes** — use a side pane only when width permits; otherwise fall back to stacked
  modal/detail surfaces.
- **(S) Focus-preserving resize** — every pane keeps its logical focus/selection when the layout
  switches between split and stacked.
- **(M) Dockable panels** — task, queue, diff, transcript outline, diagnostics, and clipboard/export
  panels can be pinned left/right/bottom.
- **(S) Zen mode** — hide secondary panels and status detail while keeping copy/search/queue actions
  available.
- **(S) Presentation mode** — larger text, reduced chrome, no secrets/status cost by default, useful
  for demos or screen sharing.

### 12.5 Transcript intelligence
- **(M) Local transcript index** — incremental index for fast search, filters, jump-to-symbol/path,
  and "show me all errors/tool calls/files mentioned."
- **(M) Semantic filters** — filter transcript by file path, command, tool, subagent, status,
  language, branch, or error class.
- **(M) Related-entry links** — connect a tool result to the prompt that caused it, the edit it
  produced, the error it fixed, and the follow-up response.
- **(S) Duplicate-output folding** — collapse repeated logs/progress lines with count and expand
  affordance.
- **(M) Code-aware copy/export** — preserve fenced languages, file path headers, and diff metadata.
- **(M) Error lenses** — failed command/tool output highlights the exact error lines and offers
  next/previous error navigation.
- **(S) Transcript health markers** — show stale context, hidden omissions, truncated external tool
  output, or unresolved approvals explicitly.

### 12.6 Clipboard, paste, and external handoff
- **(M) Clipboard history inside Squeezy** — recent Squeezy copy payloads can be re-copied, exported,
  or quoted into the composer.
- **(S) Paste transform menu** — paste as plain text, quoted text, code block, file attachment, or
  queued prompt.
- **(M) Large paste staging** — big pasted content opens a preview with size, line count, and actions
  before committing to the composer.
- **(M) Export destinations** — export transcript/selection to Markdown, JSON, HTML, clipboard,
  temp file, repo file, or bug-report bundle through one flow.
- **(S) External editor handoff** — open selected prompt/queue item/export buffer in `$EDITOR`, then
  re-import on save.
- **(S) Shareable session bundle** — zip transcript export, diagnostics, environment summary, and
  selected artifacts for handoff.

### 12.7 Personalization
- **(M) Keybinding editor UI** — edit keybindings in-app with conflict detection and live preview.
- **(M) Theme editor UI** — adjust palette, contrast, rail styles, fold indicators, and status-line
  colors with immediate preview.
- **(S) Per-terminal profiles** — different defaults for VS Code, tmux, SSH, Windows Terminal, and
  local iTerm2.
- **(S) Per-workspace UI profile** — remember density, panels, queue behavior, and transcript detail
  preferences per repo.
- **(M) Gesture settings** — configure double-click, Shift-click, drag thresholds, smooth scroll,
  trackpad sensitivity, and hover reveal delay.
- **(S) Minimal glyph mode** — ASCII-safe rails/icons for fonts or terminals with poor glyph
  rendering.

### 12.8 Collaboration and multi-agent visibility
- **(M) Subagent timeline panel** — show subagent status, latest message, tool calls, cost, and
  blocked/completed state in a dedicated navigable panel.
- **(M) Compare subagent outputs** — split or tabbed view for several subagent findings, with copy
  and quote actions.
- **(S) Promote subagent result to prompt** — turn a subagent finding into queued follow-up work.
- **(M) Live review board** — when many workers are active, show lanes for queued/running/reviewing/
  blocked/completed items.
- **(S) Attention routing** — only surface subagent events that need the user, while keeping quiet
  progress in the timeline.

### 12.9 Reliability and self-healing
- **(M) Stuck-render watchdog** — detect if frames stop changing despite state changes; capture a
  diagnostic bundle and force a clean redraw.
- **(M) Terminal restore command** — a standalone `squeezy terminal-reset` command that restores
  cursor, raw mode symptoms, mouse modes, bracketed paste, and title after a crash.
- **(S) Last-known-good layout fallback** — if a render path panics in debug/dogfood builds, fall
  back to a minimal transcript/composer frame and capture the failing layout input.
- **(M) Automatic degraded-mode suggestions** — if terminal capability probing detects broken OSC52,
  mouse, glyph width, or sync output, suggest the exact setting to change.
- **(M) Session auto-save checkpoints for UI state** — persist scroll position, search, selection,
  queue ordering, folds, and pinned panes often enough to recover after crash.

### 12.10 Measurement and quality gates
- **(M) UX latency budgets** — define target latencies for keypress echo, scroll response, paste
  preview, copy command, search jump, queue drag, and resize redraw.
- **(M) Real terminal benchmark suite** — scripted PTY/tmux/xterm.js benchmarks measuring frame
  latency, bytes emitted, scroll smoothness, and teardown correctness.
- **(M) Dogfood telemetry counters** — local-only or opt-in counters for frame time percentiles,
  copy fallback usage, resize storms, terminal capabilities, and emergency teardown events.
- **(S) Visual diff dashboard** — compare screenshots/snapshots across terminal sizes, themes, and
  emulator backends.
- **(M) Accessibility quality gate** — snapshot contrast ratios, no-color meanings, reduced-motion
  mode, and keyboard-only reachability before shipping major UI changes.

---

## 13. Appendix

### 13.1 The full options menu (historical, for context)
Earlier analysis enumerated F0 (scrolling-regions flag), S1 (append-only print — **disproven**),
S2 (codex DECSTBM — still buggy in xterm.js), S3 (hybrid cell-diff footer — same reflow flaw),
A (VS Code → alt-screen), D (alt-screen always), and library swaps (termina/termwiz/r3bl/
notcurses). Conclusion after empirical work: **the entire footer-in-normal-buffer family (F0/S1/
S2/S3) shares the cursor-drift-on-reflow flaw; only the alt-screen options (A/D) are immune.**
This plan is option **D** (alt-screen for all terminals), with a temporary migration/repro switch
only. It restores the practical parts of native scrollback/selection through in-app scroll, search,
copy, export, side logs, and exit mirroring.

### 13.2 Glossary
- **Alt-screen** — the terminal's alternate screen buffer (DEC 1049); no scrollback, app-owned grid.
- **Reflow** — terminal re-wrapping buffer lines on resize; the source of the cursor drift.
- **DEC 2026** — synchronized output; brackets a frame so the terminal commits it atomically.
- **OSC 52** — escape sequence to write the system clipboard.
- **DECSTBM** — set top/bottom scroll margins (codex's S2 mechanism).

### 13.3 Resolved decisions
- `/clear` should reset the model and leave a clean app surface; confirmation belongs in status/toast
  if the desired contract is "start from scratch."
- Exit mirror defaults to collapsed because it matches the main view; expanded mirror is an optional
  config/export mode.
- Scroll state should be widened to `usize`; terminal coordinates remain `u16` at render boundaries.
- Ctrl+T remains as a detail/read policy over the shared transcript surface, not a separate renderer.

### 13.4 Open design details
- Exact default keybindings for visual selection, copy-current-block, copy-last-answer, search, and
  jump navigation.
- Whether side logs are default-on, opt-in, or tied to session persistence settings.
- Clipboard-provider precedence per platform and whether large OSC 52 writes require confirmation by
  default.
- How much terminal capability probing should be active vs configured, especially over SSH/tmux.
- Whether main-view split panes ship before or after the core copy/search work.
