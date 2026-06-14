# Squeezy UI/UX Polish Review

> This review covers the full squeezy TUI coding-agent surface — from `install.sh` and first run through the live transcript, composer, approvals, queue, config, session lifecycle, resilience layer, and headless CLI. Twelve domain reports each describe how a surface works today, then list concrete findings (Category · Severity · Effort) with a "today / friction / polish / refs" body, plus a "dropped from the draft" section recording draft claims that did not survive grounding against the code. Read this index for the cross-cutting picture and the sequencing; open a domain file for the specific fixes. Every finding cites source locations and most are S-effort string/render polish.

## Domain reports

| Domain | File | Findings | High-sev | Top quick win |
|---|---|---|---|---|
| Onboarding & First Run | [onboarding-first-run.md](onboarding-first-run.md) | 8 | 4 | Verify the binary runs before telling the user it's installed |
| Conversation & Transcript Surface | [conversation-transcript.md](conversation-transcript.md) | 6 | 0 | Bound the streaming patch preview's search/replace bodies |
| Input & Prompt Composition | [input-composition.md](input-composition.md) | 6 | 0 | Add key-hint lines to the inline slash / @-mention popups |
| Navigation & Discoverability | [navigation-discovery.md](navigation-discovery.md) | 6 | 0 | Make a focused breadcrumb activatable from the keyboard |
| Approvals, Permissions & Safety | [approvals-permissions-safety.md](approvals-permissions-safety.md) | 9 | 0 | Lead the approval hint with the primary action, drop silent aliases |
| Cost, Status & Live Feedback | [cost-status-feedback.md](cost-status-feedback.md) | 7 | 2 | Wire cost-cap / pressure-gate events to the off-tab desktop notifier |
| Prompt Queue & Subagents | [queue-and-subagents.md](queue-and-subagents.md) | 11 | 1 | Make condition markers readable on a NO_COLOR terminal |
| Config, Theming & Settings | [config-theming-settings.md](config-theming-settings.md) | 11 | 0 | Show what values revert in the Discard-all confirmation |
| Session Lifecycle & Continuity | [session-lifecycle.md](session-lifecycle.md) | 9 | 0 | Surface the Tab toggle when scoped picker is empty but cross-project sessions exist |
| Accessibility & Resilience | [accessibility-resilience.md](accessibility-resilience.md) | 6 | 0 | Don't claim a stalled screen recovered when the forced redraw failed |
| CLI & Headless Surface | [cli-headless.md](cli-headless.md) | 9 | 0 | Add default + per-mode semantics to `--prompt-permission-mode` help |

## Cross-cutting themes

These recur across multiple domains and are the highest-leverage fixes because one pattern, fixed once, pays off on every surface that follows it.

**1. Silent state transitions and silent fallbacks.** The single most common pattern: the agent computes a state change or degrades to a default and shows the user nothing. Spans onboarding (deferred-key step, ambiguous progress denominator), conversation (silent syntax fallback, unstaged streaming tail), input (history boundaries, empty @-mention, dedup misses), cost (circuit-breaker trips, pressure events off-tab), config (silent Space-cycle success), session (corrupt-checkpoint drop, micro-compaction, summarization freeze, branch-load `!`), and accessibility (layout-fallback count). **Direction:** when the code already computes a number or a reason (it almost always does), spend it — one terse status line, a dim placeholder row, or a debug log on the non-happy path. Make the absence *stated*, not implied.

**2. Inconsistent / buried keybinding hints.** Hints exist but contradict each other, omit the primary action, or skip surfaces that have the same convention everywhere else. The queue overlay teaches two divergent cheatsheets for the same keys (header vs. status line); the approval footer buries `Enter` under slash-aliases the doc-comment itself calls "silent"; the inline slash and @-mention popups skip the hint line every modal overlay paints; the onboarding picker and the queue indicator give every verb equal weight. **Direction:** derive one legend from one source per overlay; lead with the primary action and demote/unadvertise compatibility aliases; backfill the missing hint line on the two inline composer popups.

**3. Discoverable features with no in-context affordance.** Real capabilities exist but are invisible at the moment of need, reachable only via a self-referential hint or an obscure chord. `squeezy auth` / OAuth absent from README; wide-block horizontal pan discoverable only after you press the keys that reveal its own hint; `/metrics` and the render HUD behind `Ctrl+Alt` debug chords; `/statusline` customization with no entry point; density/zen modes and the live RGB theme editor absent from `/config`; the action-palette and minimap have no per-target cue; the cross-project Tab toggle hidden behind a picker that's never drawn. **Direction:** add a quiet one-token in-context cue (reusing `key_hint` substitution so rebinds show correctly), or surface the feature from the centralized screen (`/config`, README, first-run notice) where the user already looks.

