# Accessibility & Resilience

> The automated accessibility gates plus the self-healing watchdogs and teardown guards that keep squeezy usable on degraded terminals and recoverable when a frame, layout, panic, or signal goes wrong.

**How it works today:** A `cfg(test)`-only accessibility gate audits rendered surfaces for contrast, screen-reader-extractable text, minimal-glyph chrome, and keyboard reachability across three reference terminal profiles. At runtime, a stuck-render watchdog forces a full redraw (and a recovery toast) when a wanted frame stalls past 2s, a last-known-good layout store substitutes a prior frame when geometry goes degenerate, and a proactive degraded-mode banner offers a one-keystroke fallback on torn/tiny/no-color/remote terminals — dismissible and latched so it never nags. Terminal teardown is single-sourced and idempotent across the clean exit, panic hook, signal handlers, and `Drop`, and scroll/focus survive resize via logical entry-id anchoring. The design prioritizes invisibility and zero idle cost, so most resilience work happens silently — which is good for performance but leaves a few visibility and diagnostic-clarity gaps.

## Quick wins
- [Don't claim a stalled screen was recovered when the forced redraw failed](#1-recovery-toast-claims-success-even-when-the-forced-redraw-failed)
- [Surface the layout-fallback count outside the hidden diagnostics HUD](#3-layout-fallback-substitutions-are-invisible-without-the-diagnostics-hud)
- [Give each accessibility gate a one-line prose explainer in its failure report](#4-gate-failures-name-the-gate-but-never-explain-why-it-matters)
- [Document the SeqCst contract once for all crash-path statics](#6-crash-path-atomics-are-all-seqcst-but-only-one-is-documented)

## Findings

### 1. Recovery toast claims success even when the forced redraw failed

- **Category · Severity · Effort:** Feedback · Medium · S
- **Today:** When the stuck-render watchdog fires, the loop calls `force_full_redraw`, captures `recovered = ….is_ok()`, then pushes the toast `"Recovered a stalled screen — forced a full redraw."` **unconditionally** — including when `recovered` is `false`. The `recovered` flag is only used to decide whether to snap the drawn revision forward; it never gates the user-facing message.
- **Friction:** A genuinely wedged terminal (the case the watchdog exists for) is exactly where `force_full_redraw` is most likely to error, yet the user is told the screen was recovered. The toast asserts a fix that did not happen, and on the next stall the throttle delays the retry — so the user sees a confident "recovered" message over a still-frozen screen.
- **Polish:** Gate the toast on `recovered`. On success keep the current wording; on failure either stay silent (the throttle will retry) or emit a distinct, honest line (e.g. `"Screen still stalled — retrying redraw."`). One `if recovered { … }` around the existing `toasts.push`.
- **Refs:** `crates/squeezy-tui/src/lib.rs:1229-1241`, `crates/squeezy-tui/src/terminal_guard.rs:891-909`

### 2. Always-path keyboard equivalents are declared as prose and never verified

- **Category · Severity · Effort:** Consistency · Medium · M
- **Today:** The keyboard-reachability gate maps every mouse affordance to a `KeyboardPath`. `Keymap(action)` arms are genuinely checked — the cited action's default binding must round-trip back to it through the keymap (`keyboard_reachability_gate_with`). `Always("…")` arms — which cover the large majority of modal/overlay affordances (queue delete, every picker's ↑↓/Enter, paste/clipboard/snippet handlers, etc.) — are accepted as documentation strings with no validation; the gate's `Some(KeyboardPath::Always(_)) => {}` arm is a no-op, and the test only asserts the `Keymap` arms resolve.
- **Friction:** If a modal's key handler is renamed, moved, or dropped, the gate still passes because nothing ties the prose string to a real handler. The accessibility guarantee ("no mouse-only affordance") is enforced for keymap actions but taken on faith for the overlay handlers that make up most of the vocabulary — a regression there is silent.
- **Polish:** Either (a) add a `#[cfg(test)]` hook that lets a modal provider pass its real key handler so each `Always` affordance can be asserted reachable, or (b) at minimum tie each `Always` arm to the downstream test that exercises that handler (a comment block naming, per overlay, the test file that asserts the path), so a moved handler breaks a named test rather than passing on the prose alone.
- **Refs:** `crates/squeezy-tui/src/accessibility.rs:571-597`, `crates/squeezy-tui/src/accessibility.rs:604-918`

### 3. Layout-fallback substitutions are invisible without the diagnostics HUD

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `LastGoodLayout::resolve` substitutes the last-known-good geometry whenever a frame goes degenerate, bumping a `fallback_count`. That count surfaces only on the `show_layout_fallback` diagnostics HUD (`render_metrics_hud`); a normal session sees nothing, and the counter wraps silently on overflow (`wrapping_add`).
- **Friction:** A user hitting repeated degenerate layouts (sub-8×4 terminals, or a pane-drag resize storm) gets the substituted frame with zero signal that stability is being actively held — or whether it is holding steady versus escalating. The silence is correct for idle cost but offers no live confidence that the fallback is what kept the screen usable.
- **Polish:** Optionally surface a single transient cue when the count first increments in a session (a one-shot `"layout regenerated"` toast, then quiet), or a dim `[layout-ok]` status badge while a good snapshot is held. Either keeps the silent-fallback contract and the one-`Cell` write while giving a glance-able signal. Toast is less noise; the badge is more live.
- **Refs:** `crates/squeezy-tui/src/layout_fallback.rs:209-227`, `crates/squeezy-tui/src/layout_fallback.rs:237-260`, `crates/squeezy-tui/src/lib.rs:25278`

### 4. Gate failures name the gate but never explain why it matters

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `GateKind` carries only a `label()` (`"screen_reader_text"`, `"contrast"`, …) plus a precise technical `detail` on each `Violation`. A failing report reads, e.g., `required content "Clamp the offset…" not extractable as plain text` — accurate, but it never states *why* that is a failure (screen readers can't read color/glyph-only meaning) or what to do about it.
- **Friction:** The audience for this output is whoever just broke the gate — a developer reading a CI failure on the `cfg(test)` accessibility module. The message assumes the reader already knows each gate's intent, so a contributor unfamiliar with the gate gets a symptom without the reasoning or the fix direction.
- **Polish:** Add a `GateKind::explanation()` returning one terse prose sentence per gate (e.g. ScreenReaderText → "Screen readers extract text only; color- or glyph-coded meaning is invisible to them"), and print it alongside the label so each violation reads as label + why + detail. One sentence, fits in a log line.
- **Refs:** `crates/squeezy-tui/src/accessibility.rs:377-387`, `crates/squeezy-tui/src/accessibility.rs:488-513`

### 5. No cue that the transcript wraps narrower than the raw terminal

- **Category · Severity · Effort:** Clarity · Low · M
- **Today:** The transcript (and the clean-exit scrollback mirror) wrap to `main_text_width` — the painted text column, which excludes the scrollbar gutter and an active minimap rail — stamped every frame at `app.main_text_width`. The render and the exit mirror both honor it, but the user only ever sees the raw terminal size; the effective content width is read internally (wide-block pan math) and never shown.
- **Friction:** On a wide terminal with a rail active, the transcript wraps to noticeably less than the full width. Copying text or toggling the minimap reflows at a column the user can't predict, and a resize that changes the rail changes wrapping with no visible cause.
- **Polish:** Optional, off by default: a dim status field (or diagnostics-HUD line) showing `content NxM` only when `main_text_width` is materially narrower than the terminal (say >10%), gated behind the diagnostics overlay or a `/width` toggle so it never adds noise to a normal session.
- **Refs:** `crates/squeezy-tui/src/lib.rs:21689-21691`, `crates/squeezy-tui/src/lib.rs:33328`, `crates/squeezy-tui/src/terminal_guard.rs:319-332`

### 6. Crash-path atomics are all SeqCst but only one is documented

- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** The signal-teardown statics — `ALT_SCREEN_ACTIVE`, `SUSPEND_REQUESTED`, `HOOKS_INSTALLED`, `SIGNAL_HANDLERS_INSTALLED` — all use `Ordering::SeqCst`. Only `ALT_SCREEN_ACTIVE`'s accessors carry prose about the exactly-once / cross-thread guarantee; the install-guard and suspend-request statics use the same ordering with no stated rationale.
- **Friction:** The atomicity contract is implicit. A maintainer reworking the panic hook or signal handlers sees SeqCst on the install guards with no note and could downgrade to `Relaxed`/`Release` without realizing the crash-path visibility guarantee (handlers running on arbitrary threads/tasks; `Drop` racing the panic hook) depends on it.
- **Polish:** Add one module-level comment block stating the crash-path concurrency model — these statics are read/written by signal handlers and the panic hook on arbitrary threads, and SeqCst ensures the alt-screen is left exactly once under a `Drop`/panic-hook race and that the install/resume flags are globally visible — and optionally tag each static with a one-line `// SeqCst: crash-path atomicity` note.
- **Refs:** `crates/squeezy-tui/src/signal_teardown.rs:40-72`, `crates/squeezy-tui/src/signal_teardown.rs:84-101`

## Dropped from the draft (verified against the code)

- **"Missing status-line feedback on render health recoveries"** — the loop already pushes a recovery toast (`"Recovered a stalled screen — forced a full redraw."`) when the watchdog fires (`lib.rs:1238`); the draft's premise (only a tracing log) is false. The real issue is the *opposite* — that toast fires even on failure (finding 1).
- **"Degraded-mode banner has no context when dismissed"** — dismissal already sets a status line (`"degraded-mode suggestion dismissed"`, `lib.rs:14076`); the proposed persistent `[dismissed-suggestion]` badge + re-offer contradicts the spec's latched no-nag and "incorrect suggestion is worse than unknown" contract.
- **"No visual distinction between freshly-settled and re-offered degraded suggestion"** — speculative motion gated to a hypothetical animation preference; runs against the deliberately quiet, motion-free banner design and adds idle-cost surface for no concrete defect.
- **"Overlay/modal focus lacks cycle-out documentation"** — the overlay header already prints `↑↓ choose · Enter apply · Esc cancel` (`overlay.rs:165`), so the exit affordance and edge behavior are signaled; the premise (no hint) is false.
- **"Presentation-mode indicator lacks reveal-status clarity in narrow terminals"** — the `[present: revealed]` badge is prepended at the status row's **left** edge and the row truncates from the right (`truncate_line_to_width`), so the badge survives and the trailing hints are what clip; the premise (badge silently truncated to `[present:`) is false.
