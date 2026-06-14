# Prompt Queue & Subagents

> The reorderable prompt queue that drains as turns finish — with conditions, groups, and multi-select layered on it — plus the subagent timeline, hover-preview, side-by-side compare, and promote-to-queue affordances.

**How it works today:** While a turn runs, Enter/paste push composer text onto a queue that drains one item per turn-finish; the `Ctrl+X Q` overlay lets the user reorder, tag (multi-select), group (collapse/pause/dissolve), attach run-conditions, edit, or "run next" each item, all keyed by stable per-item ids so a concurrent drain never misaddresses a row. Conditions are evaluated by a pure drain pump (`plan_drain`) that runs, drops (skip), or parks (blocked/paused) the front item, emitting a status line and an undo record on a skip. Subagents surface in a separate timeline panel with status labels, a quiet hover-preview popover, a two-slot side-by-side compare view (which already names each pane's subagent), and a promote verb that distills a result into reviewed composer/queue text with a confirming status flash. The pure-state modules are well-factored and consistently honest; the gaps are narrow render-time polish — concentrated in the overlay's hint strings, the condition/group markers' reliance on color, and a couple of terse status labels.

## Quick wins
- [Condition markers are unreadable on a no-color terminal](#1-condition-markers-are-unreadable-on-a-no-color-terminal)
- [The queue overlay teaches two different keymaps for the same keys](#2-the-queue-overlay-teaches-two-different-keymaps-for-the-same-keys)
- [Empty-queue overlay still shows list-navigation keys](#3-empty-queue-overlay-still-shows-list-navigation-keys)
- [A multi-run condition-skip only flashes the last item's count](#4-a-multi-run-condition-skip-only-flashes-the-last-items-count)
- [`capped` is unexplained jargon on the subagent timeline](#5-capped-is-unexplained-jargon-on-the-subagent-timeline)

## Findings

### 1. Condition markers are unreadable on a no-color terminal
- **Category · Severity · Effort:** Accessibility · High · M
- **Today:** A conditional queue row gets a glyph — `[✓]` succeeded, `[✗]` failed, `[±]` edited, `[=]` no-edits, `[⏷]` manual — tinted by its evaluation against the last outcome: accent for runnable, warn for skip-bound, quiet for blocked (`queue_conditions.rs:95-104`, `326-344`). Under `NO_COLOR`/a monochrome terminal, `palette::best_color_for_detected_level` maps every theme color to `Color::Reset` (`render/palette.rs:46`), so accent, warn, and quiet collapse to one indistinguishable foreground.
- **Friction:** The runnable/skip-bound/blocked distinction — the whole point of "hidden automation never silently runs the wrong prompt" — vanishes, leaving only the glyph. `[✓]`/`[✗]` survive on intuition, but `[±]`, `[=]`, and `[⏷]` are cryptic without their color, so a no-color user cannot tell an "edited-files" gate from a "no-edits" gate, or a blocked one from a runnable one.
- **Polish:** Key off `palette::color_level() == NoColor` and either append a one-char ASCII state suffix the color otherwise carried (`[±]+`, `[±]-` to mark runnable vs skip-bound) or fall back to self-describing ASCII glyphs (`[y]`/`[n]`/`[*]`/`[ ]`/`[m]`). Keep the compact colored glyphs on color terminals.
- **Refs:** `queue_conditions.rs:95-104` (`marker_glyph`), `queue_conditions.rs:326-344` (`condition_marker_span`), `render/palette.rs:46` (NoColor → Reset)

### 2. The queue overlay teaches two different keymaps for the same keys
- **Category · Severity · Effort:** Consistency · Medium · S
- **Today:** Opening the overlay sets a status-line cheatsheet: `↑↓ focus · Space tag · g group · z fold · p pause · Enter/e edit · r run next · Del remove · Esc close` (`lib.rs:17723-17728`). The overlay header paints its own, different cheatsheet: `↑↓ select · Space tag · g group · v cond · Shift+↑↓ reorder · Enter/e edit · r run next · Del · Esc` (`prompt_queue.rs:204-211`). The two disagree on the verb (`focus` vs `select`), on which verbs appear (`v cond`, `Shift+↑↓ reorder`, `m merge`, `c clear` are header-only; `z fold`/`p pause` are status-only), and on ordering.
- **Friction:** Both are on screen for the same overlay at the same time, so a user reading either to learn the keys gets a partial, inconsistent map — the status line never mentions `v cond` exists, while the header never mentions `z fold`/`p pause`. There is no single authoritative key legend.
- **Polish:** Derive both strings from one source, or drop the redundant status-line legend (the header already paints a state-aware one) and have the open-status just announce `prompt queue (N queued)`. One legend, one truth.
- **Refs:** `prompt_queue.rs:204-211` (header hint), `lib.rs:17723-17728` (overlay-open status hint)

### 3. Empty-queue overlay still shows list-navigation keys
- **Category · Severity · Effort:** Discoverability · Medium · S
- **Today:** `render_lines` always paints the header with the navigation cheatsheet (`↑↓ select · Space tag · …`), then, when the queue is empty, pushes a single dimmed `(queue is empty)` body line and returns (`prompt_queue.rs:204-228`). The keybinding hint describes acting on a list that has no rows.
- **Friction:** A user who opens an empty queue overlay sees keys for selecting, tagging, grouping, and reordering items that do not exist — nothing explains that the queue fills only while a turn runs, so the overlay reads as broken or mid-load.
- **Polish:** When the queue is empty, replace the row cheatsheet in the header with a contextual line, e.g. `queue fills as you Enter prompts while a turn runs · Esc close`. Keep the full cheatsheet only when there are rows to act on.
- **Refs:** `prompt_queue.rs:204-212` (header hint, unconditional), `prompt_queue.rs:223-228` (empty-queue body)

### 4. A multi-run condition-skip only flashes the last item's count
- **Category · Severity · Effort:** Feedback · Medium · S
- **Today:** When the drain pump skips a conditional item it removes the row, records an undo entry, pushes a per-item line to the activity log (`skipped queued prompt (condition not met): <preview>`), and sets the status line to `skipped queued prompt (N left)` — then *re-plans against the shorter queue and loops* (`lib.rs:23353-23377`). Each loop iteration overwrites `app.status`, so after a run of skips the user sees only the final `(N left)` flash; the per-item previews went to the log, not the status line.
- **Friction:** Skipping is not silent (the draft's premise — no message, no undo — is wrong), but a *batch* skip reads as a single ambiguous count: the queue drops from 5 to 2 with one terse status and no on-screen note of *which* items or *which* conditions cleared them. The user has to open the log to reconstruct what happened.
- **Polish:** Accumulate the skip count/previews across the drain loop and flash one summary at the end — `skipped 3 conditional items (2 if-prev-succeeded, 1 if-prev-no-edits)` — instead of letting each iteration clobber the status with a bare count.
- **Refs:** `lib.rs:23353-23377` (Drop arm: per-iteration status + log + undo), `queue_conditions.rs:304-317` (`plan_drain` one-step-at-a-time contract)

### 5. `capped` is unexplained jargon on the subagent timeline
- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** A subagent refused before it ran (the concurrency cap was hit) renders the status label `capped` on the timeline (`subagent_timeline.rs:78-85`). The same word is reused for an unrelated bound — the two-slot mark cap in the compare view ("cap visible columns", `subagent_compare.rs:48-57`) — and the promote model labels the same lifecycle `capped` too (`subagent_promote.rs:92-99`).
- **Friction:** `capped` assumes the reader knows the concurrency-cap concept; on its own a `capped` row reads as cryptic. Reusing the word for the compare mark-cap overloads it, so the same term means "this agent never ran" in one surface and "you can only mark two" in another.
- **Polish:** Relabel the *timeline status* to `rejected` (it is honest about what happened to the agent and matches the internal `SubagentTimelineStatus::Rejected` enum), reserving "cap" for the compare two-slot bound. Or keep `capped` but add a quiet footer when a capped row is present: `capped = concurrency limit reached (agent not run)`.
- **Refs:** `subagent_timeline.rs:78-85` (`label`: `Rejected => "capped"`), `subagent_compare.rs:48-57` (two-slot "cap" doc), `subagent_promote.rs:92-99` (`PromoteStatus::Capped` label)

### 6. Paused-group marker borrows the warn (error) color
- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** A paused group's row marker is `[P]` painted in the warn color, the same red/orange used for skip-bound conditions and error chrome (`queue_groups.rs:235-246`). Pausing is a deliberate user action ("hold this batch"), not a failure. (The module doc above the function even describes a `⏸`/`▸` glyph the code does not paint — a stale comment.)
- **Friction:** On a color terminal, a held-back batch reads as "these items are in trouble" rather than "you parked these." The warn color is doing double duty for "error/about-to-skip" and "deliberately held," muddying the at-a-glance scan. (On a no-color terminal the colors collapse anyway, so the `[P]`-vs-`[G]` letter is what distinguishes them — which is fine.)
- **Polish:** Paint `[P]` in a held-not-broken color (quiet/dim, or the group accent) and reserve warn for genuinely attention-worthy state. Optionally swap the glyph to a pause symbol so the letter `P` does not read as "problem." Update the stale `⏸`/`▸` doc to match.
- **Refs:** `queue_groups.rs:224-246` (`group_marker_glyph` / `group_marker_span`; doc claims `⏸`/`▸`, code paints `[P]`/`[G]`)

### 7. `g group` hint verb does not say what the key does
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** Every overlay hint variant lists `g group` (`prompt_queue.rs:204-211`). `group` reads as a noun; the key actually *forms* a group from the tagged/selected rows (`create_group`, `queue_groups.rs:117-139`). The hint does not distinguish create from dissolve (`G dissolve`), pause (`p`), or fold (`z`).
- **Friction:** A user reading `g group` cannot tell whether `g` creates, edits, or enters a group mode — especially when multi-select is active and `g` would form a group from the tagged rows. The companion verbs (`z fold`, `p pause`, `G dissolve`) are clearer because they name an action; `g group` names a thing.
- **Polish:** Rename to an action verb consistent with its siblings — `g form group` (or `g +group` against `G -group` for dissolve) — so the create semantics are explicit.
- **Refs:** `prompt_queue.rs:204-211` (hint), `queue_groups.rs:117-139` (`create_group`)

### 8. When multi-select is active, the `v cond` affordance disappears from the hint
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The header hint is a four-way exclusive `if/else if`: `group_active` (a tag exists) → multi-select cheatsheet, else `any_group` → group cheatsheet, else `any_condition` → condition cheatsheet, else the base (`prompt_queue.rs:204-211`). The `group_active` string is `Space tag · g group · Del delete group · Shift+↑↓ move group · m merge · c clear` — it omits `v cond` and never acknowledges that some rows carry conditions.
- **Friction:** These states are not mutually exclusive in the data — a queue can have tagged rows *and* conditional rows at once — but the hint treats them as exclusive, so a user mid-multi-select loses any on-screen pointer that `v` edits conditions or that some rows are gated.
- **Polish:** Either keep `v cond` in the multi-select hint, or, when two orthogonal features are active, append a short note (`· some rows conditional (v to edit)`) so the more correctness-relevant feature (conditions) is never hidden by the more transient one (tags).
- **Refs:** `prompt_queue.rs:201-211` (`group_active`/`any_group`/`any_condition` exclusivity)

### 9. Unconditional rows render a blank marker column, reading as "unset"
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** An `Always` (unconditional) row paints three spaces where its condition glyph would go (`queue_conditions.rs:95-97`), to keep columns aligned with the always-present multi-select `[ ]`/`[x]` box (`prompt_queue_multiselect.rs:169-170`) and the group marker (`prompt_queue.rs:266-269`). In a mixed queue, conditional rows show `[✓]`/`[±]` while unconditional ones show a gap.
- **Friction:** The blank column reads as missing data, not "always-run" — a learner scanning a mixed queue sees some rows with a glyph and others with a hole and may assume the holes are incomplete. The padding (an alignment detail) leaks into the mental model.
- **Polish:** Render unconditional rows with a quiet `[·]`/`[ ]` placeholder (mirroring the multi-select empty box) so every row shows a consistent, complete 3-cell condition column that reads as "set to always-run," not blank.
- **Refs:** `queue_conditions.rs:95-104` (`marker_glyph`, `Always => "   "`), `prompt_queue.rs:266-269` (marker span order)

### 10. Indicator hint text swaps on turn start
- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** The one-line queue strip above the composer shows `Ctrl+X Q to reorder` when idle and `Ctrl+X Q to reorder · Esc cancels current (queue keeps draining)` while a turn runs (`prompt_queue.rs:357-363`). The strip is rebuilt each frame and the hint text changes length when a turn starts/ends. (The `queued: N` count is prepended *before* the hint, so it does not shift horizontally — the draft's count-shift claim does not hold.)
- **Friction:** The trailing hint text appearing/disappearing on turn boundaries is a small visual change in a strip meant to read as settled; a user re-reading the keybinding sees the line grow a clause.
- **Polish:** Keep the keybinding stable and render the `Esc cancels …` clause in a quiet color appended in both states (dimmed when idle), so the line's structure is constant and only the emphasis changes — or drop the running-state clause entirely since `Esc` already has its own affordance.
- **Refs:** `prompt_queue.rs:357-363` (`indicator_line` hint branch), `prompt_queue.rs:364-379` (count prepended before hint)

### 11. Collapsed-group member count is buried in the row body
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** A collapsed group renders its header row as `{n}. ⊟ {name} ({count})` in one uniform style, then truncates the whole body to fit the row width left of the delete glyph (`prompt_queue.rs:252-256`, `283`). The `(count)` carries the same color/weight as the name and sits at the right end of a width-budgeted string.
- **Friction:** The member count is the most useful at-a-glance fact about a collapsed group, but it reads as a parenthetical afterthought and is the first thing truncation eats when the group name is long — so on a narrow overlay a long-named group can lose its count entirely.
- **Polish:** Render the count as a distinct, protected token — `{name} ×{count}` or the count in accent/bold ahead of the truncatable name — so the count is a first-class label that survives truncation rather than parenthetical tail text.
- **Refs:** `prompt_queue.rs:252-256` (collapsed-group body format), `prompt_queue.rs:283` (body truncated to budget)

## Dropped from the draft (verified inaccurate)

- **"Skipped conditions vanish silently with no feedback."** False: the Drop arm flashes a status line *and* pushes an undo record *and* logs each item (`lib.rs:23353-23377`). The real residual gap (batch skips overwrite the status) is captured as finding #4.
- **"Promote destination is silent."** False: `promote_subagent_at_index` flashes `promoted <name> — queued (N in queue)` / `— filled composer, review and submit` naming the destination (`lib.rs:13325-13337`).
- **"Pinned compare pane titles do not name the subagents."** False: the pane title is `{role}: {agent #N [status]}` via `subagent_compare_attribution` (`lib.rs:30240-30301`); each pane already names its subagent and run state.
- **"Edit queue item gives no confirmation."** False: `save_queue_edit` flashes `updated queued prompt (N queued)` (`lib.rs:17789-17795`), and the overlay re-renders from the live `prompt_queue` each frame, so a re-opened overlay shows the new text — it is never stale.
