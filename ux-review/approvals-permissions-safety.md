# Approvals, Permissions & Safety

> How Squeezy gates tool calls — the per-capability approval prompt, the structural pre-classifier and AI reviewer behind it, Plan-mode mutation locks, and the safety-sensitive overlays (tool actions, diff/detail pane, review board).

**How it works today:** A shell/edit/web/mcp tool request flows through a policy verdict, then a structural `pre_classify_shell` pass (raises permissive verdicts to an Ask floor for dangerous interpreters / sensitive paths), then an optional async AI reviewer (auto-allow ceilings, consecutive/recent-denial circuit breakers, a 50-entry audit ring), and only then reaches the user as a modular approval block — a risk-colored header, a `Why:` rationale, the capability subject, a `Rule:` preview, and a three-option menu (Approve once / Always allow / Deny). Plan mode returns an explained, non-silent Deny for mutating tools that names the recovery (`switch to Build mode (Shift+Tab)`), and the active mode is always shown in the status bar. The reviewer audit is reachable via `/reviewer` and a line in `/tasks`; safety-sensitive overlays (Tool actions, diff/detail pane, Live Review Board) deliberately degrade to copy/jump and never bypass a gate. The system is well-architected for safety; the gaps are about *surfacing* decisions the gates already make — persistence scope, combined denial reasoning, circuit-breaker trips, and a few honest empty/narrow states.

## Quick wins

