# TUI Alt-Screen Future Improvement Specs

Expanded specifications for every item in `TUI_ALT_SCREEN_RENDERER_PLAN.md` section 12,
based on the subagent passes requested during planning.

Each item uses the same shape:

- **Spec:** user-facing behavior and product contract.
- **Steps:** implementation steps and likely model/API surfaces.
- **Verify:** tests, manual checks, or quality gates.
- **Deps/Risks:** dependencies, ordering constraints, and failure modes.
- **Platform notes:** OS, terminal-family, or shell-specific implementation/testing requirements
  where the item is not platform-neutral.

---

## 12.1 Interaction Polish

### Universal Command Palette

- **Spec:** One discoverable modal for every app command: copy, search, jump, export, fold, queue,
  layout, status, settings, diagnostics, and terminal actions. It opens over the fullscreen surface,
  filters with fuzzy search, shows command labels/descriptions/current bindings/disabled reasons,
  and runs the highlighted command with Enter. Commands that need parameters use a second palette
  step instead of spawning a separate UI.
- **Steps:** Add a typed `CommandRegistry` with stable `CommandId`, category, default binding,
  availability predicate, and executor. Route keyboard shortcuts through `CommandId` wherever
  possible. Build `ActionContext` from focused surface, transcript row id, selection, search, queue,
  composer state, terminal capabilities, and clipboard provider state. Render the palette through
  the normal fullscreen `render()` path.
- **Verify:** Unit-test command availability by context, fuzzy ordering, disabled reasons, and
  executor routing. Snapshot palette layouts at narrow/normal/wide sizes. Integration-test that
  keybinding and palette invocation call the same command. Verify opening/closing preserves scroll,
  selection, search, follow-tail, and queue order.
- **Deps/Risks:** Depends on command routing, focus model, transcript row ids, clipboard actions, and
  queue actions. Risk is duplicated shortcut/palette behavior; mitigate by making shortcuts dispatch
  command ids.

### Contextual Action Palette

- **Spec:** A compact action menu for the focused semantic unit: transcript entry, code block, tool
  output, file path, queue item, search match, or breadcrumb. Actions include expand/collapse, copy,
  export, quote, retry, inspect raw event, open file, move/delete queue item, and jump to related
  turn.
- **Steps:** Extend `ActionContext` with `FocusedUnit`. Add `ContextActionProvider` implementations
  for transcript rows, code/tool blocks, queue rows, search state, and breadcrumbs. Register click
  targets through the hit-test registry; keyboard and mouse invoke the same actions. Use copy/export
  formatters and transcript metadata, never rendered terminal cells.
- **Verify:** Unit-test action lists for assistant prose, code blocks, failed commands, file paths,
  queue items, and search matches. Snapshot palette placement near top/middle/bottom and narrow
  terminals. Verify every contextual mouse action has keyboard parity.
- **Deps/Risks:** Depends on focused-row state, hit-testing, transcript metadata, queue row model,
  copy/export formatter, and file target extraction. Risk is accidental mutation from retry/quote;
  require explicit command payloads and status feedback.
- **Platform notes:** File/path actions must understand Unix paths, Windows drive/UNC paths, and
  paths with spaces. External open actions should use a platform abstraction: macOS `open`, Linux
  `xdg-open`/desktop portals when available, and Windows shell open, all optional and never required
  for core TUI behavior.

### Mouse Hover Intent

- **Spec:** Hover controls reveal only after stable intent, not on every mouse move. Transcript
  cards, code/tool blocks, breadcrumbs, queue rows, and scrollbars can reveal compact controls after
  a short delay. Wheel scroll, drag, selection, focus movement, and disabled mouse capture suppress
  hover reveal. The default hover state is visual, not structural: text becomes slightly brighter,
  the target may gain a restrained underline or weight change, and controls appear only when they
  add obvious value.
- **Steps:** Add `HoverIntentState` with target id, first-seen time, last movement, revealed target,
  suppression reason, and pointer cell. Map mouse coordinates to stable semantic ids via hit-test
  registry. Schedule redraw ticks only while reveal is pending. Render controls without changing row
  heights.
- **Verify:** Unit-test delay, reveal, leave, scroll suppression, drag suppression, keyboard-focus
  reveal, resize while hovering, no redraw loop after settling, and no row-height/width changes from
  hover state. Snapshot rows with/without hover controls.
- **Deps/Risks:** Depends on stable semantic ids, hit-test registry, frame ticks, focus model, and
  mouse capture. Risk is flicker or content overlap; use fixed-size targets and palette fallback on
  narrow terminals.
- **Platform notes:** Treat mouse quality as terminal-family-specific: VS Code/xterm.js, iTerm2,
  tmux/SSH, Windows Terminal/ConPTY, and Linux virtual consoles can report different move/wheel
  granularity. Hover reveal must degrade to keyboard focus when mouse reporting is absent or lossy.

### Hover Preview And Double-Click Activation

- **Spec:** Every interactive row or item has the same pointer contract: hover/mouse-move gives a
  subtle noncommittal preview, single click selects or focuses, and double-click performs the
  natural primary action. Transcript entries expand/collapse or open detail, tool outputs open raw
  output/detail, code blocks open copy/actions, queue cards expand/edit, breadcrumbs jump, and panel
  rows open their detail target. The preview should be quiet: stronger foreground, light silver
  emphasis, optional underline, or bold where supported, without changing layout height or stealing
  keyboard focus. When the terminal or embedding host supports pointer-shape/cursor affordances,
  clickable targets may request a hand/link-style pointer; unsupported terminals rely on the cell
  styling alone.
- **Steps:** Add a `PointerActivationPolicy` per `HitTargetKind` with `hover_preview`, `select`,
  `primary_activate`, and optional `secondary_activate`. Track click count/time/cell and semantic
  target id in one gesture recognizer. Render hover preview from semantic target state, not from raw
  terminal cells. Route double-click to the same command/action registry used by Enter or the
  contextual action palette. Add semantic style tokens such as `interactive.hover`,
  `interactive.hover_text`, `interactive.active`, `interactive.clickable`, and
  `interactive.pointer_hint`. Keep keyboard parity through Enter/Space and explicit expand/jump
  commands.
- **Verify:** Unit-test single-click vs double-click timing, target changes between clicks, hover
  preview suppression during scroll/drag/selection, and keyboard parity. Snapshot preview styling in
  normal, focused, selected, disabled, no-color, high-contrast, and narrow layouts. Assert hover
  styling never changes row geometry and never hides text. Test that double-click never triggers a
  destructive action directly; delete/retry/export still require explicit action commands.
- **Deps/Risks:** Depends on hit-testing, command registry, focus model, hover intent, stable ids,
  and gesture settings. Terminal mouse reports are coarse and sometimes compressed, so double-click
  thresholds must be configurable and fall back cleanly to keyboard/context-menu activation.
- **Platform notes:** Double-click timing should be app-configured, not borrowed blindly from an OS
  GUI setting that terminal apps usually cannot read. Validate separately in macOS terminals with
  high-resolution trackpads, Linux tmux/SSH sessions, and Windows Terminal/ConPTY. Mouse-pointer
  shape changes are not portable across terminals; implement them only through a capability-gated
  adapter and keep brightness/underline/weight as the guaranteed affordance.

