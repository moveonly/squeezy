# TUI Alt-Screen Renderer — Parallelized Execution Plan (Workflow-Driven)

> Companion to `TUI_ALT_SCREEN_RENDERER_PLAN.md`. That doc is *what* and *why*. This doc is
> *how to execute it with maximum parallelism*: the dependency DAG, the file-contention model,
> the wave schedule, and the concrete `Workflow` orchestrations to run each wave.
>
> Grounded in a 13-agent code map of `crates/squeezy-tui/` (June 2026). All line numbers verified
> against the current tree; they drift — anchor on symbol names.

---

## 0. The governing constraint (why naive parallelism fails)

`crates/squeezy-tui/src/lib.rs` is **18,940 lines in one file**, and essentially every phase of the
migration edits it. That single fact caps parallelism. Two regions are *dense* collision zones —
multiple subsystems target the same ~1,700 contiguous lines:

- **Z1 — RENDER zone (~6867–8545):** `render()` (6867), `render_inline()` (7046), `render_transcript`
  (7737), the wrapping/row helpers (`wrap_transcript_overlay_rows` 7955, `rail_prefix_width` 7971,
  `split_spans_at_column` 8015, `wrap_cells_preserving` 8047, `wrap_transcript_overlay_line` 8084,
  `transcript_lines_for_overlay` 8311, `cached_transcript_entry_lines` 8545). Targeted simultaneously
  by the fullscreen refactor, inline deletion, overlay-wrapping extraction, **and** scroll widening.
- **Z2 — GUARD zone (~17961–18936):** `TerminalGuard` struct (17961), `draw_app` (18152),
  `paint_main` (18205), `prepare_history` (18296), `enter/leave_overlay_screen` (18373/18412),
  `clear_scrollback_and_visible` (18474), `render_footer_to_buffer` (18499), `emit_buffer_row_styled`
  (18598). Targeted by lifecycle, inline deletion, mirror pipeline, **and** the `SizeSource` field.

Plus a contended **test file**: `lib_tests.rs` (15,888 lines) — `render_to_string` has **86 call
sites**, `render_inline_to_string` has 8. Deleting either is a bottom-up, single-pass tail operation.

**Thesis of this plan:** *convert one-file contention into many-file parallelism.* The migration is
front-loaded with **extraction** (carve the hot zones into modules) so that all later work lands in
**separate files** that can be edited concurrently. After extraction, the only serialized spine is
the fullscreen lifecycle and the inline-deletion tail; everything else fans out in greenfield.

### 0.1 The escape hatch — verified greenfield

None of these exist yet, so building them is **collision-free** with `lib.rs` edits:

| New file | Owns | From plan phase |
|---|---|---|
| `transcript_surface.rs` | shared row model, wrapping, row ids, copy ranges | 3 |
| `terminal_guard.rs` | lifecycle, `enter`, `finish_fullscreen`, emergency teardown | 1, 2, 9 |
| `scroll.rs` | follow-tail, anchor math, scrollbar geometry | 4 |
| `selection.rs` | visual selection, range math, semantic targets | 5 |
| `clipboard.rs` | OSC 52 + platform providers + temp-file fallback | 5 |
| `search.rs` | incremental search, match nav | 7 |
| `interaction.rs` | focus, hit-test registry, gestures | 7B |
| `queue_surface.rs` | queue rows, drag/drop, delete | 7B |
| `modal.rs` | shared modal/centered-block surface for pickers | 6 |
| `termsim/` (+ `tools/termsim/`) | capture seam, emulator backends, scenarios, invariants | 0, 8 |
| `terminal_writer.rs` `Capture` variant | tee bytes for tests (file exists, variant is new) | 0B |
| `SizeSource` trait | injectable terminal dimensions | 0B |

### 0.2 Zone model for the residual `lib.rs` work

