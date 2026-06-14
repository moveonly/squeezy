# Navigation & Discoverability

> How a user finds, filters, and moves through a long transcript: the command/action palettes, fuzzy search, semantic filters, breadcrumbs, jump marks, bookmarks, the minimap rail, hover previews, and hyperlinks.

**How it works today:** The navigation surfaces are well-modeled and largely discoverable. A universal command palette is built from the keymap and slash registries and is taught by a settle-delayed first-run hint; a contextual action palette gathers the verbs that apply to the focused entry; both list their nav keys in-modal. Search, jump marks, bookmarks, and breadcrumbs all anchor on stable entry ids (never row offsets) and are pure, testable models, and state changes already surface status-line feedback (the semantic filter even paints a persistent bold badge). The residual gaps are narrow: a couple of keyboard-parity holes, one cross-surface search heuristic that degrades, and a few popovers/summaries that omit a one-token cue the model already carries.

## Quick wins
- [Make a focused breadcrumb activatable from the keyboard](#1-a-focused-breadcrumb-cannot-be-activated-from-the-keyboard)
- [Re-anchor search by entry id when the surface switches](#2-search-re-anchors-by-row-across-a-surface-switch-landing-on-an-unrelated-hit)
- [Add a truncation cue to the jump-history summary](#3-jump-history-summary-truncates-with-no-more-exist-cue)
- [Name the preview source in the popover header](#4-hover-preview-popover-does-not-say-whether-it-is-sticky)
- [Surface an in-context cue that the focused entry has an action menu](#5-action-palette-has-no-in-context-affordance-on-the-focused-entry)

## Findings

### 1. A focused breadcrumb cannot be activated from the keyboard

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** The breadcrumb strip (`Alt+2`) is documented as `←→` to move focus and `Enter` to activate the focused crumb, and `breadcrumbs_activate_focused` exists to do exactly that. But `handle_breadcrumbs_key` deliberately lets bare `Enter` fall through (so the composer can still submit a prompt while the non-modal strip is shown), and `breadcrumbs_activate_focused` is invoked from exactly one place: the `BreadcrumbActivate` *mouse-click* handler. No key reaches it. So `←→` moves the highlight but `Enter` submits the prompt — the crumb is reachable only by mouse.
- **Friction:** A keyboard user steps the focus along the trail, presses `Enter` expecting to jump, and instead fires off a prompt. The strip's own doc comment promises "←→ + Enter" and the click path calls itself "the mouse twin of ←→ + Enter," but that keyboard path does not exist. This breaks the mouse/keyboard parity the rest of the navigation surfaces hold.
- **Polish:** Bind crumb activation to a key the non-modal strip can safely claim — a modified `Enter` (e.g. `Alt+Enter` is taken by the action palette, so `Shift+Enter` or a dedicated `ActivateBreadcrumb` chord), or reuse the strip's own toggle modifier — and route it to `breadcrumbs_activate_focused`. Then update the status hint painted on open to name the real activation key.
- **Refs:** `crates/squeezy-tui/src/lib.rs:11653-11693`, `crates/squeezy-tui/src/lib.rs:11703-11734`, `crates/squeezy-tui/src/lib.rs:18226-18228`, `crates/squeezy-tui/src/breadcrumbs.rs:6-22`

### 2. Search re-anchors by row across a surface switch, landing on an unrelated hit

- **Category · Severity · Effort:** Smoothness · Medium · M
- **Today:** When the Ctrl+T overlay opens or closes while search is live, `lib.rs` sets `state.surface` to the now-active surface and calls `refresh_search` → `search::rebuild`. `rebuild` preserves the current match by picking the survivor nearest the previous one in `(row, col.start)` reading order. That heuristic is sound within one surface (a keystroke or resize), but the main view and the overlay draw from *different* painted `Vec<Line>` sources, so a `(row, col)` from the old surface has no meaningful correspondence to rows on the new one. The "nearest" match it lands on is positionally arbitrary, not the same logical hit.
- **Friction:** A user finds match 3 in the overlay, closes it, and the cursor jumps to whatever match happens to share a similar row index on the main surface — often somewhere unrelated. The search appears to lose its place exactly when the user expected it to follow them back.
- **Polish:** Track the current match's stable entry id (the surface-independent identity the rest of navigation uses), and on a surface switch re-anchor to the nearest match *within that entry's rows* rather than by raw `(row, col)`. Failing that, on a surface switch fall back to match 1 with a brief status note ("search re-run on main view") so the jump is at least honest instead of silently mis-landing.
- **Refs:** `crates/squeezy-tui/src/lib.rs:9471-9479`, `crates/squeezy-tui/src/search.rs:225-265`

### 3. Jump-history summary truncates with no "more exist" cue

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `JumpMarkStack::history_summary(max, label)` formats the recent jump trail as `#5 ← #3 ← #1`, joined with ` ← `, and stops after `max` entries. The history ring holds up to `HISTORY_CAP` (16), but the only caller passes `max = 4`. When there are more than four jumps, the summary shows four with no trailing marker, so the oldest shown reads as if it were the oldest in history.
- **Friction:** A power user relying on the jump trail sees `recent jumps: #8 ← #6 ← #4 ← #2` and cannot tell whether `#2` is the end or whether older jumps were cut. The absence of a trailing token makes an exhaustive readout indistinguishable from a truncated one.
- **Polish:** In `history_summary`, when `self.history.len() > max`, append a trailing ` ← …` token so a clipped trail reads `#8 ← #6 ← #4 ← #2 ← …`. One token, computed from the data already in hand.
- **Refs:** `crates/squeezy-tui/src/jump_marks.rs:116-129`, `crates/squeezy-tui/src/lib.rs:16848-16850`

### 4. Hover preview popover does not say whether it is sticky

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** A preview popover can be raised by a mouse hover (`PreviewSource::Hover`, dismisses on pointer-leave) or by the keyboard preview verb (`PreviewSource::Keyboard`, sticky — it survives an incidental mouse drift). The model carries the source and exposes `is_keyboard()`, and the keyboard path's *status line* does say "Esc / Alt+1 close." But the popover itself reads only " Preview — entry " with a footer activation hint; the header never indicates whether the popover is pinned or will vanish on the next mouse move.
- **Friction:** A user who pinned a keyboard preview and then reaches for the mouse can't tell from the popover whether moving the pointer will dismiss it. The self-describing artifact (the popover) is silent on the one behavior that differs between the two sources.
- **Polish:** Append a dim source suffix to the popover header when keyboard-pinned — e.g. " Preview — entry · pinned " — gated on `preview.is_keyboard()`, with no suffix for a mouse hover. One conditional span; the styling token (`quiet`/`dim`) already exists.
- **Refs:** `crates/squeezy-tui/src/lib.rs:30751-30762`, `crates/squeezy-tui/src/hover_preview.rs:251-257`

### 5. Action palette has no in-context affordance on the focused entry

- **Category · Severity · Effort:** Discoverability · Low · S
- **Today:** The contextual action palette opens with `Alt+Enter` on the focused (or top-visible) entry, and once open it lists its actions and nav keys. It is discoverable *transitively* — `OpenActionPalette` is a keymap action, so it appears in the command palette, which itself has a first-run hint. But nothing in the main view signals that the *focused entry* has a contextual menu: the first-run hints teach the command-palette chord and the focus-to-peek gesture, never "this card has actions."
- **Friction:** A keyboard user who focuses a card and raises a preview (the taught gesture) is one chord away from the action menu but is never told it exists in context. They learn it only by hunting the command palette or `Alt+Enter` by accident — the CLI analogue of a right-click menu with no hint that right-click does anything.
- **Polish:** When an entry is keyboard-focused (or in the hover-preview footer that already paints for that entry), add a quiet, dim suffix naming the chord — e.g. "· Alt+Enter for actions" — reusing the `key_hint` substitution so a rebound key shows correctly. Keep it to one token so it stays within the quiet aesthetic.
- **Refs:** `crates/squeezy-tui/src/lib.rs:12957-12984`, `crates/squeezy-tui/src/lib.rs:30791-30797`

### 6. Minimap rail cells have no per-cell hover cue

- **Category · Severity · Effort:** Discoverability · Low · M
- **Today:** The minimap rail (`Alt+r`) paints semantic glyphs and registers each occupied cell as a click target (`TargetKey::Entry`). The toggle status already teaches "minimap rail on — click a tick to jump," and the rail brightens the viewport band so the user can see where they are. But there is no hover affordance on an individual cell: the pointer dwelling over a tick produces no box, arrow, or dim highlight to confirm *that* cell is clickable.
- **Friction:** Once the toggle status scrolls away, a returning user sees a colored column with no per-cell signal of interactivity. Because rail cells register as `TargetKey::Entry`, a hover *might* even raise the full entry preview popover, which is louder than the rail's quiet aesthetic intends.
- **Polish:** Give a hovered rail cell a restrained, debounced cue — invert the glyph, or shift it to the `accent` color for the dwell — distinct from raising the full entry popover over a 1-cell rail. Keep it to a single-cell glyph/color change so the rail stays quiet.
- **Refs:** `crates/squeezy-tui/src/lib.rs:33663-33701`, `crates/squeezy-tui/src/minimap.rs:80-114`

## Dropped from the draft (premise did not survive the code)

- **"Command palette lacks an affordance hint"** — The palette is taught by a first-run hint ("tip: press {chord} for the command palette") and, once open, paints a title ("Command palette — run any command") plus a nav header in every state, including the empty query. `first_run_hints.rs:95`, `lib.rs:31284-31298`.
- **"Fuzzy matcher never surfaces scoring feedback"** — The proposal is a debug/stderr trace, not user-facing polish; the draft itself states the ranking is invisible by design. `fuzzy.rs`.
- **"Semantic filter offers no hint when toggled"** — `cycle_main_semantic_filter` writes a status flash on every toggle, *and* `render_semantic_filter_badge` paints a persistent bold accented "⧩ filter: … · {key}" badge whenever a filter is active. `lib.rs:10422-10431`, `lib.rs:33874-33914`.
- **"Breadcrumb crumbs offer no keyboard focus indicator"** — The focused crumb is painted in `accent` + `BOLD`; unfocused crumbs use plain `fg`. The bold (non-color) modifier even survives a monochrome terminal. `lib.rs:42833-42837`. (The real breadcrumb gap is keyboard *activation*; see finding 1.)
- **"Bookmarks offer no inline creation or confirmation"** — `DropBookmark` (`Alt+;`) creates an anonymous bookmark at the top-visible entry with no modal and confirms via status ("bookmark dropped at … (N total) — Alt+q to list"). `lib.rs:14437-14447`.
- **"Hyperlinks silently fail when OSC 8 is unsupported"** — Linkification applies only to the exit-mirror scrollback rows, not the live TUI; the visible glyphs are byte-identical with or without the escapes and stay selectable/copyable. The proposed bracket/`[link]` injection would corrupt copy-paste of the URL it claims to help. `hyperlinks.rs:3-17`, `lib.rs:48309-48357`.