### Clickable Breadcrumbs

- **Spec:** Breadcrumbs for session, turn, tool, file, and search context become clickable and
  keyboard-focusable jump/action targets. They orient long sessions without permanent instructional
  chrome.
- **Steps:** Build `BreadcrumbModel` from transcript focus, search state, session metadata, queue,
  and detail context. Give each segment a `BreadcrumbId` and command/action. Render in header/status
  areas with middle truncation. Register rects in hit-test registry and support keyboard traversal.
- **Verify:** Unit-test breadcrumb construction for tail-following, scrolled transcript, Ctrl+T,
  active search, queue focus, and empty sessions. Test hit targets and jump behavior. Snapshot
  narrow truncation and wide full breadcrumbs.
- **Deps/Risks:** Depends on focus model, transcript row ids, search state, session metadata, and jump
  commands. Risk is stale breadcrumbs after resize; derive from model each frame.

### Multi-Cursor-Like Transcript Selection

- **Spec:** Users can select multiple non-contiguous entries, blocks, or ranges for copy/export/quote.
  Selection remains app-owned and logical across resize, folding, filtering, and Ctrl+T/main view
  switches.
- **Steps:** Replace single-range storage with `SelectionSet` keyed by entry ids, row ids, and copy
  ranges. Support visual row ranges, semantic entries, code blocks, tool outputs, queue item text,
  and search match groups. Normalize overlapping selections while preserving block boundaries. Extend
  copy/export formatters to accept `SelectionSet`.
- **Verify:** Unit-test add/remove/merge/order/clear. Golden-test copy/export for mixed non-contiguous
  selections. Test resize, fold/unfold, filter changes, and Ctrl+T round-trip. Snapshot highlight
  rendering with active search and focus.
- **Deps/Risks:** Depends on transcript copy ranges, stable row ids, selection renderer, clipboard
  provider chain, and focus model. Risk is confusing hidden folded selections; folded entries need a
  selected marker.

### Inline Rename Labels

- **Spec:** Users can label important turns, pinned outputs, queue items, bookmarks, or search marks.
  Labels are UI metadata only and never alter transcript/model-provider history unless explicitly
  quoted/copied.
- **Steps:** Add `LabelTargetId` for turn, transcript entry, pinned output, queue item, bookmark, and
  jump mark. Store labels in session UI metadata. Add `InlineEditState` and reuse composer editing
  primitives. Render inline editor in place or as a small modal. Surface labels in breadcrumbs,
  rails, search, queue, and exports where configured.
- **Verify:** Unit-test create/edit/cancel/clear/persist/resume. Snapshot inline editor near edges and
  narrow widths. Verify labels do not enter model-visible transcript or default copy output.
- **Deps/Risks:** Depends on session UI metadata, focus model, queue ids, text-edit primitives, and
  persistence. Risk is schema churn; keep metadata optional and backward-compatible.

### Gentle First-Run Interaction Hints

- **Spec:** Dismissible hints appear only when behavior suggests the user is stuck, never as permanent
  chrome or transcript entries. Examples: repeated scroll no-ops, first selection without copy,
  repeated queue open, first hover reveal.
- **Steps:** Add `HintEngine` observing interaction events with `HintId`, trigger, cooldown, max
  displays, priority, text, optional action, and dismissal state. Render through toast/status UI and
  suppress during modals, approvals, command palette, search input, paste preview, and inline edits.
- **Verify:** Unit-test triggers, cooldowns, display counts, dismissals, modal suppression, disabled
  config, and no idle redraw loop. Snapshot placement with composer, queue strip, status, search, and
  approvals.
- **Deps/Risks:** Depends on interaction event telemetry, toast/status UI, focus/modal state, optional
  settings persistence, and command registry. Risk is noisy/patronizing hints; use delayed triggers
  and strict max-display counts.

---

## 12.2 Reading And Comprehension

### Semantic Turn Outline

- **Spec:** A structural map of long sessions: user prompts, assistant answers, tool calls, errors,
  plans, approvals, decisions, checkpoints, and queue actions. Wide terminals use a rail; narrow
  terminals use an overlay. Selecting a node jumps to the logical transcript row.
- **Steps:** Add `OutlineIndex` from `TranscriptSurface` with `outline_id`, `turn_id`, `entry_id`,
  `row_id`, kind, title, status, and children. Generate deterministic local titles from first lines,
  tool names/status, error first lines, or plan step text. Incrementally rebuild on entry/revision/
  fold/filter changes.
- **Verify:** Unit-test outline extraction, click-to-jump, huge/empty/streaming/title-less sessions.
  Snapshot narrow overlay, normal layout, and wide rail. Verify jumps survive resize and fold changes.
- **Deps/Risks:** Depends on stable ids and row projection. Risk is noisy weak titles; group nodes and
  prefer honest deterministic labels over fake summaries.

### Collapsible Reasoning/Tool Lanes

- **Spec:** Dense turns split into foldable lanes: assistant text, reasoning summary, tool input,
  tool output, system notice, approval, error, and plan. Main view is concise; Ctrl+T/detail can be
  expanded.
- **Steps:** Introduce `TranscriptLane` and `LaneId` before row wrapping. Store fold state by
  `(entry_id, lane_id)`. Project lanes through `TranscriptSurface`. Add lane click targets for
  disclosure, copy, pin, and open detail. Copy/export supports visible-only, current lane, whole
  entry, or raw event.
- **Verify:** Row-model tests for folded/expanded lanes. Snapshot assistant/tool/error turns in main
  and Ctrl+T. Test search include/exclude collapsed lanes, copy behavior, and resize fold stability.
- **Deps/Risks:** Depends on structured transcript entries. Risk is hiding failures; errored lanes
  keep visible headers.

### Pinned Compare View

- **Spec:** Pin an old response, tool output, diff, error, or rerun result beside the live transcript.
  Pinned panes have independent scroll/search/copy and can compare old/new content.
- **Steps:** Add `PinnedViewState` with entry id, row anchor, detail policy, scroll offset, mode, and
  optional compare target. Use layout thresholds for split vs overlay/tab. Render pinned content via
  `TranscriptSurface`; route focus so scroll/search/copy target the active pane. Start with line-based
  clean-text diff.
- **Verify:** Snapshot wide split, narrow overlay, and tiny fallback. Test pin/unpin preserves main
  scroll, independent scroll offsets, active-pane copy, and huge pinned output while streaming.
- **Deps/Risks:** Depends on shared row projection, focus model, copy formatter, and layout solver.
  Risk is expensive large diffs; add size limits and lazy diffing.

### Reading Position Bookmarks

- **Spec:** Mark and return to important places across resize, folds, filters, and resume. Bookmarks
  use semantic anchors, not terminal rows.
- **Steps:** Add `Bookmark` with id, name, entry id, optional text anchor/copy range, created time,
  scroll bias, and note. Store in session UI metadata. Resolve through `TranscriptSurface`, falling
  back to entry start if exact range is gone. Add create/rename/delete/list/next/previous commands.
- **Verify:** Test anchor resolution across widths, resume persistence, folded/filtered/compacted/
  cleared/missing entries, and outline/timeline markers.
