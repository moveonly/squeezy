# Session Lifecycle & Continuity

> Starting fresh vs resuming, persisting and restoring UI state, navigating session events, external-editor handoff, shareable bundles/exports, and context compaction.

**How it works today:** A startup picker (`resume_picker.rs`) offers recent sessions, scoped to the current directory by default with a Tab toggle into a cross-project view; "Start fresh" leads the list as the pre-selected safe default. UI state (scroll anchor, selection, search, minimap) auto-saves to a debounced, schema-versioned per-session checkpoint (`session_checkpoint.rs`) and restores on relaunch, clamped against the live transcript. Bundles (`session_bundle.rs`) and exports (`export_destination.rs`) reuse the shared transcript renderer with redaction-on-by-default and traversal-safe destinations; the timeline (`session_timeline.rs`), annotations (`annotations.rs`), and editor handoff (`editor_handoff.rs`) round out navigation/editing. Context pressure is relieved in three tiers — micro-compaction clears bulky tool-output bodies in place, then extractive/model-assisted summarization folds an older slice, with an undo checkpoint persisted (`micro_compaction.rs`, `context_compaction.rs`). The happy path is smooth and safe; the friction is concentrated at boundaries where consequences, constraints, and heavy-operation feedback go unsurfaced.

## Quick wins
- Surface the Tab affordance when the scoped view is empty but cross-project sessions exist
- Spell out the directory switch in the cross-project footer, not just on row highlight
- Add a legend for the `!` branch-load-failed marker
- Show a live character counter in the annotation edit modal (280-char cap)
- Tell the user how to opt out of bundle redaction in the preview

## Findings

### 1. Cross-project-only sessions never see the picker or the Tab toggle

- **Category · Severity · Effort:** Discoverability · Medium · S
- **Today:** When the merged candidate list is non-empty but every entry is cross-project (no session for the current cwd), `run_picker` builds the default *scoped* view, finds `state.candidates` empty, and returns `StartFresh` without drawing a single frame. The user is dropped straight into a fresh session and never learns that resumable sessions exist one Tab away. The module doc frames this as deliberate ("opt in via Tab rather than surprise them with foreign sessions"), but the Tab affordance is exactly what's hidden.
- **Friction:** The one control that would reveal those sessions is only reachable from a picker that is never shown. A user with sibling-clone sessions silently loses the resume path.
- **Polish:** When the scoped view is empty but `all_sessions` holds cross-project candidates (`has_scoped_candidates` already answers this), draw a minimal picker — the "Start fresh" row plus a single hint line, e.g. `Tab — N session(s) in other projects`. One frame teaches the toggle.
- **Refs:** `crates/squeezy-tui/src/resume_picker.rs:573`, `crates/squeezy-tui/src/resume_picker.rs:491`, `crates/squeezy-tui/src/resume_picker.rs:418`