Edits that *must* stay in `lib.rs` are bucketed into four disjoint zones. **Across zones → safe to
parallelize** (with worktree isolation + a merge step, since regions don't overlap). **Within a zone
→ serialize under one nominated owner.**

- **Z1 RENDER** (~6867–8545) → mostly *evacuated* in Wave 1 into `render/` + `transcript_surface.rs`.
- **Z2 GUARD** (~17961–18936) → mostly *evacuated* in Wave 1 into `terminal_guard.rs`.
- **Z3 APP-STATE/EVENT** (struct fields ~15938/16080; `handle_key`/`handle_mouse`/dispatch ~2059–5512):
  scroll fields, selection state, focus, click registry, queue dispatch, scroll call-sites.
- **Z4 TOP** (~96–267): `SizeSource` trait, `INLINE_VIEWPORT_HEIGHT` (152), alt-screen helper fns
  (240/251/267), constants.

**Integration points** (unavoidable shared edits, but *additive and cheap*): `mod x;` declarations
at the top of `lib.rs`; new `Action` variants in `keymap.rs` (465 lines, additive); new dispatch
arms in `handle_key`/`handle_mouse` (Z3). These get a single **integration owner** per wave who
plugs in the greenfield lanes' self-contained `handle_*` functions after they land.

---

## 1. The seven enabling moves (the leverage)

From the dependency synthesis — these are ordered by leverage. The first five are the critical path
that *unlocks* parallelism; the rest reduce friction.

1. **MOVE 1 — Evacuate the RENDER zone.** Extract `render()`, `render_inline()`, `render_transcript`,
   the `*_height` helpers, and the `render_*` widgets out of `lib.rs` into `render/main.rs`,
   `render/footer.rs`, `render/transcript.rs`. (Today `mod render` is only a toolkit — markdown/theme/
   card/palette/spinner/diff — *not* the home of `render()`. This is a real refactor.) Collapses the
   biggest collision zone.
2. **MOVE 2 — Extract `transcript_surface.rs`.** Pull the wrapping/row-model helpers (7955–8119) plus
   `transcript_lines_for_overlay` (8311) and `cached_transcript_entry_lines` (8545) into one shared
   row model. ⚠️ `wrap_cells_preserving` (8047) is shared by entry formatters — extract as a
   module-level shared fn, **not** a copy. Unblocks Ctrl+T unification *and* clipboard copy-range math.
3. **MOVE 3 — Add the `SizeSource` seam early.** Trait near `lib.rs:96`, field on `TerminalGuard`
   (17961), default impl wrapping `crossterm::size`, swap the two `terminal_size()` sites (2194,
   18206). Small (2 call-sites). Cache `(w,h)` on the struct to avoid per-frame dispatch. Must precede
   other `TerminalGuard` struct churn so the struct isn't re-merged repeatedly.
4. **MOVE 4 — Add `TerminalWriter::Capture`.** Self-contained 90-line file (`terminal_writer.rs`,
   currently only `Plain`/`Tee`). Add a from-sink constructor (don't break `from_optional_path`'s
   `Option<OsString>` signature). Unblocks in-memory output assertions independent of all `lib.rs` work.
5. **MOVE 5 — Widen scroll to `usize`, signature-first.** Chain: getter (8685) / setter (8691) /
   `transcript_scroll_offset` → fields (`transcript_scroll_from_bottom` 16080, `SubagentRecord::
   scroll_from_bottom` 15938) → the ~13 cast sites (2059, 2061, 2064, 2066, 2320, 2430, 3258, 3269,
   3311, 3312, 3316, 3317, 5444, 7742). Prefer a `follow_tail: bool` over a `usize::MAX` sentinel.
   One serialized pass; removes a `u16/usize` landmine from every later scroll/overlay edit.
6. **MOVE 6 — `modal.rs` before picker refactors.** Greenfield shared `Clear + centered block`
   abstraction; de-dupes both pickers' `render_widget(Clear, full)` flow.
7. **MOVE 7 — Single owner for `draw_app` (18152) + `TerminalGuard` struct (17961).** These are the
   convergence points for 5+ subsystems. Serialize *by ownership*, not by code structure, for the
   whole migration window.

---

## 2. Dependency DAG → wave schedule

```
WAVE 0  Seams & Oracle  ───────────────────────────────────────────────┐  (parallel, off hot path)
  ├─ 0A xtermcheck oracle (new files, node)              [greenfield]   │
  ├─ MOVE 4 TerminalWriter::Capture (terminal_writer.rs) [isolated]     │
  ├─ Cargo deps + term-matrix feature + xtask scaffold   [Cargo/new]    │
  └─ MOVE 3 SizeSource seam (lib.rs Z4+Z2 struct, tiny)  [spine, solo]  │
        gate: build+test green; oracle reproduces 22-stack on inline    │
                                                                        ▼
WAVE 1  Extraction (the leverage) ──────────────────────────────────────┐  (worktree-parallel + merge)
  ├─ Lane R: MOVE 1 + MOVE 2  (Z1 → render/*, transcript_surface.rs)    │
  ├─ Lane G: extract TerminalGuard (Z2 → terminal_guard.rs)            │
  └─ Lane S: MOVE 5 scroll usize  (runs AFTER R; 7742 moved into        │
             render/transcript.rs)                                      │
        gate: lib.rs ~halved; build+test green; render_frame unchanged  │
                                                                        ▼
WAVE 2  Fullscreen core (now disjoint files → parallel) ────────────────┐
  ├─ Lane Lifecycle (terminal_guard.rs): Phase 1 boot + Phase 2 mirror  │
  ├─ Lane Scroll-UX (render/* + scroll.rs): Phase 4 follow-tail/scrollbar│
  └─ Lane Surface (transcript_surface.rs): Phase 3 finalize row model    │
        gate: fullscreen boots/resizes/tears down; xterm.js 0 stacks;    │
              exit-mirror byte-order test; scroll unit tests             │
                                                                        ▼
WAVE 3  Product completeness (greenfield fan-out → max parallel) ───────┐
  ├─ clipboard.rs   ├─ selection.rs   ├─ search.rs                       │
  ├─ interaction.rs ├─ queue_surface.rs ├─ modal.rs + picker refactors   │
  ├─ export (/export) ├─ Ctrl+T unification (detail policy)              │
        + ONE Z3 integration owner wires handle_key/keymap arms          │
        gate: copy-range/clipboard/search/queue/picker tests; snapshots  │
                                                                        ▼
WAVE 4  Delete inline + tests reflect reality (serialized tail) ────────┐
  └─ bottom-up deletion chain (lib.rs/terminal_guard.rs/lib_tests.rs)    │
        gate: zero dead-code warnings; one renderer; CI == dogfood       │
                                                                        ▼
WAVE 5  Hardening (parallel, disjoint files) ───────────────────────────┐
  ├─ Phase 8 perf (render/* + transcript_surface.rs)                     │
  ├─ Phase 9 signals/suspend (terminal_guard.rs)                         │
  ├─ Phase 0B/8 full term-matrix (termsim/ backends + scenarios + CI)    │
  └─ Windows/ConPTY (cfg(windows))                                       │
        gate: full term-matrix green all backends; teardown+perf tests   │
                                                                        ▼
BACKLOG §11  themed parallel workflows, post-spine
```

### 2.1 Hard ordering edges (must not be reordered)

- `SizeSource` (MOVE 3) **before** any `TerminalGuard` struct churn (re-merge avoidance).
- `TerminalWriter::Capture` (MOVE 4) **before** harness `Capture` wiring (harness instantiates it).
- Scroll signature widening **before** field widening **before** the 13 cast sites (won't compile
  otherwise).
- `transcript_surface.rs` extraction (MOVE 2) **before** Ctrl+T unification *and* clipboard copy-range.
- Inline deletion chain is a **hard order**: `paint_main` (sole caller of `prepare_history` +
  `render_footer_to_buffer`) → `render_footer_to_buffer` → `render_inline`. `draw_app` must drop the
  `paint_main`/`clear_scrollback_and_visible` calls *in the same change* as their deletion.
- Inline deletion **requires** all overlay/config/picker flows already render via fullscreen
  `render()` (the `draw_app` `overlay_screen_active` branch must be self-sufficient first).
- `lib_tests.rs` helper deletions **after** retargeting all 86 `render_to_string` / 8
  `render_inline_to_string` call sites.
- Picker refactor **after** `modal.rs` (MOVE 6) and **after** `SizeSource`/struct stabilization.
- Clipboard Phase 5 selection math **after** scroll widening + `transcript_surface.rs`.

---

## 3. Parallelism budget — what runs concurrently at peak

| Wave | Concurrent lanes | Ceiling set by |
|---|---|---|
| 0 | 4 (3 non-`lib.rs` + 1 tiny `lib.rs` solo) | only 1 lib.rs editor |
| 1 | 2 worktree carves (R, G) → then S | disjoint regions; merge step |
| 2 | 3 (different files) | terminal_guard vs render vs surface |
| 3 | 8–10 greenfield lanes + 1 integration owner | new files = near-unbounded |
| 4 | 1 (serialized tail) | hard deletion chain |
| 5 | 4 (disjoint files) | perf vs signals vs termsim vs win |

**Peak parallelism is Wave 3** (the greenfield fan-out). The spine — MOVE 3 → Wave 1 carve → Wave 2
lifecycle → Wave 4 deletion — is the irreducible serial critical path. Everything else hangs off it.

---

## 4. Contention & merge protocol

1. **Worktree isolation for the Wave 1 carves.** Lane R (Z1 → `render/*`, `transcript_surface.rs`)
   and Lane G (Z2 → `terminal_guard.rs`) edit disjoint regions of `lib.rs`; run them in separate git
   worktrees (`isolation: 'worktree'`), then a dedicated **merge/verify agent** integrates. The only
   conflicts are the trivial `mod` lines at the top — resolve, `cargo build`, done.
2. **Sequence Lane S (scroll) after Lane R.** Cast site 7742 lives in `render_transcript`, which Lane
   R moves into `render/transcript.rs`; widening must target the post-move location.
3. **One owner per zone.** Z3 (event/dispatch) and the `TerminalGuard` struct each get a single owner
   for the migration window (MOVE 7). Greenfield lanes never edit Z3 directly — they deliver
   self-contained `handle_*`/`ClickAction` functions the Z3 owner plugs in.
4. **`keymap.rs` is additive.** New `Action` variants from clipboard/selection/queue lanes are
   append-only; a quick merge order avoids churn. Not a real bottleneck.
5. **`lib_tests.rs` is a serialized tail.** All `render_to_string` (86) / `render_inline_to_string`
   (8) retargeting happens in one pass in Wave 4, bottom-up. No lane touches it mid-migration except
   to *add* fullscreen/surface tests in new test modules.
6. **`SQUEEZY_INLINE_REPRO=1`** keeps the inline path alive *only* until the xterm.js oracle proves
   the old bug and fullscreen is dogfood-stable; deleted in Wave 4. Never a user-facing mode.

---

## 5. Verification gates (per wave)

- **W0:** `cargo build -p squeezy-tui && cargo test -p squeezy-tui` green; xterm.js replay reproduces
  the ≥16-stack on the inline binary (oracle proves the bug before we touch the renderer).
- **W1:** build+test green; `TuiHarness::render_frame` output byte-identical (pure refactor, no
  behavior change); `lib.rs` line count materially down; no new public API.
- **W2:** fullscreen boots/draws/resizes/tears down cleanly; xterm.js + alacritty replay → **0 stacked
  dividers**; exit-mirror byte-order test (`LeaveAlternateScreen` precedes CRLF mirror rows; no
  `\x1b[3J`); per-scroll-command unit tests; resize-while-scrolled keeps logical anchor.
- **W3:** copy-range tests across wrapped/wide-glyph/rail/fence rows; clipboard provider chain trace
  (OSC52 → platform → temp-file); incremental search match nav; queue reorder/delete/drag-cancel;
  picker/`/clear`/resume tests on the fullscreen path; first `insta` snapshots.
- **W4:** zero dead-code warnings; no inline product flag; no second terminal; CI and dogfooding
  exercise one renderer.
- **W5:** full `term-matrix` green across xterm.js + alacritty_terminal + vt100; panic/Ctrl+C/SIGTERM/
  Ctrl+Z teardown tests; perf stress (huge tool output, thousands of entries, resize storms) bounded.

Cross-cutting: `cargo test -p squeezy-eval` green every wave (it drives the same `render()` via the
`testing` feature — already verified). This is the free regression net for the fullscreen path.

---

## 6. Workflow catalog (runnable orchestrations)

Each wave is one `Workflow` invocation. Sketches below; author the final scripts at run time. The
patterns: **`pipeline`** for sequential-on-same-file, **`parallel` with `isolation: 'worktree'`** for
concurrent `lib.rs` carves, plain **`parallel`** for greenfield fan-out, and a **verify lane** after
each generative lane (adversarial: "does this compile, keep `render_frame` byte-identical, and not
regress eval?").

### 6.1 Wave 0 — seams & oracle

```js
export const meta = {
  name: 'tui-altscreen-w0-seams',
  description: 'Wave 0: xtermcheck oracle, Capture writer, SizeSource seam, term-matrix deps',
  phases: [{ title: 'Build seams' }, { title: 'Verify' }],
}
const LANES = [
  { key: 'oracle',   prompt: 'Port /tmp/xtermcheck (@xterm/headless@6 per-frame width-reconstruction replay) into crates/squeezy-tui/tools/termsim/xtermcheck/. New files only. Add a runner. Capture a VS Code width-drag log and assert >1 composer horizon on the current inline binary.' },
  { key: 'capture',  prompt: 'In crates/squeezy-tui/src/terminal_writer.rs add a Capture variant teeing bytes into Arc<Mutex<Vec<u8>>>, with write/flush arms and a from-sink constructor (do NOT break from_optional_path). Add terminal_writer_tests.rs coverage. terminal_writer.rs only.' },
  { key: 'deps',     prompt: 'Add a term-matrix feature to crates/squeezy-tui/Cargo.toml and dev-deps vt100 + alacritty_terminal + insta (gated). Scaffold an xtask crate in the workspace for `cargo xtask term-matrix`. Cargo.toml + new crate only; no lib.rs.' },
  { key: 'sizesrc',  prompt: 'Add a SizeSource trait near lib.rs:96 (default impl wraps crossterm::size, caches (w,h) on TerminalGuard struct ~17961), and swap terminal_size() at 2194 and 18206. SOLO owner of lib.rs this wave. Keep behavior identical.' },
]
phase('Build seams')
const built = await parallel(LANES.map(l => () =>
  agent(l.prompt + '\nReturn a summary of files changed and how to verify.', { label: `w0:${l.key}`, phase: 'Build seams' })))
phase('Verify')
await agent('Run cargo build+test -p squeezy-tui and the xtermcheck oracle. Confirm the inline binary reproduces the stacked-divider bug and the tree is green. Report pass/fail with output.', { label: 'w0:verify', phase: 'Verify' })
return built.filter(Boolean)
```

### 6.2 Wave 1 — extraction (worktree-parallel carves + merge)

```js
export const meta = {
  name: 'tui-altscreen-w1-extract',
  description: 'Wave 1: evacuate RENDER and GUARD zones into modules; widen scroll to usize',
  phases: [{ title: 'Carve' }, { title: 'Merge+widen' }, { title: 'Verify' }],
}
phase('Carve')
const carves = await parallel([
  () => agent('Lane R: extract render(), render_inline, render_transcript, *_height helpers and render_* widgets from lib.rs (~6867-8545) into render/main.rs, render/footer.rs, render/transcript.rs; and the wrapping/row helpers (7955-8311 + cached_transcript_entry_lines 8545) into transcript_surface.rs. wrap_cells_preserving must be a shared module fn, not a copy. Pure move; render_frame output must stay byte-identical.',
    { label: 'w1:render-zone', phase: 'Carve', isolation: 'worktree' }),
  () => agent('Lane G: extract the TerminalGuard lifecycle (struct 17961, draw_app, paint_main, prepare_history, enter/leave_overlay_screen, clear_scrollback_and_visible, footer + emit helpers, Drop) from lib.rs into terminal_guard.rs. Pure move, no behavior change.',
    { label: 'w1:guard-zone', phase: 'Carve', isolation: 'worktree' }),
])
phase('Merge+widen')
await agent('Merge the render-zone and guard-zone worktrees into the branch (only the top-of-file `mod` lines conflict). Then widen the scroll model to usize signature-first (getter 8685/setter 8691, fields 16080+15938, the ~13 cast sites incl. 7742 now in render/transcript.rs); prefer follow_tail:bool over usize::MAX. cargo build green.',
  { label: 'w1:merge-scroll', phase: 'Merge+widen' })
phase('Verify')
await agent('cargo test -p squeezy-tui AND -p squeezy-eval. Assert render_frame snapshots unchanged vs pre-wave (pure refactor). Report lib.rs line delta and any behavior diff.', { label: 'w1:verify', phase: 'Verify' })
```

### 6.3 Wave 2 — fullscreen core (3 disjoint-file lanes)

```js
export const meta = {
  name: 'tui-altscreen-w2-core',
  description: 'Wave 2: fullscreen lifecycle + exit mirror + scroll UX + finalize row model',
  phases: [{ title: 'Implement' }, { title: 'Verify' }],
}
phase('Implement')
const lanes = await parallel([
  () => agent('terminal_guard.rs: Phase 1 (enter alt-screen at startup, Viewport::Fullscreen, draw_app always render(), mouse capture default-on unless SQUEEZY_MOUSE_CAPTURE=0, Drop emergency-only) + Phase 2 (finish_fullscreen(app): leave alt-screen THEN mirror CRLF rows, exit hint, restore modes; never \\x1b[3J; idempotence guards). Keep SQUEEZY_INLINE_REPRO=1 path behind a flag.', { label: 'w2:lifecycle', phase: 'Implement' }),
  () => agent('scroll.rs + render/*: Phase 4 follow-tail (pin/freeze/End re-pin + indicator), main-view scrollbar via shared geometry, scroll commands (wheel/PgUp/PgDn/Home/End/turn/tool/error jumps), smooth-scroll primitives, resize anchor preservation.', { label: 'w2:scroll-ux', phase: 'Implement' }),
  () => agent('transcript_surface.rs: Phase 3 finalize the row model (row_id, entry_id, entry_kind, detail_policy, visual_line_index, text_range, copy_range, style_spans, fold_state, search_match_ranges, click targets) + cache key (session,entry,revision,width,detail,theme,fold,search). Main=collapsed/follow; Ctrl+T=expanded/independent.', { label: 'w2:surface', phase: 'Implement' }),
])
phase('Verify')
await agent('Build+test. Run xterm.js + alacritty replay storms -> assert 0 stacked dividers, latest response present, cursor in [0,h). Add+run exit-mirror byte-order test and scroll-command unit tests. Report.', { label: 'w2:verify', phase: 'Verify' })
```

### 6.4 Wave 3 — product completeness (greenfield fan-out, verify-per-lane)

```js
export const meta = {
  name: 'tui-altscreen-w3-product',
  description: 'Wave 3: clipboard, selection, search, interaction, queue, modal/pickers, export, Ctrl+T',
  phases: [{ title: 'Build modules' }, { title: 'Integrate' }, { title: 'Verify' }],
}
const MODULES = [
  { f: 'clipboard.rs',    p: 'Phase 5 provider chain: OSC 52 (payload limit + chunking) -> pbcopy/wl-copy/xclip/clip.exe -> temp-file fallback; status toasts; unit-testable in isolation.' },
  { f: 'selection.rs',    p: 'Phase 5 visual selection state machine + row/char range math over transcript_surface rows; keyboard (Shift+arrows/page/home/end) + mouse drag/double/triple click.' },
  { f: 'search.rs',       p: 'Phase 7 incremental search + next/prev match over the shared row model; include/exclude tool output/reasoning.' },
  { f: 'interaction.rs',  p: 'Phase 7B frame-local hit-test registry, focus model, gestures; every mouse op has a keyboard equivalent; targets derive from row ids+rects.' },
  { f: 'queue_surface.rs',p: 'Phase 7B queue rows with ids/focus, clickable delete x, drag handle + live insertion marker, keyboard parity; drag state is model state.' },
  { f: 'modal.rs+pickers',p: 'MOVE 6 modal.rs shared surface, then adapt resume_picker.rs + startup_model_picker.rs to render through TerminalGuard::term(); /clear resets model + clears grid (toast, not transcript).' },
  { f: 'export',          p: 'Phase 5 /export md|txt|json|html reusing the copy formatter.' },
  { f: 'ctrlt-unify',     p: 'Phase 7 make Ctrl+T a detail policy over transcript_surface (no forked wrapping/copy/search); expand/collapse-all, filters, fold memory.' },
]
const results = await pipeline(MODULES,
  m => agent(`Build ${m.f}: ${m.p} Deliver the module + its own unit tests + a self-contained handle_* fn for the Z3 owner to wire (do NOT edit handle_key yourself).`, { label: `w3:${m.f}`, phase: 'Build modules' }),
  (r, m) => agent(`Adversarially verify ${m.f}: compiles standalone, tests pass, range/copy/search math correct on wrapped+wide-glyph rows. Report defects.`, { label: `w3:verify:${m.f}`, phase: 'Verify' }))
phase('Integrate')
await agent('Z3 integration owner: add the new keymap.rs Action variants (additive) and wire each module\'s handle_* fn into handle_key/handle_mouse + TuiApp struct fields. Single pass, build green. Then run cargo test -p squeezy-tui + insta snapshots.', { label: 'w3:integrate', phase: 'Integrate' })
return results.filter(Boolean)
```

### 6.5 Wave 4 — delete inline (serialized tail)

```js
export const meta = { name: 'tui-altscreen-w4-delete', description: 'Wave 4: delete inline renderer + retarget tests', phases: [{ title: 'Delete' }] }
phase('Delete')
// Hard order — one agent, sequential, because the call-graph chains.
await agent(`Phase 10 deletion, bottom-up: (1) retarget all 86 render_to_string + 8 render_inline_to_string call sites in lib_tests.rs and delete the 5 inline-contract tests; (2) delete draw_app's paint_main/clear_scrollback_and_visible calls together with paint_main, prepare_history, clear_scrollback_and_visible; (3) delete render_footer_to_buffer, emit_buffer_as_lines, capped_footer_height, footer_content_height, render_lines_to_buffer/owned (unless mirror/export still need them); (4) delete render_inline + inline_history_lines_for_flush*; (5) delete INLINE_VIEWPORT_HEIGHT/RESET_AND_CLEAR_VISIBLE/CLEAR_SCROLLBACK_AND_VISIBLE; (6) delete overlay_terminal/overlay_screen_active/enter/leave/sync_overlay_screen and the inline TerminalGuard state fields; (7) remove SQUEEZY_INLINE_REPRO from product code. Keep cargo build+test green after EACH step. Zero dead-code warnings at the end.`, { label: 'w4:delete', phase: 'Delete' })
```

### 6.6 Wave 5 — hardening (parallel, disjoint files)

```js
export const meta = { name: 'tui-altscreen-w5-harden', description: 'Wave 5: perf, signals, full term-matrix, Windows', phases: [{ title: 'Harden' }, { title: 'Verify' }] }
phase('Harden')
await parallel([
  () => agent('Phase 8: revision-keyed row caches, virtualization near viewport, LRU bounds, frame-budget instrumentation, wide-glyph audit. render/* + transcript_surface.rs.', { label: 'w5:perf', phase: 'Harden' }),
  () => agent('Phase 9: panic hook + SIGTERM/SIGHUP emergency teardown + Ctrl+Z suspend/resume (re-enter alt-screen, redraw from model); idempotent state machine. terminal_guard.rs.', { label: 'w5:signals', phase: 'Harden' }),
  () => agent('Phase 0B/8: full term-matrix in termsim/ — alacritty_terminal + vt100 backends + the §8.4 scenario set + §8.5 invariants; xtask + feature-gated #[test] wrapper; CI gate.', { label: 'w5:termsim', phase: 'Harden' }),
  () => agent('Windows/ConPTY: verify alt-screen enter/leave, mouse, clipboard fallback, resize, panic teardown under cfg(windows).', { label: 'w5:windows', phase: 'Harden' }),
])
phase('Verify')
await agent('Run the full term-matrix across all backends + teardown + perf stress. Assert green and bounded. Report.', { label: 'w5:verify', phase: 'Verify' })
```

### 6.7 Backlog (§11) — themed parallel workflows, post-spine

Once the spine lands, the §11 capability backlog parallelizes by theme, each a `parallel` fan-out over
its items against the now-stable module boundaries: selection/clipboard (§11.1), scroll/nav (§11.2),
perf (§11.3), persistence (§11.4), mouse/input (§11.5), queue (§11.6), Ctrl+T (§11.7), robustness
(§11.8). These are additive and mostly land in their owning greenfield modules — high parallelism,
low contention.

---

## 7. One-paragraph execution summary

Run **Wave 0** to stand up the oracle + seams (4-way parallel, only `SizeSource` touches `lib.rs`).
Run **Wave 1** to evacuate the two `lib.rs` hot zones into `render/*`, `transcript_surface.rs`, and
`terminal_guard.rs` (two worktree-parallel carves + a merge/scroll-widen step) — this is the single
most important move, converting one-file contention into many-file parallelism. From there the work
fans out: **Wave 2** builds the fullscreen lifecycle, scroll UX, and row model in three disjoint
files concurrently; **Wave 3** is the peak — 8–10 greenfield module lanes (clipboard, selection,
search, interaction, queue, modal/pickers, export, Ctrl+T) verified per-lane, then a single Z3
integration owner wires them in; **Wave 4** is the irreducible serialized deletion tail; **Wave 5**
hardens perf/signals/term-matrix/Windows in parallel. The critical path is
`SizeSource → carve → fullscreen lifecycle → delete inline`; everything else hangs off greenfield and
runs concurrently. Every wave keeps `cargo test -p squeezy-tui` and `-p squeezy-eval` green, and
`squeezy-eval` (which already drives the fullscreen `render()`) is the free regression net.

---

## 8. §12 future-improvement specs — parallelization

§12 of the companion plan is **post-core capability work**: it is *not* on the critical path and adds
no new constraint to the spine (`SizeSource → carve → fullscreen lifecycle → delete inline`). Every
§12 subsection layers on the *same* module boundaries the migration already carves out — the shared
row model (`transcript_surface.rs`), scroll/anchor math (`scroll.rs`), selection/copy
(`selection.rs` + `clipboard.rs`), search (`search.rs`), and the interaction framework
(`interaction.rs`, `queue_surface.rs`, `modal.rs`). That is the whole point of front-loading the
extraction: once the hot zones are evacuated and the greenfield files exist with stable ids and copy
ranges, the ten §12 themes become **additive lanes in mostly-new files** that fan out the moment their
dependency phase lands. They never re-enter Z1/Z2, and they touch `lib.rs` only through the same cheap
additive integration points (a `mod x;` line, append-only `keymap.rs` `Action` variants, and
self-contained `handle_*` functions a per-wave Z3 owner wires in). In short: §12 is Wave 3+ greenfield
fan-out, scheduled by dependency, not a new spine.

### 8.1 Dependency table (subsection → core phase → owning module(s) → size → parallel)

| §12 subsection | Depends-on core phase(s) | Owning module(s) | Size | Parallel w/ core |
|---|---|---|---|---|
| 12.1 Interaction Polish | 1, 3, 4, 5, 7, **7B** | `interaction.rs`, `selection.rs`, `search.rs`, `clipboard.rs`; NEW `command_palette.rs`, `breadcrumb_model.rs`, `hints.rs`, `label.rs` | L | yes (7B is the gate) |
| 12.2 Reading & Comprehension | 3, 4, 5, 7 | `transcript_surface.rs`, `interaction.rs`; NEW `outline_surface.rs`, `lanes_surface.rs`, `pinned_view.rs`, `bookmarks.rs`, `annotations.rs`, `timeline_surface.rs` | L | yes |
| 12.3 Workflow Acceleration | 3, 4, 5, **7B**, §11.6 queue | `queue_surface.rs`, `interaction.rs`, `selection.rs`, `clipboard.rs`; NEW `workflow.rs`, `scratchpad.rs` | L | yes (queue lanes after §11.6) |
| 12.4 Layout Intelligence | 1, 3, 4 (+ stable row/pane ids) | `render/layout`, `render/main`, `interaction.rs`, `modal.rs` | L | yes (Wave 2.5 if focus model promoted to core) |
| 12.5 Transcript Intelligence | 3, 5, 7 | `transcript_surface.rs`, `search.rs`, `clipboard.rs`; NEW `relations.rs` | L | yes |
| 12.6 Clipboard, Paste & External Handoff | 1, 2, 3, 4, 5 | `clipboard.rs`, `selection.rs`, `search.rs`, `interaction.rs`, `transcript_surface.rs`, `terminal_guard.rs`, `queue_surface.rs`, `modal.rs` | L | yes (after Phase 5) |
| 12.7 Personalization | 1, 3, 4, **7B**, 8 | `interaction.rs`, `render` (GlyphSet); NEW `keybinding_editor`, `theme_editor`, `terminal_profiles`, `workspace_profile`, `gesture_recognizer` | XL | yes (editors W3, profiles/glyph W5) |
| 12.8 Collaboration & Multi-Agent | 2, 3, 4, 5, **7B** | `transcript_surface.rs`, `scroll.rs`, `selection.rs`, `interaction.rs`, `queue_surface.rs`; NEW `subagent_surface.rs` | L | yes |
| 12.9 Reliability & Self-Healing | 1, 3, 4, 5, 7 | `terminal_guard.rs`; NEW `health_and_recovery.rs`, `degradation.rs`, `state_checkpoint.rs`; `render/*` fallback frame | L | yes (Wave 5, after Phase 9) |
| 12.10 Measurement & Quality Gates | 3, 4, 5, 7, 8, 9, **0B** | `termsim`, `render`, `metrics`, `accessibility` | L | yes (alongside framework, once render output stable) |

**Reading the "depends-on" column:** a §12 lane is *eligible to start* the moment all listed core
phases have landed (the green gate of the wave that owns the last dependency). Phase 7B (direct
manipulation, hit-test registry, focus model, gesture recognizer) is the single most common
prerequisite — it gates 12.1, 12.3, 12.7, and 12.8 — so promoting/stabilizing 7B early is the highest-
leverage unlock for the §12 backlog, exactly as `SizeSource`/carve were for the core.

### 8.2 Wave assignment (extends §2's schedule)

§12 grafts onto the existing waves as **themed parallel lanes that light up as their dependency phase
lands** — no new serial spine. The grouping below extends §2.0's DAG; each theme is a `parallel`
fan-out over its `parallel_units` against the now-stable module boundaries, verified per-lane, then a
single Z3 integration owner per wave wires the self-contained `handle_*` fns into
`handle_key`/`handle_mouse` and `keymap.rs` (additive).

```
… (Wave 2 fullscreen core lands: row model + scroll UX + 7B foundations) …
                                       │
WAVE 3  Product completeness + §12 greenfield fan-out ───────────────────┐  (peak parallelism)
  core §3:  clipboard · selection · search · interaction · queue · modal · export · Ctrl+T
  ── as Phase 5 lands ──────────────────────────────────────────────────
  ├─ §12.6 Clipboard/Paste/Handoff   (clipboard.rs + terminal_guard editor handoff)
  ── as Phase 3/7 land ─────────────────────────────────────────────────
  ├─ §12.5 Transcript Intelligence   (transcript_surface index/folds + relations.rs + search filters)
  ├─ §12.2 Reading & Comprehension   (outline/lanes/pinned/bookmarks/annotations/timeline surfaces)
  ── as Phase 7B lands (highest-leverage §12 unlock) ───────────────────
  ├─ §12.1 Interaction Polish        (command_palette · breadcrumb · hints · label · multi-cursor)
  ├─ §12.8 Collaboration/Multi-Agent (subagent_surface timeline/compare/promote/board/attention)
  ├─ §12.3 Workflow Acceleration     (tool-action + snippet/scratchpad now; queue-themed lanes → 3B)
  ── as Phase 4 + 7B land ──────────────────────────────────────────────
  ├─ §12.4 Layout Intelligence       (adaptive density/zen/presentation now; split-solver/dock pair)
  └─ §12.7 (editors)  Keybinding · Theme · Gesture editors (after interaction framework)
        + ONE Z3 integration owner per landing wires keymap arms + handle_* fns
        gate: per-lane adversarial verify; insta snapshots; no Z1/Z2 re-entry
                                       │
WAVE 3B  Queue-themed §12 lanes (after §11.6 queue row model) ───────────┐
  └─ §12.3 queue-groups · conditional-items · templates · macros
        single Z3 owner wires all into handle_key/queue dispatch after per-lane verify
                                       │
… (Wave 4 inline deletion tail — unaffected by §12) …
                                       │
WAVE 5  Hardening + §12 reliability/measurement/profiles ────────────────┐
  ├─ §12.9 Reliability & Self-Healing (after Phase 9: watchdog · terminal-reset CLI · fallback
  │                                    layout · degraded-mode suggestions · auto-save checkpoints)
  ├─ §12.10 Measurement & Quality Gates (alongside 0B/8 term-matrix: latency budgets · benchmark
  │                                      suite · telemetry counters · visual-diff dashboard · a11y)
  └─ §12.7 (profiles) Terminal/Workspace profiles + Minimal Glyph Mode (parallel w/ perf/signals)
        gate: full term-matrix green; latency budgets met; a11y gate green; checkpoint crash-sim
                                       │
WAVE 6  Post-core polish backlog (12.2 extensions, residual L/XL items)
```

**Rationale for the grouping.** Most §12 work is greenfield layered on the Phase 3/4/5/7 modules, so
it is scheduled *by dependency*, not crammed into one wave: clipboard/handoff (§12.6) opens the moment
Phase 5's selection/copy math is stable; transcript-intelligence (§12.5) and reading (§12.2) ride the
Phase 3/7 row-model + search; interaction (§12.1) + layout (§12.4) + multi-agent (§12.8) + the
workflow/queue lanes (§12.3) light up behind Phase 4/7B; reliability (§12.9) waits for Phase 9's
signal/teardown machinery and lands in Wave 5; and measurement (§12.10) attaches to the Phase 0B/8
term-matrix and can begin **alongside** the framework as soon as render output is stable. §12.3's
queue-themed sub-lanes (groups/conditional/templates/macros) depend on the §11.6 queue row model, so
they slot into a Wave 3B sub-fan-out after §11.6, with one integration owner wiring all of them into
the queue dispatch.

### 8.3 Workflow sketch — §12 themed fan-out (runnable orchestrations)

Mirrors §6's style: a **meta** with dependency-gated phases, **`parallel`** for the greenfield theme
lanes, per-lane **verify**, and a single **Z3 integration owner** per landing. The driver
(`W3_THEMES`) fans out a theme only after its `gate` phases report green, so the script naturally
schedules each §12 theme as its dependency phase lands.

```js
export const meta = {
  name: 'tui-altscreen-w3-s12-fanout',
  description: '§12 greenfield fan-out: schedule each theme as its core dependency phase lands',
  phases: [
    { title: 'After Phase 5' }, { title: 'After Phase 3/7' },
    { title: 'After Phase 7B' }, { title: 'After Phase 4' },
    { title: 'Integrate (Z3)' }, { title: 'Verify' },
  ],
}

// theme → { gate phase, parallel_units (each becomes one greenfield lane) }
const W3_THEMES = [
  { key: '12.6-clipboard', phase: 'After Phase 5', units: [
    'clipboard.rs core: history ring buffer + OSC52 (limit/chunk) + pbcopy/wl-copy/xclip/clip.exe + temp-file provider chain, trace-testable without a live clipboard',
    'paste detection heuristics (diff/json/log/code/path) + transform preview modal (plain/quote/code/attach/queue/cancel) via interaction+modal; bracketed-paste safe',
    'large-paste staging: threshold + temp-backed store + overlay (byte/line/token estimates) + insert/quote/code/tempfile/attach/queue/split/summarize actions',
    'export request/format/destination + atomic writers + redaction pass + preview modal (text/md/json/html) over transcript_surface rows',
    'external editor handoff in terminal_guard.rs: leave alt-screen, restore modes, spawn $VISUAL/$EDITOR (POSIX+Windows PATHEXT), diff/summary re-import, signal/panic cleanup',
    'session bundle: manifest schema + isolated atomic writer + checksum + sanitization + preview modal' ] },

  { key: '12.5-transcript-intel', phase: 'After Phase 3/7', units: [
    'local index keyed by entry id/revision (normalized text/paths/commands/tools/statuses/langs/errors/health) in transcript_surface.rs; append/update/clear',
    'semantic filter parser + validation + row-visibility state in search.rs; Ctrl+T filter controls',
    'relations.rs (NEW): related-entry graph builder + relation types + confidence/provenance model',
    'duplicate-output folding: detection + raw-content retention + folded-row projection; errors stay visible',
    'error lenses: rustc/cargo/test/permission/network/panic/sandbox detectors + styled spans, structured-event preferred',
    'transcript health markers state + display (stale context, compaction, truncation, unresolved approvals, degraded caps)' ] },

  { key: '12.2-reading', phase: 'After Phase 3/7', units: [
    'outline_surface.rs: OutlineIndex extraction + rail/overlay render + click-to-jump + incremental rebuild',
    'lanes_surface.rs: per-entry lane folding (TranscriptLane/LaneId), fold state by (entry_id,lane_id), lane click targets + copy modes',
    'pinned_view.rs: PinnedViewState + split/overlay/tab layout solver + line-based clean-text diff + independent scroll/search/copy per pane',
    'bookmarks.rs: semantic anchors resilient to resize/fold/filter/resume + create/rename/delete/list/next/prev',
    'annotations.rs: Annotation CRUD + badges + modal editor + include/omit in export/search (never model-visible)',
    'timeline_surface.rs: TimelineEvent extraction + filters + rail/list render + jump-back-to-row' ] },

  { key: '12.1-interaction-polish', phase: 'After Phase 7B', units: [
    'command_palette.rs (NEW): CommandRegistry + ActionContext + universal/contextual fullscreen palette + fuzzy filter + disabled-reason + keybinding display + parameterized 2nd step',
    'breadcrumb_model.rs (NEW): BreadcrumbModel from focus/search/session/queue + jump commands + keyboard traversal + middle-truncation + hit-test rects',
    'selection.rs multi-cursor: SelectionSet keyed by entry/row ids + add/remove/merge/order + extended copy/export formatters; logical across resize/fold/filter',
    'hover intent + preview: HoverIntentState + delay timer + gesture suppression during scroll/drag/selection + visual-only render (no layout change)',
    'double-click activation: PointerActivationPolicy per hit-target kind + click-count tracking via gesture recognizer; keyboard parity (Enter/Space)',
    'label.rs (NEW): LabelTargetId + InlineEditState (reuse composer primitives) + session-only metadata; surface in breadcrumbs/rails/search/exports',
    'hints.rs (NEW): HintEngine trigger tracking + dismissal/cooldown/max-display + toast/status surface',
    'platform pointer shapes: capability-gated cursor-shape adapter with terminal-aware degradation' ] },

  { key: '12.8-collab', phase: 'After Phase 7B', units: [
    'subagent timeline panel: extend SubagentRecord (status/activity/elapsed/tool-count/cost/attention) + route events + app-owned scroll render',
    'hover/click: hit-test targets + SubagentActivationTarget + focus/scroll anchors + jump+back command + semantic style tokens',
    'compare outputs: SubagentCompareState + per-subagent cache + tabs/cols layout solver + independent scroll per pane + copy/quote/promote',
    'promote result: command + plain-text projection + source metadata + composer fill (idle) / queue (active turn), never auto-submit',
    'live review board: queued/running/reviewing/blocked/completed lane classification + board layout + id routing + cap/rejection visibility',
    'attention routing: SubagentAttentionKind quiet/warning/error classification + status/toast/title routing + log/eval preservation' ] },

  { key: '12.3-workflow', phase: 'After Phase 7B', units: [
    'workflow.rs (NEW): tool-action dispatcher (retry/edit/copy/open/jump) + command/cwd/exit/approval/sandbox metadata preservation + path/location parsers',
    'scratchpad.rs (NEW): session-scoped buffer + append-quote/insert-link/send-to-composer/queue/export/clear + persistence',
    'snippet builders in selection.rs: selection→quote/queue-item/bug-note/plan-refinement with provenance/source-link' ] },

  { key: '12.4-layout', phase: 'After Phase 4', units: [
    'adaptive density: policy+resolver selecting compact/default/expanded, preserving scroll anchor/selection/focus/queue/composer',
    'zen mode: layout-suppression policy keeping search/copy/queue/error/help reachable; toggle restores prior visibility',
    'presentation mode: spacious cards + reduced/redacted metadata + one-shot reveal + non-mutating display policy',
    'smart split panes + layout solver (interdependent pair): place side panes only when width permits, degrade to stacked/modal/inline',
    'focus-preserving resize: logical id resolution so focus/selection/scroll/drag/active-action survive split↔stacked↔modal↔compact↔expanded',
    'dockable panels: panel state machine (task/queue/diff/outline/diagnostics/clipboard) + tabbed containers + persisted prefs (depends on palette/modal)' ] },

  { key: '12.7-editors', phase: 'After Phase 7B', units: [
    'keybinding_editor (NEW): CommandId registry + layered KeymapResolver + key/chord capture + conflict detection + delta-only persistence',
    'theme_editor (NEW): semantic Theme tokens + live preview + truecolor/256/no-color palette mapping + GlyphSet separation + versioned storage',
    'gesture_recognizer (NEW): centralized recognizer + double/triple/Shift-click/drag/wheel/hover/edge-scroll tuning + per-surface bindings + reduced-motion + test panel',
    'shared modal infra for both editors (centered-block surface, reuse modal.rs)' ] },
]

// One generative lane per parallel_unit; one adversarial verify per lane; one Z3 owner per landing.
for (const theme of W3_THEMES) {
  phase(theme.phase)
  const lanes = await parallel(theme.units.map((u, i) => () =>
    agent(`Build §${theme.key} lane ${i + 1}: ${u}\n`
      + `Deliver the module/struct + its own unit tests + a self-contained handle_* fn for the Z3 owner to wire. `
      + `Do NOT edit handle_key or re-enter the RENDER (Z1) / GUARD (Z2) zones. Return files changed + how to verify.`,
      { label: `s12:${theme.key}:${i + 1}`, phase: theme.phase })))
  await parallel(lanes.filter(Boolean).map((_, i) => () =>
    agent(`Adversarially verify §${theme.key} lane ${i + 1}: compiles standalone, tests pass, `
      + `range/copy/search/anchor math correct on wrapped+wide-glyph+folded rows; logical ids survive resize/fold/filter; `
      + `no keyboard op lacks a mouse equivalent and vice-versa. Report defects.`,
      { label: `s12:verify:${theme.key}:${i + 1}`, phase: 'Verify' })))
}

// Single Z3 integration owner: additive keymap arms + wire each handle_* fn, build green, snapshots.
phase('Integrate (Z3)')
await agent('Z3 integration owner for the landed §12 themes: add append-only keymap.rs Action variants and '
  + 'wire each lane\'s self-contained handle_* fn into handle_key/handle_mouse + TuiApp struct fields, single pass. '
  + 'cargo build+test -p squeezy-tui AND -p squeezy-eval green; refresh insta snapshots.',
  { label: 's12:integrate', phase: 'Integrate (Z3)' })
```

Queue-themed §12.3 (groups, conditional items, templates, macros) and the Wave-5 themes (§12.9
reliability, §12.10 measurement, §12.7 profiles + Minimal Glyph Mode) follow the identical
shape — `parallel` over `parallel_units`, verify-per-lane, single integration owner — but gated on
§11.6 (queue row model) and Phase 9 / Phase 0B-8 respectively, so they run as a Wave 3B sub-fan-out
and inside the Wave 5 `tui-altscreen-w5-harden` workflow rather than the Wave 3 driver above.