- **Deps/Risks:** Depends on persisted UI metadata. Risk is stale anchors; show unresolved bookmarks
  rather than jumping incorrectly.

### Entry Annotations

- **Spec:** Attach notes to turns, tool outputs, errors, or selected ranges without polluting model
  context. Badges show annotation presence; full notes open on focus/action.
- **Steps:** Add `Annotation` with id, entry id, optional range, text, tags, created/updated times.
  Store in session UI metadata. Add CRUD commands and contextual actions. Render badges via row
  metadata. Export/search can include or omit annotations.
- **Verify:** Unit-test CRUD, persistence, and attachment. Snapshot badges/editor modal. Test search
  and Markdown/JSON export include/omit modes.
- **Deps/Risks:** Depends on session UI metadata and modal input. Risk is annotations entering model
  context; keep serialization paths separate.

### Session Timeline

- **Spec:** Chronological event view of prompts, turns, tools, approvals, edits, checkpoints, costs,
  errors, queue actions, and state changes. Events are grouped by turn/time and jump back to rows.
- **Steps:** Add `TimelineEvent` with id, sequence, timestamp, kind, status, turn id, optional entry
  id, and metadata. Build from structured session events plus transcript entries. Render as rail/list
  with text labels and filters.
- **Verify:** Unit-test event extraction/order, jump links, streaming status transitions, missing
  timestamps/costs, and filtered timeline snapshots.
- **Deps/Risks:** Depends on structured session/event data. Risk is noise; start with high-signal
  events and filters.

### What Changed Since Here?

- **Spec:** From a selected turn, show observed later changes grouped by files, commands/tests,
  errors, checkpoints, decisions, approvals, and tool results. Use honest "observed since this turn"
  language.
- **Steps:** Use selected entry sequence as anchor. Scan later timeline events/transcript metadata.
  Build `ChangeSummary` groups and link items to transcript/timeline/checkpoint/diff data. Cache by
  anchor sequence and invalidate on new events.
- **Verify:** Unit-test each group, empty results, anchors near session end, compacted/missing
  metadata, filtered views, snapshot panel, and jump links.
- **Deps/Risks:** Depends on timeline/session events and checkpoint metadata. Risk is overstating
  completeness; avoid claiming full project history.

---

## 12.3 Workflow Acceleration

### Actionable Tool Outputs

- **Spec:** Tool result rows expose next actions: retry, edit command, copy command/output, open log,
  jump to file/location, or create follow-up prompt. Retry never bypasses sandbox or approval.
- **Steps:** Add `WorkflowAction` variants and preserve structured tool metadata: command, cwd, exit
  status, stdout/stderr slices, approval/sandbox context, artifact path. Extend transcript rows with
  action refs and hit targets. Add path/location parsers for common compiler/test/grep formats. Route
  all actions through `UiCommand`.
- **Verify:** Test action availability for success/failure/timeout/cancel/approval denial. Test retry
  preserves cwd/command/env/sandbox metadata. Test Unix/Windows/path-with-spaces parsing and focused
  row snapshots.
- **Deps/Risks:** Requires structured tool metadata. Retry is safety-sensitive. Path parsing can false
  positive; degrade to copy/open-output when uncertain.
- **Platform notes:** Path/location parsing must cover Unix absolute/relative paths, macOS
  case-folded filesystems, Windows drive letters, UNC paths, backslashes, CRLF line numbers, and
  tool output that quotes paths differently under PowerShell, cmd.exe, Bash, zsh, and common Linux
  shells.

### Prompt Snippets From Selection

- **Spec:** Selected transcript text can become a composer quote, queued prompt, scratchpad note, bug
  note, plan refinement, or macro input. Snippets retain internal provenance while visible text stays
  concise.
- **Steps:** Reuse selection and copy formatter. Add `SnippetSource` with entry ids, row ids, text
  ranges, formatter, and provenance. Add builders for quote, queue item, scratchpad insert, bug note,
  and plan refinement. Add size warnings for large snippets.
- **Verify:** Golden-test wrapped lines, code fences, tool output, wide glyphs, and multi-entry
  selections. Verify resize stability and disabled actions for empty selections.
- **Deps/Risks:** Depends on selection and clean copy/export formatting. Large snippets can bloat
  prompts; preview and warn.

### Scratchpad Pane

- **Spec:** A session-scoped notes/composition pane for observations, quotes, draft prompts, and
  temporary task structure. It never enters model context unless explicitly inserted.
- **Steps:** Add scratchpad model with text buffer, cursor, selection, dirty flag, and source links.
  Persist in session UI metadata. Reuse composer editing primitives. Add append quote, insert source
  link, send to composer, queue as prompt, export, and clear actions. Render as side/bottom/overlay
  based on layout.
- **Verify:** Model tests for editing/source links/persistence/clear. Layout tests at narrow/medium/
  wide sizes. Resume tests prove scratchpad survives but is excluded from prompts.
- **Deps/Risks:** Depends on focus model, pane layout, and persistence. Risk is confusing notes with
  conversation history; keep storage separate.

### Queue Groups

- **Spec:** Prompt queue becomes deliberate batches: named groups, reorder, pause/resume, dissolve,
  run after current turn. Queue strip shows next/paused/blocked groups and item counts.
- **Steps:** Replace flat queue with `PromptQueue { groups: Vec<QueueGroup> }`, stable group/item
  ids, and `QueueGroupState`. Extend queue row model with headers, nested rows, insertion markers,
  and hit targets. Update drain policy to select next runnable item from next runnable group.
- **Verify:** Queue model tests for grouping, reorder, pause/resume, dissolve, run-after-current.
  Render tests for collapsed/expanded groups. Drag/drop tests across groups. Execution tests confirm
  paused/blocked groups do not auto-run.
- **Deps/Risks:** Depends on queue row model and direct manipulation. More state can surprise users;
  labels must make execution policy obvious.

### Conditional Queue Items

- **Spec:** Queue items run only when explicit conditions are satisfied: previous turn succeeded/
  failed, files edited/no edits, output matches, tool failed, approval denied, or manual-only.
- **Steps:** Add `QueueCondition`, `TurnOutcome`, and evaluation result states: runnable, blocked,
  satisfied, skipped, stale. Add bounded substring/regex matching and a condition editor. Evaluate
  conditions during queue drain, never render.
- **Verify:** Unit-test every condition against synthetic outcomes, drain behavior, manual override,
  bounded patterns, and rendering for runnable/blocked/skipped/stale.
- **Deps/Risks:** Requires reliable turn outcome metadata. Hidden automation is risky; execution must
  be visible, cancellable, and approval-preserving.

### Prompt Templates As Queue Cards

- **Spec:** Queue cards can be templates with editable slots, e.g. `Review {file}`. Missing/invalid
  slots block execution with inline status.
- **Steps:** Add `QueueItemKind::TemplateCard`, `PromptTemplate`, slot types, deterministic slot
  resolver, slot focus/edit commands, reusable template storage, and command-palette actions for
  create/insert/save.
- **Verify:** Golden-test template rendering, slot validation, runtime resolution, focus movement,
  persistence, and instantiated queued cards.
- **Deps/Risks:** Depends on queue editing/focus. Avoid a large templating engine; use a small
  deterministic resolver.