### 2. Cross-project directory switch is spelled out late and not echoed in the footer

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** Highlighting a cross-project row draws a hint at `layout[4]` — `Enter switches to {cwd} and resumes there` — but only while that exact row is active. The persistent footer's first line reads a generic `Enter confirm`, never mentioning that confirming a foreign row re-roots the workspace into another directory. The inline `↪ project` marker that distinguishes those rows can also truncate on a narrow terminal.
- **Friction:** The consequence (a working-directory change) is discoverable only on hover and is contradicted by the always-visible "confirm" wording. A user can press Enter on a cross-project row and not register that the cwd moved.
- **Polish:** When the cross-project view is active (`show_all_projects`), append a footer note such as `cross-project rows change your working directory`. (The draft's "pre-highlight Start fresh" ask is already satisfied — the cursor opens on the Start-fresh row at index 0.)
- **Refs:** `crates/squeezy-tui/src/resume_picker.rs:734`, `crates/squeezy-tui/src/resume_picker.rs:755`, `crates/squeezy-tui/src/resume_picker.rs:913`

### 3. Branch-load-failure `!` marker has no legend

- **Category · Severity · Effort:** Feedback · Medium · M
- **Today:** When a candidate's `events.jsonl` cannot be opened or parsed (e.g. a transient Windows file-lock), `load_candidates` sets `branch_load_failed` and the row renders a bare `!` in the warn color. The session still resumes normally — at the latest event — but the picker offers no key, footer hint, or explanation of what `!` means or what the user is losing.
- **Friction:** A lone `!` is cryptic. The user can't tell whether the session is corrupt, whether branches were lost, or whether resuming is safe.
- **Polish:** When any visible row has `branch_load_failed`, add a footer legend line, e.g. `! — branch data unavailable; resumes at latest event`. This is the honest, low-cost disambiguation; gating selection behind a confirmation overlay is optional and heavier.
- **Refs:** `crates/squeezy-tui/src/resume_picker.rs:79`, `crates/squeezy-tui/src/resume_picker.rs:861`, `crates/squeezy-tui/src/resume_picker.rs:900`, `crates/squeezy-tui/src/resume_picker.rs:541`

### 4. Annotation 280-char cap is invisible at input time and truncates silently

- **Category · Severity · Effort:** Friction · Low · S
- **Today:** The annotation edit modal appends every typed/pasted character to `annotation_edit` with no bound; the header renders `note: {buf}█ · Enter save · Esc cancel` with no counter. On commit, `normalise_text` clamps to `ANNOTATION_TEXT_LIMIT` (280) characters. A 500-char paste is accepted whole on screen, then silently loses its tail at save with no warning.
- **Friction:** The boundary is invisible until the user re-opens the note and notices the missing tail. The clamp happens off-screen at a different moment from the input.
- **Polish:** Render a counter in the edit header, e.g. `note: {buf}█  (123/280)`, switching to a warn color as it nears the cap. Optionally stop accepting chars past the limit so the on-screen buffer and the stored note agree.
- **Refs:** `crates/squeezy-tui/src/annotations.rs:36`, `crates/squeezy-tui/src/annotations.rs:82`, `crates/squeezy-tui/src/lib.rs:14875`, `crates/squeezy-tui/src/lib.rs:31420`

### 5. Bundle preview doesn't tell the user how to opt out of redaction

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** `/bundle` defaults to `redact: true`. The preview reports `redaction: redacted (N masked)` or `NOT redacted (local share only)`, and the usage hint is `usage: /bundle [md|json] [no-redact]`. Nothing in the preview connects the on-by-default redaction to the `no-redact` opt-out, so a user wanting a fully unmasked local bundle has to already know the keyword.
- **Friction:** The default is correct and safe, but the escape hatch is undiscoverable from the artifact the user is looking at.
- **Polish:** In the redacted case, append the opt-out to the preview line, e.g. `redaction: redacted (N masked) — pass no-redact for an unmasked local bundle`. Keeps the safe default while making the override visible at a glance.
- **Refs:** `crates/squeezy-tui/src/session_bundle.rs:185`, `crates/squeezy-tui/src/session_bundle.rs:210`, `crates/squeezy-tui/src/session_bundle.rs:227`

### 6. Corrupt/newer-schema checkpoint is dropped with no trace

- **Category · Severity · Effort:** Transparency · Low · S
- **Today:** `session_checkpoint::load` returns `None` uniformly for four cases: file missing, unreadable, unparseable, newer schema version, or session-id mismatch. The caller treats `None` as "no checkpoint, use defaults". A genuinely corrupt or future-version checkpoint is therefore indistinguishable from the common "no checkpoint exists" case, and the user's scroll/search/selection state is silently discarded.
- **Friction:** Low impact (checkpoints are UI convenience, not data), but it breaks predictability: a crash that corrupts the file means the session reopens at defaults with no signal that anything was lost.
- **Polish:** Log a single debug line for the *non-missing* failure paths only — e.g. `checkpoint ignored: schema {found} > {current}` or `checkpoint corrupt; using defaults` — keeping the missing-file happy path silent. Splitting the `None` reasons inside `load` is enough.
- **Refs:** `crates/squeezy-tui/src/session_checkpoint.rs:303`

### 7. Timeline filter cycle silently skips empty kinds with no state readout

- **Category · Severity · Effort:** Discoverability · Low · M
- **Today:** `cycle_filter` advances only through `present_kinds()` — the kinds that actually have events — so a session with no Tool events jumps straight from Prompt to Approval. The model exposes `filter()`, `present_kinds()`, and per-kind `count_of`, but nothing reports the active filter as the user cycles, so the list appears to skip positions for no visible reason.
- **Friction:** A user expecting to step through all ten `TimelineKind` variants sees the cycle land in unexpected places and can't tell that empty kinds are being skipped by design.
- **Polish:** On each cycle, surface the active filter in the overlay header / status line, e.g. `timeline: all` → `timeline: prompt (1/5 present)` → `timeline: tool (2/5)`. Teaches the present-kinds-only constraint and shows progress through the cycle.
- **Refs:** `crates/squeezy-tui/src/session_timeline.rs:355`, `crates/squeezy-tui/src/session_timeline.rs:379`, `crates/squeezy-tui/src/session_timeline.rs:346`

### 8. Micro-compaction clears tool outputs with no contemporaneous toast

- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** When micro-compaction rewrites bulky `FunctionCallOutput` bodies to the `[Old tool output cleared — call_id=…, name=…, original_bytes=…]` placeholder, the agent records a `context_micro_compacted` *session event* via `log_session_event` (carrying `cleared_call_ids` and `bytes_saved`) but the TUI surfaces no status line or toast for it. The placeholder itself is clearly self-describing, but it's only seen if the user later scrolls back to that output.
- **Friction:** The state change is invisible at the moment it happens. A user who later scrolls to a cleared output sees the placeholder with no link to when or why the bytes went away.
- **Polish:** Surface a transient status line when a micro-compaction pass clears anything, e.g. `cleared {N} old tool output(s), {bytes_saved} bytes reclaimed`. The report already carries both numbers; only the TUI-side toast is missing.
- **Refs:** `crates/squeezy-agent/src/micro_compaction.rs:100`, `crates/squeezy-agent/src/micro_compaction.rs:159`, `crates/squeezy-agent/src/lib.rs:6958`, `crates/squeezy-agent/src/lib.rs:9254`

### 9. Context summarization freezes the turn with no in-progress signal

- **Category · Severity · Effort:** Smoothness · Low · L
- **Today:** `compact_conversation_with_strategy` runs the extractive pass and, for the model-assisted strategy, a streamed summary call with a multi-second timeout — all synchronously within the turn. `/compact` sets a status line *after* the fact via `compaction_status_line`; while the pipeline runs (large conversations, the model-assisted round-trip) the UI shows nothing.
- **Friction:** For long sessions the input appears to hang during compaction even though work is progressing, with no spinner or phase text to reassure the user.
- **Polish:** Emit a transient status while compaction runs — `compacting context…`, and for the model-assisted path a `compacting (model)…` phase. The strategy branch points already exist as natural emit sites. Higher effort because it must thread a status update through the synchronous agent path.
- **Refs:** `crates/squeezy-agent/src/context_compaction.rs:691`, `crates/squeezy-agent/src/context_compaction.rs:788`, `crates/squeezy-tui/src/lib.rs:20292`, `crates/squeezy-tui/src/lib.rs:21260`

## Dropped from the draft

- **"Editor handoff doesn't confirm which action was taken"** — inaccurate. Each branch of `apply_editor_handoff_review` sets a distinct status line: Accept → `composer updated from editor`, Discard → `editor changes discarded`, Reopen → `reopening in editor …` (or `editor no longer configured — kept the edit`). The action is already confirmed. (`crates/squeezy-tui/src/lib.rs:15308`)
- **"Export unknown-format error doesn't show valid options inline"** — the error already inlines them. `parse_export_request` appends `EXPORT_USAGE` (`usage: /export <md|txt|json> …`), so `/export mdd` produces `unknown export format "mdd". usage: /export <md|txt|json> …`. The valid set is on screen; a "did you mean" rewrite is marginal. (`crates/squeezy-tui/src/export_destination.rs:107`)
- **"Checkpoint undo message wording inconsistent"** — too thin to be UX polish, and the premise is weak. `nothing to undo` is paired symmetrically with `nothing to revert` in the sibling handler, and these are tool-result JSON `message` fields, not user-facing status vocabulary. (`crates/squeezy-tools/src/checkpoints.rs:171`, `crates/squeezy-tools/src/checkpoints.rs:230`)
