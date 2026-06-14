# Input & Prompt Composition

> The composer and everything that feeds it: slash-command completion, `@`-mention file lookup, prompt-history recall, the graduated large-paste UIs, prompt templates, snippets, clipboard history, and quote/scratchpad capture.

**How it works today:** The user types into a live composer with inline assistance — a slash-command menu (typed `/…`), an `@<path>` mention popup ranked against a cached workspace walk, and Up/Down prompt-history recall. Pastes are routed by size: small types inline, >1k chars collapses to a `[Pasted text #N]` attachment, >10k chars opens a confirm/cancel preview modal, and >50k chars opens a richer staging menu (insert, quote, code block, strip ANSI, temp file, queue, copy preview, cancel). The paste models are pure and well-factored, the modals already carry titles, summaries, warnings, and `↑↓/Enter/Esc` hint lines — so the remaining gaps are narrow polish, concentrated in the two *inline* composer popups (slash, mention) which lack the hint lines every modal overlay has, and in recall/dedup feedback.

## Quick wins
- [Inline popups (slash, @-mention) have no key-hint line](#1-inline-popups-slash-and-mention-have-no-key-hint-line)
- [Prompt-history recall gives no position or boundary feedback](#2-prompt-history-recall-gives-no-position-or-boundary-feedback)
- [`@`-mention with no matches silently shows nothing](#3-mention-with-no-matches-silently-shows-nothing)
- [Strip-ANSI pre-selection reason is never connected to the warning](#4-strip-ansi-pre-selection-reason-is-never-connected-to-the-warning)
- [Clipboard-history dedup keys on label, so re-copies duplicate rows](#5-clipboard-history-dedup-keys-on-label-so-re-copies-duplicate-rows)

## Findings

### 1. Inline popups (slash and @-mention) have no key-hint line
- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** Every *modal* overlay paints a trailing hint line — the paste transform/staging menus show `↑↓ choose · Enter apply · Esc cancel`, snippets/templates/theme/terminal pickers show their own `↑↓ … Enter … Esc` strips. The two popups that live *inline* in the composer — the slash menu and the `@`-mention popup — render only their rows (the slash menu adds parameter/badge hints, the mention popup adds an `idx/total` footer), with no line telling the user which keys drive them.
- **Friction:** When suggestions exist, Up/Down navigate the slash menu and `return` before history recall (`lib.rs:8362-8398`), so a user mid-`/command` who expects Up to recall a prior prompt instead moves the menu selection — with nothing on screen explaining that the menu now owns those keys, or that Tab/Enter complete it. The mention popup similarly accepts both Tab and Enter to apply (`input.rs:569`) but advertises neither.
- **Polish:** Add a one-line hint under each inline popup, matching the modal convention: slash → `↑↓ choose · Tab/Enter complete · Esc close`; mention → `↑↓ choose · Tab/Enter apply · Esc cancel`. This closes the consistency gap and makes the Up/Down handoff away from history self-evident.
- **Refs:** `lib.rs:42013-42132` (slash menu render, no hint), `lib.rs:42355-42412` (mention popup render, only `idx/total`), `lib.rs:8362-8398` (Up/Down dispatch: menu first, then history), `input.rs:548-572` (mention Tab|Enter apply)

### 2. Prompt-history recall gives no position or boundary feedback
- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** Up/Down cycle the 100-entry history, swapping the composer text in place. There is no indicator of where you are in the buffer (no `history N/M`), unlike the mention popup which shows `idx/total`. At the oldest entry, Up saturates silently (`(Some(0), Previous) => Some(0)`); stepping Down past the newest entry restores the stashed draft and clears `input_history_index`, but sets no status — the only message the path ever emits is `"no prompt history"` on an empty buffer (`input.rs:915-917`).
- **Friction:** Recall is invisible: the user cannot tell how deep they are, whether another Up will do anything, or that the last Down dropped them back into their own draft versus another history entry. The boundary transitions are exactly the moments a one-word status would help, and they are the silent ones.
- **Polish:** On each successful recall set `status = "history {index+1}/{len}"`; when Down restores the draft (`input.rs:936-942`) set `status = "draft"`. Reuses the status line already in scope — no new chrome.
- **Refs:** `input.rs:914-954` (`recall_prompt_history`), `input.rs:936-942` (Down-to-draft boundary, no status), `lib.rs:8362-8398` (recall dispatch)

### 3. @-mention with no matches silently shows nothing
- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** When the `@<word>` query ranks to zero workspace files, `refresh_mention_popup` sets `app.mention_popup = None` and returns with no status (`input.rs:459-461`). Typing `@doesnotexist` simply shows no popup — indistinguishable from "the popup feature isn't active here" or "the workspace walk hasn't loaded yet."
- **Friction:** The user gets no confirmation that the lookup ran and found nothing, so a typo'd path reads the same as a disabled feature. The cache-still-building case (popup also absent) compounds the ambiguity.
- **Polish:** When a live mention query matches nothing, either set `status = "no files match @{query}"` or render a single dimmed `no matching files` row in the popup so the affordance stays visible and the empty result is explicit.
- **Refs:** `input.rs:425-466` (`refresh_mention_popup`), `input.rs:459-461` (empty-match path clears popup, no status)

### 4. Strip-ANSI pre-selection reason is never connected to the warning
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** When a paste carries escape bytes, both the transform menu and the staging menu pre-select `Strip ANSI` as the default cursor position (`paste_transform.rs:396-404`, `paste_staging.rs:422-430`). The staging modal already shows a warn-colored `warnings: terminal control bytes …` line (`lib.rs:25967-25984`) and the selected row already shows its description (`Remove terminal escapes, then attach`) — but nothing ties the pre-selected cursor to that warning, so the highlight reads as an unexplained default.
- **Friction:** The user sees `Strip ANSI` highlighted and a separate warning line, but no causal link — they may assume it's the only valid action or that the tool is guessing at their intent. The transform menu has no warnings line at all, so its pre-selection is even less explained.
- **Polish:** When the menu opens on an escape-laden paste, append a short suffix to the warnings/hint line such as `— Strip ANSI selected` (staging) or add the same one-liner to the transform menu, so the highlighted default is visibly *because of* the detected escapes.
- **Refs:** `paste_staging.rs:405-436` (`PasteStaging::new` pre-selection), `paste_transform.rs:382-410` (`PasteTransformMenu::new` pre-selection), `lib.rs:25967-25984` (staging warnings line render)

### 5. Clipboard-history dedup keys on label, so re-copies duplicate rows
- **Category · Severity · Effort:** Friction · Low · M
- **Today:** `record` collapses a back-to-back duplicate only when the newest entry matches on *both* text and label (`clipboard_history.rs:159-167`). Re-copying an existing history entry always relabels it `"clipboard history"` (`lib.rs:15124-15128`), so re-copying a row that was originally captured as `"selection"` or `"paste preview"` inserts a second row with identical text and a new label — the dedup misses and the picker grows a near-twin.
- **Friction:** The exact re-copy gesture the picker exists to enable is the one that defeats its dedup, cluttering the list with payload-identical rows that differ only by an internal label string the user never chose.
- **Polish:** Either dedup on text alone when the newest entry's payload matches (return its id, refresh its label/cursor), or special-case the re-copy path to reuse the source entry's id instead of recording a fresh `"clipboard history"` row.
- **Refs:** `clipboard_history.rs:158-182` (`record`, dedup on text *and* label), `lib.rs:15121-15129` (`recopy_clipboard_entry` always labels `"clipboard history"`)

### 6. Paste modals explain the action but not why this UI versus inline
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The preview modal titles `Large paste — confirm before it enters the composer` and the staging modal titles `Large paste staged — choose an action`; the staging header reports `kind · lines · chars · bytes · ~tokens` (`lib.rs:25954-25965`). So the size and the required action are visible — but nothing states the routing rationale: a 10k-char paste gets a confirm/cancel modal while a 50k one gets the full action menu, and the user has no cue why this paste earned a richer UI than the last.
- **Friction:** Without a size-band cue, the escalation between inline → attachment → confirm modal → staging menu feels arbitrary; a user who pastes slightly more than last time and lands in a different UI can't tell what crossed the line.
- **Polish:** Add a faint sub-label to each title, e.g. preview → `(>10k chars)` and staging → `(>50k chars)`, so the band that triggered the UI is self-documenting. Cheap, and it never touches the inline/attachment paths.
- **Refs:** `paste_preview.rs:29-50` (`VERY_LARGE_PASTE_CHAR_THRESHOLD`, `is_very_large_paste`), `paste_staging.rs:47-78` (`HUGE_PASTE_CHAR_THRESHOLD`, `is_huge_paste`), `lib.rs:25662-25673` (preview title), `lib.rs:25919-25930` (staging title)

---

**Dropped from the draft (verified against code, not real):**
- *"@-mention truncation footer never signals truncation"* — false. The footer already reads `{idx}/{total}  (+ more files not shown — refine query)` when the walk hit the cap (`lib.rs:42396-42404`).
- *"Template slot wrap is silent / focus unclear"* — false. The card header names the focused slot and shows `slot N/count {name}` (`lib.rs:26983-26991`), the focused row carries a `›` caret in bold secondary, and the row window scrolls to keep it visible (`lib.rs:27118-27163`).
- *"Preview/staging line cap may make the user think the rest is lost"* — weak/moot. The header already reports the full line count and the body ends with a `… +N more lines` marker (`paste_preview.rs:163-170`, `paste_staging.rs:332-339`), so the cap is already legible.