### Replayable Interaction Macros

- **Spec:** Record and replay UI workflows such as search, select, copy, queue, export, fold, and
  navigation. Macros record logical commands, not terminal coordinates, and never bypass approvals.
- **Steps:** Add canonical `UiCommand` layer. Macro recorder captures committed commands and logical
  targets while ignoring hover/mouse move/ticks/resize/toasts. Add `InteractionMacro`, replay
  validation, visible progress, cancel, and stale-target handling.
- **Verify:** Test command serialization, replay for search/select/copy/queue/export/fold, stale
  targets, and that replay uses same dispatcher as keyboard/mouse. Verify noise events are ignored.
- **Deps/Risks:** Depends on command/action abstraction and stable ids. Automation can surprise users;
  replay must be visible and cancellable.

---

## 12.4 Layout Intelligence

### Adaptive Density

- **Spec:** Auto-select compact/default/expanded density from terminal size and workflow while
  honoring explicit user override. Density changes preserve scroll anchor, selection, focus, queue,
  and composer state.
- **Steps:** Add `DensityMode` and computed `ResolvedDensity`. Centralize margins, borders, panel
  heights, transcript padding, and status detail level. Renderers consume `ResolvedDensity` rather
  than scattered width/height checks.
- **Verify:** Golden-test small/medium/large terminals, explicit override, resize compact/expanded
  transitions, and mouse hit targets per density.
- **Deps/Risks:** Depends on layout constants and stable row ids. Risk is jumpy resize; recompute only
  on real resize/workflow changes and preserve anchors.

### Smart Split Panes

- **Spec:** Use side panes only when width permits; otherwise use stacked panels, modal/detail
  surfaces, or inline cards. Impossible layouts degrade gracefully.
- **Steps:** Define `PaneKind`, `LayoutSolver`, placement enum, min pane sizes, and frame-local pane
  rect registry. Use ratatui `Layout` after the abstract solver chooses placement.
- **Verify:** Solver tests across thresholds, golden wide/medium/narrow layouts, open diff/queue/
  outline from transcript focus, and hit-test after split-to-stack transitions.
- **Deps/Risks:** Depends on pane/focus registry. Risk is special-case sprawl; keep one policy table.

### Focus-Preserving Resize

- **Spec:** Logical focus, selection, scroll, drag, and active action survive layout changes between
  split, stacked, modal, compact, and expanded.
- **Steps:** Represent focus as logical ids (`TranscriptEntry`, `QueueItem`, `Pane`, `Composer`).
  Store scroll anchors as row/entry ids plus offset. Run `resolve_focus_after_layout` after resize.
  Recompute rects from ids and assert no hidden focus without fallback.
- **Verify:** Resize while selecting, queue-focused, dragging queue, side diff pane open, following
  tail, and scrolled up.
- **Deps/Risks:** Requires stable row/pane ids. Risk is invisible focus; use a single resolver and
  render-time validation.

### Dockable Panels

- **Spec:** Task, queue, diff, outline, diagnostics, clipboard/export panels can be pinned left,
  right, or bottom, with tabbed containers when multiple panels share a dock.
- **Steps:** Add `PanelState`, extend layout solver for pinned panels, enforce transcript/composer
  minimums, add panel commands, and persist user panel preferences.
- **Verify:** Solver tests for one/multiple/impossible panels. Golden left/right/bottom docks. Focus
  traversal and mouse tab/close tests.
- **Deps/Risks:** Needs command palette/layout actions. Risk is cramped chrome; collapse to modal/
  stacked when minimums fail.

### Zen Mode

- **Spec:** Low-noise mode focused on transcript and composer. Secondary panels and detailed status
  hide, but search/copy/queue/error/help remain reachable.
- **Steps:** Add `UiMode::Zen`. Layout solver suppresses nonessential panels. Status renderer uses
  minimal one-line state. Queue becomes strip/count. Store previous panel visibility for restoration.
- **Verify:** Golden hidden chrome render, toggle restores panels, search/copy/queue work, and resize
  while entering/leaving zen.
- **Deps/Risks:** Must keep blocking errors/approvals visible. Zen is layout policy, not a renderer.

### Presentation Mode

- **Spec:** Screen-share mode with spacious cards, reduced metadata, stronger contrast, and default
  hiding of cost/account/provider/secret-like values/full paths where configured.
- **Steps:** Add `UiMode::Presentation`, display policy separate from transcript persistence, metadata
  suppression flags, active mode indicator, and one-shot reveal command.
- **Verify:** Golden renders with hidden metadata, copy/export explicit policy tests, internal data
  persists and reappears after mode exit.
- **Deps/Risks:** Display hiding is not redaction. Keep reveal command and avoid data mutation.

---

## 12.5 Transcript Intelligence

### Local Transcript Index

- **Spec:** Incremental local index over transcript model for search, path lookup, command lookup,
  tool calls, errors, health markers, and related-entry discovery.
- **Steps:** Add `TranscriptIndex` keyed by entry id and revision. Index normalized/copy/ANSI-stripped
  text plus paths, commands, tools, statuses, languages, error classes, health markers, subagents,
  and code metadata. Update on append, stream settle, revision, clear, compaction, fold changes, and
  resume.
- **Verify:** Test append/update/delete/clear/resume/stream consolidation, Unicode/wide glyphs, ANSI,
  code fences, duplicate filenames, paths with spaces, perf on large transcripts, and stale id cleanup.
- **Deps/Risks:** Needs stable ids/ranges. Live indexing can be expensive; throttle stream deltas.
- **Platform notes:** Normalize path keys without destroying platform meaning: preserve original
  display text, but index Windows separators/drive letters case-insensitively where appropriate,
  keep Unix paths case-sensitive by default, and tolerate macOS worktrees on case-insensitive or
  case-sensitive volumes.

### Semantic Filters

- **Spec:** Filter by file path, command, tool, subagent, status, language, branch, error class,
  entry kind, approval state, health marker, and time/turn range.
- **Steps:** Define `TranscriptFilter`, metadata extraction, parser for syntax like `tool:shell
  status:failed`, validation errors, and filter integration into row building. Share state between
  main and Ctrl+T.
- **Verify:** Unit-test every dimension and combined filters. Snapshot hidden vs dimmed rendering,
  no-match state, malformed syntax, scroll anchor preservation, and copy/export visible-vs-full mode.
- **Deps/Risks:** Depends on metadata. Hidden filters can look like data loss; active filter UI must
  be unmistakable.

### Related-Entry Links

- **Spec:** Link prompt -> assistant -> tool call -> result -> edit -> error -> fix -> follow-up,
  plus approvals and subagents.
- **Steps:** Add `TranscriptRelationGraph`, relation types with confidence/provenance, direct event
  relations, derived relations from paths/commands/errors/subagents, and relation affordances via
  hit-test/action registry.
- **Verify:** Unit-test representative event sequences, jumps after resize/filter/fold/search,
  relation cleanup after clear/compaction/session switch, and subagent linkage fixtures.
- **Deps/Risks:** Needs event provenance. Derived links can be noisy; rank weak links lower and expose
  provenance in debug mode.

### Duplicate-Output Folding

