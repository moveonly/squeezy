# Cost, Status & Live Feedback

> How Squeezy surfaces spend, cost-cap pressure, latency, and ambient status across the status line, transcript, toasts, and off-tab desktop notifications.

**How it works today:** A configurable status line (default: provider/model, dir, languages, branch, PR, branch-changes, cost-with-cap-%, cache-hit) renders session-cumulative spend live, refreshed mid-turn from `AgentEvent::CostUpdate`/`Completed` snapshots that are canonicalized to the same micros the cost broker enforces against. The cost broker fires three cap signals — a one-shot warning at `cost_warn_percent`, a pressure gate at 80%, and a hard cap — all of which land in the transcript as system/error notices (the pressure gate and hard cap as red turn-failed banners). Desktop notifications (OSC 9 / BEL, opt-in, default off) fire only on turn-complete and approval-pending. Render-metrics and per-interaction latency overlays exist but are gated behind deliberately obscure `Ctrl+Alt` debug chords with no slash-command or help entry point.

## Quick wins

- [Cost-cap warning and pressure-gate events never reach the off-tab desktop surface](#1-cost-cap-warning-and-pressure-gate-events-never-reach-the-off-tab-desktop-surface)
- [No in-UI affordance points users at `/statusline` customization](#4-no-in-ui-affordance-points-users-at-statusline-customization)
- [Cost segment shows a spend percentage next to `⚠ unpriced`, which is self-contradictory](#3-cost-segment-shows-a-spend-percentage-next-to--unpriced-which-is-self-contradictory)
- [`budget denied:N` reads as past tense for a counter that is actually current-turn-only](#5-budget-deniedn-reads-as-past-tense-for-a-current-turn-only-counter)

## Findings

### 1. Cost-cap warning and pressure-gate events never reach the off-tab desktop surface

- **Category · Severity · Effort:** Feedback · High · M
- **Today:** `DesktopNotifier` (OSC 9 / BEL) fires on exactly two events — turn-complete and approval-pending. Cost-cap crossings, the 80% pressure gate, and the hard-cap stop are surfaced *only* in the transcript (the gate and hard cap as red `push_error` turn-failed banners). A user watching a long turn in another tab gets pinged when it finishes, but not when spending silently blocks the next round.
- **Friction:** The pressure gate and hard cap end the turn with `AgentEvent::Failed`; if the user has tab-switched away, the terminal stays quiet and they return to a stalled session with no idea a cost ceiling stopped it. The notification plumbing already exists and is the correct surface for exactly this off-tab attention case, but it is underused.
- **Polish:** Wire `DesktopNotifier::notify` into the `CostWarning` and `Failed`-with-pressure/cap paths in `events.rs`, e.g. "squeezy cost warning: 75% of cap" and "squeezy paused: approaching cost cap". Keep it gated by the same `[tui].desktop_notifications` opt-in as turn-complete so a default install stays silent.
- **Refs:** `crates/squeezy-tui/src/notification.rs:50-79`, `crates/squeezy-tui/src/lib.rs:46383-46393`, `crates/squeezy-tui/src/events.rs:426-439`, `crates/squeezy-agent/src/lib.rs:7553-7577`

### 2. Latency-budget violations and the render HUD are reachable only via obscure debug chords

- **Category · Severity · Effort:** Discoverability · High · M
- **Today:** Two diagnostic overlays exist — `RenderMetrics` (frame time, bytes, rows, cache hit-rate, wrap) and `LatencyTracker` (per-interaction p95/p99 vs. budget, last violation). Both are toggled only by `Ctrl+Alt+L` / `Ctrl+Alt+M` (documented in-source as "deliberately obscure debug chord") or env vars. There is no slash command and no help entry; `/statusline` exists as a `DispatchCommand` but no equivalent `/metrics` does.
- **Friction:** A user feeling keystroke lag or scroll jank has no discoverable way to confirm Squeezy noticed — the tracker records the violation (`LastViolation`) but it is buried in an overlay the user cannot find. The `Ctrl+Alt` chords are also the classically unreliable Meta encoding over tmux/SSH, so even a tipped-off user may not be able to trigger them.
- **Polish:** Add a `/metrics` slash command that toggles the HUD (and its latency panel) from the prompt, mirroring how `/statusline` opens the picker. Optionally surface a compact `⚡ slow` status-line marker when `last_violation` is recent (< 10s) so a lagging session self-announces without forcing the overlay open.
- **Refs:** `crates/squeezy-tui/src/latency.rs:238-273`, `crates/squeezy-tui/src/metrics.rs:84-122`, `crates/squeezy-tui/src/keymap.rs:1291-1310`, `crates/squeezy-tui/src/lib.rs:19804`

### 3. Cost segment shows a spend percentage next to `⚠ unpriced`, which is self-contradictory

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** When a cap is configured but the active model is unpriced, the cost segment renders e.g. `cost $0.00 / $10.00 (0.0%) ⚠ unpriced`. The percentage is real (it is the cap-basis spend) but the `unpriced` flag means the cap *cannot advance* for this model, so the `0.0%` reads as "you are nowhere near your cap" exactly when the cap is inert and offering no protection.
- **Friction:** The two halves of the segment tell opposite stories: the percent implies "plenty of headroom", the suffix implies "this guardrail is off". A user glancing at `(0.0%) ⚠ unpriced` is more likely to read reassurance than the intended warning.
- **Polish:** When `cap_unenforceable` is set, suppress the `(N.N%)` term (since it cannot move) and render the cap as inert, e.g. `cost $0.00 / $10.00 (cap inert: unpriced model)`, so the percent never contradicts the warning. The flag already self-clears on the next priced `CostUpdate` (`events.rs:505-507`), so this only affects the genuinely-unpriced window.
- **Refs:** `crates/squeezy-tui/src/status.rs:663-688`, `crates/squeezy-tui/src/events.rs:430-439,505-507`

### 4. No in-UI affordance points users at `/statusline` customization

- **Category · Severity · Effort:** Discoverability · Low · S
- **Today:** The default status line shows eight items and `/statusline` opens a picker over 50+ items with presets (`narrow-linux`), but nothing in the running UI hints the line is configurable. The picker is a slash command with no entry point in the status line, idle state, or first-run notice.
- **Friction:** A user on a narrow SSH/tmux terminal who wants spend/budget but not directory/language metadata has no way to discover the `narrow-linux` preset or the picker exists; the customization path is invisible unless they already know the command.
- **Polish:** Append a faint `(/statusline)` hint to the status line in the idle/home state, or add one line to the first-run notice: "Customize your status line with `/statusline`." Keep it absent during active turns so it never competes with live spend.
- **Refs:** `crates/squeezy-tui/src/status.rs:29-54`, `crates/squeezy-tui/src/lib.rs:19804,20104`

### 5. `budget denied:N` reads as past tense for a current-turn-only counter

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The `Budget` item renders `budget ok` at zero denials and `budget denied:N` otherwise. The counter is per-turn (each turn builds a fresh `CostBroker` whose `metrics` starts at `TurnMetrics::default()`), so it resets to `budget ok` once a clean turn completes — exactly as its own description says ("Budget-denial counter for the active turn").
- **Friction:** The wording undersells that correct behavior. "denied:N" is past tense and ambiguous: a user cannot tell from the label whether a tool was just blocked *this turn* or some denial happened earlier and lingered. There is no glyph or color to mark it as an active in-turn condition versus the calm `ok` state.
- **Polish:** Phrase it as a live count for the active turn, e.g. `budget: N blocked` with the error-tier color/`✖` glyph already used elsewhere for nonzero denials, so it reads as a current-turn signal rather than a stale tally. (Drop the "never resets across the session" concern — the counter is genuinely per-turn.)
- **Refs:** `crates/squeezy-tui/src/status.rs:192,615-621,756-762`, `crates/squeezy-agent/src/cost_broker.rs:155-165,467-469`

### 6. Cap-percent overflow clamps disagree between display and enforcement

- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** The status-line cost segment computes its percentage as `(spent / cap * 100.0).min(999.9)` with one decimal, while every broker-side `CostCapStatus` (`cap_percent`) clamps the same ratio to a `u8` ceiling of `255` as an integer. The display and the transcript notices can therefore print different percentages for the same overshoot — e.g. `(640.0%)` in the status line but `(255%)` in the cap-reached banner.
- **Friction:** A heavy-overshoot session shows two different "how far over" numbers depending on which surface the user reads, with no indication that one is clamped. The decimal precision in the status line also implies an accuracy the integer cap notices do not share.
- **Polish:** Share one clamp and one precision between `format_cost_segment` and `cap_percent` (e.g. integer percent, common ceiling) so the status line and the transcript notices always agree on the overshoot figure.
- **Refs:** `crates/squeezy-tui/src/status.rs:669-680`, `crates/squeezy-agent/src/cost_broker.rs:657-663`

### 7. Toast bursts silently drop the oldest with no "more were dropped" affordance

- **Category · Severity · Effort:** Feedback · Low · M
- **Today:** `ToastQueue` hard-caps at three visible toasts; `push_with_ttl` `pop_front`s the oldest to make room with no record that anything was dropped. (Note: production currently pushes toasts from essentially one site — the stalled-screen recovery notice at `lib.rs:1238` — so the burst case is latent until more producers are wired.)
- **Friction:** If a real burst ever lands (the module doc anticipates MCP-connect + index-ready + telemetry-flush firing together), the user sees only the last three and the first is gone with no trace. There is no counter or "+N earlier" marker.
- **Polish:** Track a dropped count on overflow and render a faint "+N earlier" line on the oldest visible toast; this is best done alongside wiring the queue's other production producers so the affordance ships with the feature, not ahead of it.
- **Refs:** `crates/squeezy-tui/src/toast.rs:30-31,129-148`, `crates/squeezy-tui/src/lib.rs:1238-1241`

---

**Dropped from the draft (claims that did not survive grounding):**

- *"Budget denial counter never resets within a session"* — false. The counter is per-turn; a fresh `CostBroker` with `TurnMetrics::default()` is built each turn (`cost_broker.rs:155-165`, `lib.rs:7081`), so it returns to `budget ok` after a clean turn. Reframed as the wording/visual nit in finding #5.
- *"Cost % is computed at display time and may have drifted from the enforcement value (off by one)"* — false. `session_cost_snapshot()` canonicalizes the snapshot's `estimated_usd_micros` to the broker's authoritative `session_cost_usd_micros`, the exact value the cap checks against, and the status line renders from that snapshot — there is no drift between displayed and enforced spend. (The real consistency gap is the *clamp/precision* mismatch, kept as finding #6.)
- *"`unpriced` warning has no TTL and persists forever, even after a priced round"* — false. `AgentEvent::CostUpdate` with `micro_usd > 0` clears `app.cap_unenforceable` (`events.rs:505-507`), so it self-clears on the next priced round. The residual UX issue (the contradictory percent shown alongside it) is kept as finding #3.
- *"Pressure gate is a silent blocker — no toast, no in-transcript notice, no highlight"* — false. The gate fires `AgentEvent::Failed` with `format_pressure_gate_reason`, rendered as a red `push_error` turn-failed banner carrying the full "approaching cap, paused before starting another round" reason (`events.rs:561-598`, `lib.rs:7553-7577`). It is prominent on-screen; the only genuine gap is the off-tab desktop surface, folded into finding #1.
- *"CacheHit read-only path renders `cached R`, inconsistent with `cache ↓R`"* — false. The read-only arm renders `cache ↓{r}` and write-only renders `cache ↑{w}` (`status.rs:479-487`); the word "cache" and the directional arrow are always present. The picker description already documents "shows write↑ and read↓ counts" (`status.rs:185-187`), so the legend concern is covered too.