**4. Feedback that asserts more than it knows.** Status text fires unconditionally or contradicts its own state. The stalled-screen recovery toast claims success even when `force_full_redraw` failed; the detail pane reports "opened" on terminals too narrow to draw it; the cost segment shows a spend percentage next to `⚠ unpriced` (cap is inert); `budget denied:N` reads as a lingering past-tense tally for a per-turn-only counter; the install script prints "run `squeezy --help`" without verifying PATH. **Direction:** gate success messages on the actual result; suppress numbers that can't mean what they imply; verify before instructing.

**5. Destructive / consequential actions under-preview their impact.** Confirmations and consequence cues don't show enough to confirm against. Discard-all lists files but no values; Reset truncates its diff at 12 rows with no scroll; the cross-project resume row changes your working directory but says so only on hover; bundle redaction's opt-out is undiscoverable from the preview; the truncated approval diff and the `Rule:`-persists-to-disk scope are both unstated before you decide. **Direction:** preview impact, not just the target; spell out the consequence (cwd change, disk persistence, opt-out keyword) persistently, not on hover or after the fact.

**6. Truncation with no recovery path or "more exists" cue.** Content is clipped and the user can't tell an exhaustive readout from a clipped one, or reach the rest. Outline titles cut at 60 chars with the full text discarded; the jump-history summary stops at 4 with no trailing `…`; the approval diff caps at 18 lines without naming `/diff`; collapsed-group member counts get eaten first by truncation; annotations clamp at 280 chars silently at save time. **Direction:** append a one-token truncation marker, name the recovery verb, protect the load-bearing token from truncation, or retain and reveal the full value on focus.

**7. Color-only encoding fails on NO_COLOR / monochrome terminals.** Meaning carried purely by theme color collapses when `palette::best_color_for_detected_level` maps every color to `Reset`. Queue condition markers (runnable/skip-bound/blocked) become one indistinguishable glyph; the paused-group `[P]` borrows the warn (error) color so a held batch reads as broken. **Direction:** make state carry a glyph or ASCII suffix in addition to color (the review board already does this with ASCII-only lane labels — generalize that discipline).

**8. Empty / not-found states dead-end instead of guiding.** A correct-but-terminal message with no next action. `providers list --configured` prints `(no providers match)` and stops; the unsupported `/help` topic dumps a flat 20-id list instead of the grouped index that already exists; the empty-queue overlay still shows list-navigation keys; the empty scoped resume picker isn't drawn at all. **Direction:** every empty/not-found path should point at the next action or the better-organized view that already exists.

## Suggested sequencing

**Phase 1 — cheap, broad, ship first (string/render-only S-effort, mostly theme 1/2/4/6/8):**
1. Fix feedback that asserts more than it knows — these are correctness-adjacent and trust-eroding: gate the recovery toast on `recovered`, gate the detail-pane "opened" on the width threshold, fix the install-script PATH/`--help` ordering, suppress the `unpriced` percentage. (accessibility, approvals, onboarding, cost)
2. Backfill and unify keybinding hints: add hint lines to the inline slash/@-mention popups, collapse the queue's two cheatsheets to one, lead the approval footer with `Enter`. (input, queue, approvals)
3. Add truncation/"more exists" tokens and recovery verbs: jump-history `…`, approval diff `/diff`, outline full-title retention, protected group counts. (navigation, approvals, conversation, queue)
4. Turn silent transitions into one-line status: history position, empty @-mention, successful Space-cycle, micro-compaction count, branch-load `!` legend. (input, config, session)
5. Make empty/not-found paths actionable and CLI help self-documenting: `providers list --configured`, grouped `/help`, `--prompt-permission-mode` semantics, empty-queue overlay copy. (cli, queue)

**Phase 2 — broad but slightly more involved (discoverability surfacing, theme 3/7):**
6. Add NO_COLOR-safe glyph/ASCII suffixes to color-only encodings (queue condition + group markers).
7. Surface discoverable-but-hidden features from the centralized screens: `auth`/OAuth in README + onboarding, density/zen and the live theme editor in `/config`, `/statusline` and `/metrics` entry points, the cross-project Tab toggle.
8. Add in-context affordance cues on individual targets (focused-entry action menu, breadcrumb keyboard activation, minimap hover, wide-block pan hint).

**Phase 3 — higher-effort structural polish (theme 5 and the M/L items):**
9. Preview impact on destructive confirmations: Discard-all value diff, scrollable Reset preview, persistent cross-project cwd-change and bundle-redaction-opt-out cues.
10. Stage live/streaming regions and heavy background ops: streaming-tail visual staging, contemporaneous compaction/summarization progress, the streaming patch-body clamp.
11. Let the setup picker accept a key inline rather than only deferring it; route `/config` color editing into the live channel picker; thread pre-classifier + reviewer reasons into a single combined deny rationale.