- **Spec:** Collapse repeated logs/progress lines while preserving raw content for expand, search,
  copy, export, and diagnostics. Errors remain visible.
- **Steps:** Add `FoldSpan`, repeated-line/progress/ANSI repeat detection, raw output retention,
  folding in row projection, and copy/export visible-vs-full modes.
- **Verify:** Test repeats, progress rewrites, ANSI repeats, near-repeats, non-repeats, unique error
  visibility, search inside folds, and large repeated output perf.
- **Deps/Risks:** Search/selection mapping through folds is tricky. Raw retention can be memory-heavy.

### Code-Aware Copy/Export

- **Spec:** Preserve code fences, languages, file headers, line ranges, diff metadata, command
  metadata, and tool boundaries in copy/export.
- **Steps:** Define `CopyFormat`, serialize from transcript entries/source ranges, preserve code/diff
  metadata, add semantic copy targets, route clipboard through provider chain, share with export,
  exit mirror, diagnostics.
- **Verify:** Golden-test Markdown/plain/JSON, languages, diffs, selections inside code blocks,
  stripped UI rails, retained metadata, and large-copy fallback.
- **Deps/Risks:** Requires code metadata through streaming/settling. Large exports can expose secrets;
  use shared redaction if available.
- **Platform notes:** Preserve source line endings by default for code/diff export, but offer a
  normalized text mode. File headers and path metadata should emit portable slash-style paths unless
  the user asks for native platform paths.

### Error Lenses

- **Spec:** Detect actionable error lines in failed outputs and make them highlighted, navigable,
  filterable, and copyable.
- **Steps:** Add `ErrorLens` with source range, class, severity, message, paths, actions. Prefer
  structured status; add detectors for rustc/cargo/tests/permission/network/panic/sandbox. Feed into
  filters, index, relations, and style spans.
- **Verify:** Detector fixtures, normal/focused/selected/folded snapshots, next/previous navigation,
  path extraction, and false-positive regressions.
- **Deps/Risks:** Regex detectors are brittle; structured events should win. Retry actions must
  respect permission policy.
- **Platform notes:** Error detectors need fixtures for Unix permission/path errors, macOS sandbox and
  codesign/notarization-style failures where relevant, Linux package/toolchain failures, and Windows
  `cmd.exe`/PowerShell/ConPTY path and access-denied formats.

### Transcript Health Markers

- **Spec:** Explicit markers for stale context, compaction, truncation, unresolved approvals, failed
  subagents, degraded terminal features, and copy/export limitations.
- **Steps:** Add `TranscriptHealthMarker` with kind, severity, entry/range, message, source, actions.
  Emit from truncation, compaction, approvals, subagent failures, degraded renderer, copy/export
  fallback, and filtering. Render important markers as rows and minor ones as badges.
- **Verify:** Unit-test marker lifecycle, snapshots, search/filter by marker, stale marker removal,
  and copy/export inclusion rules.
- **Deps/Risks:** Marker noise can be high; severity and grouping matter. Marker explanations must not
  reveal hidden secret content.

---

## 12.6 Clipboard, Paste, And External Handoff

### Clipboard History Inside Squeezy

- **Spec:** Squeezy-initiated copy payloads are recoverable inside the app without reading arbitrary
  OS clipboard contents. Users can re-copy, quote, queue, export, pin, delete, or clear entries.
- **Steps:** Add `ClipboardHistoryStore` ring buffer and `ClipboardEntry` metadata. Route every copy
  through one service that writes to OSC52/platform/temp-file provider and records history. Add max
  entry/byte caps and optional persisted history.
- **Verify:** Test eviction, pinned retention, deletion, clear, metadata, copy history panel snapshots,
  no duplicated entries, and privacy rule that OS clipboard is never scraped.
- **Deps/Risks:** Clipboard payloads can contain secrets. Default session-only first, visible clear,
  optional secret detection, and disable config.
- **Platform notes:** Clipboard provider order is platform-specific: macOS can use OSC 52 and
  `pbcopy`; Linux may need Wayland (`wl-copy`), X11 (`xclip`/`xsel`), portal fallbacks, or OSC 52
  over SSH/tmux; Windows should prefer a native/PowerShell/`clip.exe` path when OSC 52 is
  unavailable. Never read arbitrary OS clipboard contents on any platform.

### Paste Transform Menu

- **Spec:** Structured/multiline pastes open a transform preview: plain, quote, code block, attach as
  file, queue prompt, append to selected queue item, or cancel.
- **Steps:** Handle bracketed paste events. Add `PastePayload`, `PasteTransform`, conservative
  detection for diff/JSON/log/code/path/large text, and a modal preview/menu. Keep transform logic
  pure.
- **Verify:** Test transforms for plain/multiline/Markdown/code/diff/JSON/logs, bracketed paste not
  triggering shortcuts, preview snapshots, and cancel preserving composer.
- **Deps/Risks:** tmux/SSH paste behavior varies. File attachments must require explicit confirmation.
- **Platform notes:** Normalize CRLF/CR handling explicitly for Windows-origin pastes while preserving
  raw text when requested. Path/diff/code detection should recognize Unix paths, Windows paths, and
  macOS/Linux shell prompts without treating terminal escape sequences as executable input.

### Large Paste Staging

- **Spec:** Huge pastes are staged before entering composer/context. The staging view shows byte/line/
  token estimates, type, preview, warnings, and actions: insert, quote, code block, temp file,
  attach, queue, split, summarize, copy preview, cancel.
- **Steps:** Add paste thresholds and temp-backed storage for large payloads. Build searchable/
  selectable staging overlay. Integrate with queue/attachments/export. Delete staged temp data on
  cancel/exit unless saved.
- **Verify:** Test thresholds, cleanup, cancel/insert, range insertion, huge payload render perf, and
  inert display of pasted escape sequences.
- **Deps/Risks:** Large data can freeze wrapping/search or inject terminal controls if written raw;
  sanitize display and require explicit commit.
- **Platform notes:** Temp-backed staging must use the platform temp/state directory abstraction
  rather than hardcoded `/tmp`; cleanup must work on Windows where open files cannot always be
  deleted or atomically replaced.

### Export Destinations

- **Spec:** One export flow for selections, entries, turns, visible transcript, full session, queue,
  tool output, and diagnostics. Destinations: clipboard, temp file, repo file, external editor, or
  bundle. Formats: text, Markdown, JSON, HTML.
- **Steps:** Add `ExportRequest`, `ExportFormat`, `ExportDestination`, source-data exporters, atomic
  file writes, path validation, redaction pass, and preview modal with size/included items.
- **Verify:** Golden Markdown/JSON/HTML outputs, selection export preserving code/diff metadata, path
  traversal rejection, atomic write failure handling, and clipboard history recording once.
- **Deps/Risks:** HTML escaping and repo-file writes are risky. Redaction is best-effort and must be
  labeled honestly.
- **Platform notes:** Path validation must handle Unix symlinks, macOS case-insensitive volumes,
  Windows drive roots/UNC/long paths/reserved names, and CRLF-vs-LF output choices. Atomic writes
  need per-platform behavior because Windows replacement semantics differ from POSIX `rename`.

### External Editor Handoff