- [Approval keybind hint buries the primary action under slash-aliases](#1-approval-keybind-hint-buries-the-primary-action-under-slash-aliases)
- [`Rule:` line never says the rule persists across sessions](#2-rule-line-never-says-the-rule-persists-across-sessions)
- [Approval shows no `Why:` line — and no honest placeholder — when context is absent](#5-approval-shows-no-why-line-and-no-honest-placeholder-when-context-is-absent)
- [Truncated approval diff tail offers no way to see the rest](#7-truncated-approval-diff-tail-offers-no-way-to-see-the-rest)
- [Detail pane reports "opened" on terminals too narrow to render it](#8-detail-pane-reports-opened-on-terminals-too-narrow-to-render-it)

## Findings

### 1. Approval keybind hint buries the primary action under slash-aliases

- **Category · Severity · Effort:** Discoverability · Medium · S
- **Today:** The footer reads `Up/Down choose · Enter/Y approve once · A/P always approve repo · N/D deny · Esc cancel`. Every action is a slash-joined alias pair (`Enter/Y`, `A/P`, `N/D`), so the one key a first-timer needs — `Enter` to approve once — has equal visual weight to muscle-memory aliases (`P`, `D`) the module's own doc-comment calls "silent aliases kept for compatibility."
- **Friction:** On a minimal terminal with little color, the comma/slash run blurs into one string; the primary path is not distinguishable from the secondary keys. The footer also says "approve repo" while the menu option labels say "Always allow …" — two vocabularies for one action.
- **Polish:** Drop the silent aliases from the visible hint and lead with the primary verb: `Enter approve once · A always allow · N deny · Esc cancel`. Keep `P`/`D` bound but unadvertised (as the doc-comment already intends), and align the footer verb with the menu's "allow" wording.
- **Refs:** `crates/squeezy-tui/src/approval.rs:7-9`, `crates/squeezy-tui/src/lib.rs:24595-24596`

### 2. `Rule:` line never says the rule persists across sessions

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** The preview shows a `Rule:` line (e.g. `command prefix cargo`, `network docs.rs`) that "Always allow" will write to `.squeezy/permissions.toml`. The "Always allow …" *option label* names the scope target (and even appends a Windows broad-rule warning), but neither the `Rule:` line nor the option says the rule is **persisted to disk and applies to every future matching request in this project**, not just this once.
- **Friction:** A user who picks "Always allow command cargo" to clear one prompt has no on-screen signal that they just authored a durable, file-backed rule with project-wide reach. The scope/duration only becomes visible if they later open `permissions.toml`.
- **Polish:** Append a one-line dim note under `Rule:` when the project option is offered: `(saved to .squeezy/permissions.toml — applies to all matching requests in this project)`. The persistence is real (`persist_permission_rule` writes a `[[permissions.rules]]` block), so the note is honest, not aspirational.
- **Refs:** `crates/squeezy-tui/src/approval.rs:350-369`, `crates/squeezy-agent/src/permission_persist.rs:13-61`

### 3. Circuit-breaker trips happen silently — no proactive surface

- **Category · Severity · Effort:** Feedback · Medium · M
- **Today:** The AI reviewer trips a per-turn circuit breaker after consecutive (`CONSECUTIVE_DENIAL_TRIP = 2`) or windowed (`RECENT_DENIAL_TRIP = 5`) denials, after which it stops auto-deciding for the turn. The trip is recorded into the audit ring as `CircuitTripped`, but the only way to learn it happened is to *run* `/reviewer` or read the `reviewer …` line in `/tasks` — there is no toast, banner, or status marker when a breaker flips mid-turn.
- **Friction:** A user whose turn quietly degrades (the reviewer stops helping after a denial spike) gets no signal that the safety net changed state; they only see individual requests now reaching them, with no "the reviewer backed off because it denied N in a row" context. Unlike the explained per-request deny, the *systemic* event is invisible at the moment it matters.
- **Polish:** Emit a one-shot status/toast on the first trip of a turn, e.g. `AI reviewer paused: N denials this turn — requests now reach you directly`. The audit ring already carries the trip reason, so the message can reuse it verbatim. Keep it one-shot per turn so it never repeats per blocked request.
- **Refs:** `crates/squeezy-agent/src/ai_reviewer.rs:74-77,384-395`, `crates/squeezy-tui/src/lib.rs:21427-21437`

### 4. A pre-classifier denial and a reviewer denial collapse to one reason

- **Category · Severity · Effort:** Feedback · Medium · M
- **Today:** `pre_classify_shell` tightens a permissive verdict to an Ask floor with a structural reason (`pre-classifier requires approval: dangerous interpreter "sudo"`). That Ask then flows to the AI reviewer; if the reviewer denies, its verdict — `AI reviewer denied: …` — fully *replaces* the tightened verdict (the `reason` field is overwritten). The structural reason is written to the session log but never reaches the user-facing deny detail, which only shows the reviewer's text.
- **Friction:** A user denied `sudo` cannot tell whether `sudo` is structurally dangerous (the pre-classifier raised the gate) or whether the reviewer weighed context and declined — two different remediations (rephrase vs. argue the context). The decision tree that produced the deny is flattened to its last node.
- **Polish:** When a request was raised by the pre-classifier *and* denied by the reviewer, thread the structural reason into the final verdict: `dangerous interpreter "sudo" (pre-classified) · reviewer agreed: {reason}`. The pre-classifier reason is already computed at the tightening site — carry it forward rather than only logging it.
- **Refs:** `crates/squeezy-tools/src/safety.rs:39-78`, `crates/squeezy-agent/src/lib.rs:15418-15452`, `crates/squeezy-agent/src/ai_reviewer.rs:397-414`

### 5. Approval shows no `Why:` line — and no honest placeholder — when context is absent

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `ToolApprovalRequest.context` is a head-truncated snippet of the latest assistant message, and is `None` on the first turn or in subagent contexts with no transcript. When it is `None` (or whitespace), `render_preview_parts` skips `append_context` entirely, so the block jumps from the risk header straight to the `$ command` / `✎ path` subject with no `Why:` row at all.
- **Friction:** The preview's shape silently changes between requests — sometimes there's a rationale, sometimes the row vanishes — so a user can't tell "no rationale was provided" from "I missed it." A rare, deliberate approval moment benefits from a predictable layout.
- **Polish:** When context is absent, emit a single dim `Why: (no rationale provided)` row so the block's structure is stable and the absence is stated, not implied. Cheap: one branch in `render_preview_parts` where `append_context` is skipped.
- **Refs:** `crates/squeezy-tui/src/approval.rs:42-45,84-89`, `crates/squeezy-agent/src/lib.rs:18058-18062`

### 6. Two Windows sandbox warnings say nearly the same thing in two different colors

- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** The shell preview can paint the same posture concern twice with different styling. The `windows_sandbox_posture` block renders `Windows: no filesystem/network sandbox; approval is the enforcement boundary` in **red+bold** (lines 218-226), while the `filesystem` metadata branch renders the near-identical `Windows: no filesystem/network isolation; process tree will be killed on timeout/cancel` via `warn_line` in **cyan+bold** (lines 240-249). Routine sandbox posture below them is dim. So one "your reads/network aren't isolated" message is red and a sibling is cyan.
- **Friction:** A user scanning for "the warning that matters" sees two severities for one underlying fact; the cyan/red split implies a hierarchy that isn't real. On a narrow terminal where both truncate, the inconsistency makes it harder to trust either.
- **Polish:** Pick one tier for "posture that leaves approval as the boundary" — red+bold reads as the genuine caution — and route both the `windows_sandbox_posture` text and the `filesystem` write-isolation warnings through the same styled helper. Keep the dim tier for informational posture (backend/mode/network-policy) only.
- **Refs:** `crates/squeezy-tui/src/approval.rs:209-252,404-414`

### 7. Truncated approval diff tail offers no way to see the rest

- **Category · Severity · Effort:** Discoverability · Low · S
- **Today:** An edit approval renders at most `APPROVAL_DIFF_BODY_CAP = 18` diff lines, then a dim `… (N more lines)` tail. The module's own comment says reviewers "can still see the full patch via `/diff` once the call lands," but the rendered tail never says so — it states the count and stops.
- **Friction:** A user approving a large edit sees a clipped diff and a bare `… (12 more lines)` with no path to the rest *before* deciding. They either approve blind or cancel to inspect, when a one-token hint would let them widen or jump to the full patch.
- **Polish:** Make the tail carry the recovery verb the comment already documents: `… (12 more lines — full diff via /diff)`. Pure string change at the existing truncation site; no new state.
- **Refs:** `crates/squeezy-tui/src/approval.rs:18-22,280-296`

### 8. Detail pane reports "opened" on terminals too narrow to render it

- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** Pressing `d` in the transcript overlay sets `diff_detail_pane = Some(..)` and unconditionally sets the status to `detail pane opened (Shift+↑/↓ to scroll · d/Esc to close)`. The width check lives later in `split_overlay_content`, which returns `None` below `MIN_SPLIT_WIDTH = 64` and paints nothing. So on a 60-column terminal the user is *told* the pane opened, told how to scroll it, and sees no pane.
- **Friction:** The confident "opened" status directly contradicts the empty screen — worse than a silent no-op, because it sends the user looking for a pane that was never drawn and offering scroll keys that do nothing.
- **Polish:** Gate the open on the same width threshold the renderer uses (the overlay content width is known at toggle time), and when it's too narrow report it honestly: `detail pane needs 64+ columns — widen the terminal`. Keep the pane closed rather than opening an invisible one.
- **Refs:** `crates/squeezy-tui/src/lib.rs:18549-18552`, `crates/squeezy-tui/src/diff_detail_pane.rs:23-27,78-81`

### 9. Review-board lanes carry no inline gloss for Blocked vs. Capped

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The Live Review Board labels four lanes `Running`, `Blocked`, `Capped`, `Completed` (ASCII-only, so meaning never depends on color). `Blocked` means *ran and failed*; `Capped` means *refused before start because the concurrency cap was hit* — a real distinction the module documents in source but never surfaces on the board.
- **Friction:** Both `Blocked` and `Capped` read as "failure" to a new user, but the remediation differs — a capped worker can simply be retried once capacity frees up, while a blocked one failed and needs inspection. The labels alone don't convey which is which.
- **Polish:** When a lane is focused, show a one-line gloss in the board's status/footer: `Blocked — ran and failed` / `Capped — refused before start (concurrency cap)`. The classification already maps each real lifecycle status to its lane, so the gloss is a fixed string per lane, not new state.
- **Refs:** `crates/squeezy-tui/src/review_board.rs:70-121`