- **Spec:** Users can edit composer text, queue items, staged pastes, export buffers, or template
  slots in `$VISUAL`/`$EDITOR`, then re-import with accept/reopen/discard/save draft.
- **Steps:** Add `EditorHandoffRequest`, editor resolution, clean terminal suspend/restore, temp file
  with syntax extension, editor subprocess, diff/summary confirmation, and cleanup.
- **Verify:** Fake editor tests for modify/unchanged/fail/sleep, terminal restore on failure,
  composer/queue apply/discard, and signal/panic coverage.
- **Deps/Risks:** `$EDITOR` parsing is tricky. Terminal restoration is critical; leaving alt-screen
  before editor is safer than running editor inside alt-screen.
- **Platform notes:** Resolve editors through `$VISUAL`/`$EDITOR` on macOS/Linux and `%VISUAL%`/
  `%EDITOR%`/known editor commands on Windows. Avoid shell-string parsing when possible; command-line
  quoting, PATHEXT lookup, spaces in `Program Files`, and terminal restoration differ sharply between
  Unix shells and Windows process creation.

### Shareable Session Bundle

- **Spec:** A portable artifact for support/handoff containing transcript exports, diagnostics,
  selected artifacts, manifest, checksums, and redaction status.
- **Steps:** Define bundle manifest schema, export Markdown/JSON transcript, include sanitized config/
  terminal/events/artifacts, package via isolated writer abstraction, atomic output path, and preview.
- **Verify:** Golden manifest and sanitized env tests, archive unpack/validate, checksum, missing
  artifacts, redaction defaults, size warnings, partial write cleanup.
- **Deps/Risks:** Bundles can leak secrets and paths; preview/redaction are mandatory. Archive
  dependency should stay isolated.
- **Platform notes:** Bundle manifests should use portable slash paths and record native path
  originals separately when needed. Handle symlinks, executable bits, CRLF text, case collisions, and
  Windows reserved names so a bundle created on macOS/Linux can be unpacked safely on Windows and
  vice versa.

---

## 12.7 Personalization

### Keybinding Editor UI

- **Spec:** Inspect/edit/reset/test keybindings inside the TUI. Shows command, current/default
  binding, source layer, contexts, conflicts, and description. Captures next key/chord and protects
  recovery bindings.
- **Steps:** Add typed `CommandId` registry, layered `KeymapResolver`, serializable key/chord structs,
  `KeyCaptureState`, generated help overlays, and delta-only persistence.
- **Verify:** Test serialization, conflicts, context precedence, reset, command lookup, input
  fixtures, editor snapshots, and registry completeness.
- **Deps/Risks:** Terminal key reporting varies. Some chords are ambiguous. Esc/help/reset-safe-mode
  need guardrails.
- **Platform notes:** Defaults and conflict detection must account for macOS Option/Command
  expectations, Linux terminal Alt/Esc behavior, Windows Ctrl/Alt/AltGr ambiguity, and terminals that
  do not support enhanced keyboard protocols. Recovery bindings must avoid platform-reserved chords.

### Theme Editor UI

- **Spec:** Semantic theme editor with live preview for palette, transcript cards, rails, fold
  indicators, selection, search, status, warnings/errors, queue, hover, clickable text, pointer
  hints, and focus rings.
- **Steps:** Add `Theme` semantic tokens, replace raw render colors with token lookups, map truecolor/
  256/no-color/high-contrast, separate `GlyphSet`, store versioned themes.
- **Verify:** Test parsing/migration/fallback/token coverage, snapshots for built-in themes, contrast
  checks, and static check preventing raw colors outside theme definitions.
- **Deps/Risks:** Terminal palettes vary. Too many tokens can overwhelm; provide presets and groups.
- **Platform notes:** Verify truecolor, 256-color, and no-color fallbacks across macOS Terminal/iTerm2,
  common Linux terminals/tmux, and Windows Terminal/legacy conhost. Do not assume the same named
  color looks identical across platforms.

### Per-Terminal Profiles

- **Spec:** Adapt UX policy to terminal quirks without forking renderer. Profiles cover mouse, OSC52,
  sync output, glyphs, scroll sensitivity, paste, focus, clipboard order.
- **Steps:** Split detected `TerminalCapabilities` from resolved `TerminalProfile`. Detect env/cap/
  tmux/SSH/ConPTY hints and optional probes. Add built-in profiles and manual pins/overrides.
- **Verify:** Fixture env-map tests, nested VS Code/tmux/SSH, no renderer path changes, `/terminal`
  snapshots, tmux/ConPTY smoke where available.
- **Deps/Risks:** Detection is probabilistic. Downgrades must be visible and overrideable.
- **Platform notes:** Built-in profiles should explicitly cover macOS Terminal/iTerm2, Linux desktop
  terminals, Linux console, tmux/screen, SSH remoting, VS Code/xterm.js, Windows Terminal, legacy
  conhost, and ConPTY. Keep OS detection separate from terminal capability detection.

### Per-Workspace UI Profile

- **Spec:** Remember density, panels, queue behavior, transcript detail, folds, filters, copy/export,
  and theme per repo without dirtying the worktree.
- **Steps:** Add layered config precedence, workspace identity by resolved root/hash, storage under
  Squeezy state/config, schema versioning, and source-label reporting.
- **Verify:** Test precedence/reset/migration/source labels, nested repos/symlinks/worktrees, no
  profile leakage, settings snapshots, and diagnostics redaction.
- **Deps/Risks:** Repo detection can be ambiguous; profile paths may reveal local info.
- **Platform notes:** Store workspace UI state through the existing Squeezy state/config abstraction,
  mapping to platform-appropriate locations such as XDG state/config on Linux, Application Support on
  macOS, and AppData on Windows. Never write profile files into the repo unless explicitly requested.

### Gesture Settings

- **Spec:** Tune double/triple-click, Shift-click, drag threshold, edge scroll, wheel/trackpad
  sensitivity, smooth scroll, hover delay, and reduced-motion behavior with a gesture test panel.
- **Steps:** Centralize mouse input in a gesture recognizer emitting semantic gestures. Add per-surface
  bindings, wheel accumulation/coalescing, reduced-motion/instant-scroll handling, and settings UI.
- **Verify:** Timestamped event tests for clicks/drags/hover/wheel/queue reorder, stress wheel/drag/
  resize/streaming, and keyboard parity audit.
- **Deps/Risks:** Crossterm often lacks high-resolution deltas and terminals compress mouse events.
- **Platform notes:** Calibrate wheel/drag behavior separately for macOS trackpads, Linux terminals
  behind tmux/SSH, Windows Terminal/ConPTY, and low-feature terminals. Every gesture must have a
  keyboard equivalent because mouse reporting may be disabled by policy or terminal capability.

### Minimal Glyph Mode

- **Spec:** ASCII-safe UI chrome for fonts/remoting/accessibility. Only Squeezy chrome changes, not
  user/tool output.
- **Steps:** Add `GlyphSet` tokens for borders, rails, folds, spinners, markers, drag handles,
  scrollbars, status, queue, expand/collapse, search. Provide Unicode/compact/ASCII sets and replace
  hardcoded chrome glyphs.
- **Verify:** Snapshots across main, Ctrl+T, queue, settings, command palette, selection, search,
  errors. Test glyph widths and static hardcoded-glyph checks.
- **Deps/Risks:** ASCII labels can add noise; keep replacements compact and consistent.
- **Platform notes:** Unicode width and glyph availability vary by font, locale, and terminal:
  Windows legacy consoles and remote Linux sessions are common failure cases, while macOS/iTerm2 may
  render wider symbol sets. Minimal glyph mode should be selectable per terminal profile.

---

## 12.8 Collaboration And Multi-Agent Visibility

### Subagent Timeline Panel

- **Spec:** Persistent navigable panel for subagents with id/name, role, status, latest activity,
  elapsed time, tool count, cost, and attention state.
- **Steps:** Extend `SubagentRecord` with structured lifecycle/activity metrics. Route subagent
  events into this model, including cap/rejection synthetic records. Render with app-owned scroll.
- **Verify:** Snapshot narrow/wide/overflow/running/completed/failed/blocked/cap-rejected. Test key
  routing with composer, Ctrl+T, config, approvals, and queue.
- **Deps/Risks:** Accurate cost depends on child metrics. Retention must be bounded. Use structured
  events, not formatted strings.

### Subagent Hover Preview And Double-Click Jump

- **Spec:** Subagent rows and mentions use the same pointer grammar as transcript rows. Hovering a
  subagent lightly strengthens the row or mention, revealing status/cost/last-activity affordances
  without resizing the layout. Single click selects or pins the subagent as the active comparison
  target. Double-click jumps to that subagent's transcript/detail pane, preserving the user's
  current main transcript scroll so they can return.
- **Steps:** Add subagent hit targets for timeline rows, mentions, review-board cards, compare tabs,
  and status indicators. Define `SubagentActivationTarget` with timeline, transcript detail,
  compare, and latest important event destinations. Route double-click through a
  `jump_to_subagent` command that opens the correct pane or tab depending on width. Store prior
  focus/scroll as a return anchor. Render hover preview with semantic style tokens, such as
  `subagent.hover`, `subagent.selected`, and `subagent.attention`.
- **Verify:** Unit-test hover/select/double-click across running, completed, failed, blocked,
  capped, and missing-metrics subagents. Snapshot timeline, review board, compare tabs, and narrow
  modal layouts. Test return-anchor restoration after jump and keyboard equivalents for select,
  pin, compare, and jump.
- **Deps/Risks:** Depends on subagent records, hit-test registry, focus/scroll anchors, layout
  solver, command registry, and style tokens. Risk is accidental context loss when jumping away from
  the main transcript; always preserve return state and provide a visible breadcrumb/back command.

### Compare Subagent Outputs

- **Spec:** Mark multiple subagents and compare their findings with attribution. Wide terminals use
  side-by-side; narrow terminals use tabs. Actions include copy pane/all, quote, and promote.
- **Steps:** Add `SubagentCompareState`, shared transcript row source, per-subagent row cache, layout
  columns/tabs, and independent scroll per pane.
- **Verify:** Snapshot 80x24 and 160x40, test mark/unmark, independent scroll, copy, failed/capped
  records, and missing metrics.
- **Deps/Risks:** Side-by-side wrapping can be expensive; cap visible columns and use tabs.

### Promote Subagent Result To Prompt

- **Spec:** Turn a useful subagent result into reviewed follow-up work. Idle fills composer; active
  turn queues prompt. Never auto-submit.
- **Steps:** Add `promote_subagent_result` using selected rows/final summary/failure diagnostic and
  clean plain-text projection. Store source metadata if structured queue items exist.
- **Verify:** Test completed/failed/capped/running records, idle vs running behavior, decoration
  stripping, and excerpt limits.
- **Deps/Risks:** Auto-drain could run promoted work unexpectedly; review insertion policy carefully.

### Live Review Board

- **Spec:** Fanout orchestration board with queued, running, reviewing, blocked, and completed lanes.
  Capped/rejected workers remain visible.
- **Steps:** Derive board from `SubagentRecord` plus planned-work records. Add lane classification,
  wide columns/narrow tabs, and stable id navigation. Do not infer runtime queueing from cap rejection.
- **Verify:** Unit-test lane classification, snapshots, navigation through empty/overflow/changing
  lanes, and cap rejection visibility.
- **Deps/Risks:** "Queued" can mislead if it is not actual runtime admission; label UI/planning state.

### Attention Routing

- **Spec:** Quiet progress updates timeline only; failures, cap rejections, blockers, approvals, and
  selected/pinned completions surface through status/toasts/title attention.
- **Steps:** Add `SubagentAttentionKind`, route quiet events to timeline, important events to visible
  warnings, and preserve all raw events in logs/evals.
- **Verify:** Test quiet activity does not overwrite important status, failures remain visible,
  heartbeats stay logged, and pinned subagents can opt into notifications.
- **Deps/Risks:** Over-filtering can hide real failures. UI suppression must never suppress logs/evals.

---

## 12.9 Reliability And Self-Healing

### Stuck-Render Watchdog

- **Spec:** Detect app state changes without successful frame refresh, capture diagnostics, invalidate
  caches, force one clean alt-screen redraw, and show a low-priority recovery status.
- **Steps:** Add `RenderHealth` with state/drawn revisions, last frame time/signature, stalled count,
  and forced redraw throttle. Increment revision on visible state changes. Record frame commit after
  draw+flush. On trigger write diagnostics, `Clear(All)`, `MoveTo(0,0)`, full render, flush.
- **Verify:** Unit-test health transitions, fake backend suppressed commits, forced clear before
  replacement frame, resize storms not triggering, and no recursive recovery.
- **Deps/Risks:** Frame signatures must be cheap and not confuse ratatui diffing. Recovery stays in
  alt-screen; teardown remains terminal guard responsibility.

### Terminal Restore Command

- **Spec:** `squeezy terminal-reset` repairs hidden cursor, raw mode symptoms, mouse, paste/focus,
  sync output, keyboard enhancement flags, alt-screen, alternate scroll, and title state.
- **Steps:** Add dependency-free CLI subcommand. Implement `TerminalRestorer` using crossterm and raw
  ANSI. Disable modes, leave alt-screen, reset attrs/title, flush. Support dry-run/json/no-title.
  Never emit scrollback purge.
- **Verify:** Golden byte sequence, idempotence, PTY/manual smoke, dry-run emits no control bytes,
  no workspace/config needed.
- **Deps/Risks:** Some state cannot be perfectly restored after crash. tmux/screen may filter
  sequences.
- **Platform notes:** Implement separate restore paths for POSIX terminals and Windows console modes.
  POSIX restore should handle termios/raw mode plus ANSI mode resets; Windows should restore console
  input/output modes where possible and also emit compatible ANSI resets for Windows Terminal/ConPTY.
  tmux/screen/SSH may require downgraded or pass-through-safe sequences.

### Last-Known-Good Layout Fallback

- **Spec:** In debug/dogfood builds, catch render/layout panic, capture failing input, and render a
  minimal transcript/composer fallback frame. If fallback panics, emergency teardown and rethrow.
- **Steps:** Narrow `catch_unwind` behind feature/env. Store `LastGoodLayout` each successful frame
  and `FailingLayoutInput` on panic. Render conservative fallback without risky helpers/panels/glyphs.
- **Verify:** Inject panic, assert fallback frame and diagnostics, no state mutation, tiny sizes, and
  recursive fallback teardown.
- **Deps/Risks:** Partial terminal writes can leave backend inconsistent. Diagnostics may include
  sensitive visible text.

### Automatic Degraded-Mode Suggestions

- **Spec:** `/terminal` shows unsupported/broken features with impact, active fallback, and exact
  suggested setting or terminal configuration.
- **Steps:** Extend capability probing with status, evidence, confidence, fallback. Add
  `DegradedModeSuggestion` and route to `/terminal`, diagnostics, toasts, metrics. Cover OSC52,
  mouse, alternate scroll, glyphs, DEC2026, focus, truecolor.
- **Verify:** Table-test capability profiles, `/terminal` snapshots, one-toast-per-feature, config
  overrides, and terminal-matrix cases for tmux/VS Code/Windows/SSH.
- **Deps/Risks:** Capabilities are not always queryable. Incorrect suggestions are worse than
  unknown; include confidence.
- **Platform notes:** Suggestions should name the detected terminal/profile when possible: macOS
  Terminal/iTerm2, Linux terminal/tmux/SSH, VS Code/xterm.js, Windows Terminal, legacy conhost, or
  ConPTY. Keep OS-specific advice behind confidence checks and always show the active fallback.

### Session Auto-Save Checkpoints For UI State

- **Spec:** Persist scroll/search/selection/queue/folds/pinned panes often enough for crash/suspend/
  resume recovery. Store logical anchors, not coordinates.
- **Steps:** Add versioned `UiStateCheckpoint` with session id, transcript revision, size, scroll
  anchors, search, selection, queue order, folds, pinned panes, detail policy. Persist via session
  storage/state partition with debounce and atomic writes. Validate and clamp on load.
- **Verify:** Serialization, resume with changed size/missing ids/stale matches, corrupt checkpoint,
  debounce, crash simulation, and perf impact tests.
- **Deps/Risks:** Requires stable row and queue ids. Search queries may be sensitive; keep local and
  honor privacy opt-out.

---

## 12.10 Measurement And Quality Gates

### UX Latency Budgets

- **Spec:** Define measurable p95/p99 budgets for keypress echo, scroll, page jumps, queue drag,
  paste preview, copy ack, search jump, and resize redraw. Measure event receipt, dispatch, state
  mutation, render, write, and flush separately.
- **Steps:** Add `TuiLatencyBudget`, tag interactions, instrument event loop phases, track row-cache
  rebuild, ratatui render, crossterm flush, clipboard, search, and resize. Add `/metrics` or debug
  overlay with percentiles and last violation.
- **Verify:** Unit-test interaction ids, harness burst typing/scroll/paste/search/queue/resize, fake
  clocks where possible, and report-only real-terminal baseline initially.
- **Deps/Risks:** Wall-clock terminal latency is noisy; separate app time from PTY/terminal flush.
  Pair latency with correctness.
- **Platform notes:** Maintain separate baselines for macOS local terminals, Linux local/tmux/SSH, and
  Windows Terminal/ConPTY. Networked SSH latency and CI virtualization should be report-only unless
  the benchmark controls the environment.

### Real Terminal Benchmark Suite

- **Spec:** Benchmark actual fullscreen byte stream through realistic terminals for latency, emitted
  bytes, smoothness, resize, copy fallback, and teardown correctness.
- **Steps:** Extend term-matrix benchmark mode with capture, scripted size source, xterm.js, tmux,
  PTY, vt100, and optional alacritty backends. Emit JSON summaries for scenario/backend/size/frames/
  bytes/p95/scroll jumps/teardown.
- **Verify:** Fast Rust backends on every PR; xterm/tmux/PTY report-only then required. Hard invariants:
  one composer, no duplicated UI, cursor bounds, stable selected row, latest output visible, clean
  teardown, mirror after alt-screen leave.
- **Deps/Risks:** Node/tmux/PTY tests are environment-sensitive. Keep raw byte logs only on failure.
- **Platform notes:** Use Rust-only backends as the portable floor on every platform. Run xterm.js
  wherever Node is available, tmux/PTY primarily on macOS/Linux, and add a Windows ConPTY leg for
  Windows Terminal behavior. Do not make POSIX-only PTY assumptions in Windows CI.

### Dogfood Telemetry Counters

- **Spec:** Local privacy-preserving counters explain renderer quality across terminals without
  storing prompt/transcript/commands/paths/env/clipboard/model text.
- **Steps:** Add `TuiMetrics` in-memory collector with optional JSONL persistence. Record frame/flush
  duration, bytes, skipped frames, cache hits, wrap cost, input counts, resize storms, scroll deltas,
  copy providers, terminal profile, emergency teardown, reduced motion/high contrast. Add config/env
  controls and `/metrics`.
- **Verify:** Schema cannot contain payloads, `/metrics` snapshots, event-loop counters, JSONL
  versioning, fake secret privacy tests, dogfood gates before inline deletion.
- **Deps/Risks:** Metrics can become logs; keep fields numeric/enumerated and buffered.
- **Platform notes:** Platform/terminal profile may be recorded only as bounded enum-like values
  such as `macos_iterm2`, `linux_tmux`, or `windows_terminal`, not raw environment variables,
  usernames, paths, shell commands, clipboard contents, or hostnames.

### Visual Diff Dashboard

- **Spec:** Static artifact comparing rendered frames across scenarios, sizes, themes, and backends.
  It should expose clipped text, overlap, stale cells, duplicated composer, missing focus, bad colors,
  and resize artifacts.
- **Steps:** Build from `TuiHarness`, term-matrix replay, plain/styled grids, semantic regions,
  backend viewport, metrics, and metadata. Generate static HTML with filters and side-by-side
  baseline/current cell-grid diffs.
- **Verify:** Snapshot dashboard HTML, fixture one-cell/moved/clipped/stale changes, renderer modes,
  queue/search/selection/copy status, and explicit baseline-update workflow.
- **Deps/Risks:** Raster screenshots vary; keep cell-grid HTML primary. Curate scenarios to avoid
  snapshot noise.

### Accessibility Quality Gate

- **Spec:** Major TUI work must pass contrast, no-color-only meaning, reduced motion, keyboard
  reachability, and mouse-free scroll/select/copy/search/navigation gates.
- **Steps:** Add accessibility audit module, built-in theme contrast checks, reduced motion config,
  no-color/ASCII/high-contrast modes, `ActionId` keyboard reachability audit, and keyboard parity for
  queue, transcript, detail, copy, search, and exit.
- **Verify:** Contrast tests, monochrome snapshots, reduced-motion tick tests, clickable action
  keyboard audit, no-mouse harness scenarios, and visual diff dashboard in accessibility modes.
- **Deps/Risks:** Terminal palettes and Unicode widths vary. Explicit labels can clutter compact UI;
  make labels mode-aware.
- **Platform notes:** Accessibility gates should run against at least one macOS-style terminal
  profile, one Linux/tmux or Linux console profile, and one Windows Terminal/ConPTY profile. High
  contrast and reduced-motion behavior must be app-controlled because OS accessibility settings are
  not reliably exposed inside terminal sessions.
