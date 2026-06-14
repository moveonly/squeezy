# Config, Theming & Settings

> The `/config` three-tier (User/Repo/Local) settings screen and its sub-overlays — live filter, model picker, secret entry, inline theme editor, and MCP-server management — plus the layout knobs (density, zen) that live outside it.

**How it works today:** `/config` renders a tabbed three-tier view (User/Repo/Local) over `CONFIG_SECTIONS`; Enter opens a per-kind editor and Space cycles bool/enum fields, both saving immediately and routing the write to the correct tier. Env-shadowed fields refuse edits and render a magenta `[env]` badge, the model picker shows its provider scope and committable custom ids, and the Themes section lists named themes plus per-token RGB rows. Destructive actions (per-tier Reset, session-wide Discard) gate behind y/n confirmations, and MCP servers can be added/toggled/restarted/removed with live status glyphs. A separate, richer channel-based theme editor and the density/zen layout policies exist but are reached only by keybinding, not from `/config`.

## Quick wins

- [Discard-all confirmation lists files but never shows what values revert](#1-discard-all-confirmation-lists-files-but-never-shows-what-values-revert)
- [Density and Zen modes are invisible in the settings screen](#2-density-and-zen-modes-are-invisible-in-the-settings-screen)
- [A successful Space-cycle is silent while a failed one is noisy](#4-a-successful-space-cycle-is-silent-while-a-failed-one-is-noisy)
- [MCP transport row reads as free text, not a cycle control](#5-mcp-transport-row-reads-as-free-text-not-a-cycle-control)
- [Model-picker capability tags have no legend](#6-model-picker-capability-tags-have-no-legend)

## Findings

### 1. Discard-all confirmation lists files but never shows what values revert

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** `Shift+X` arms the "Discard all session writes" y/n overlay. It shows the write count and the affected tier-file paths, then the line "Each tier file is restored to the bytes captured when /config opened." The per-tier Reset confirmation, by contrast, renders a full field-by-field `before → after` diff.
- **Friction:** The user sees *which files* are touched but gets no sense of *what changes* are about to be undone before a session-wide, irreversible revert. The data exists — `undo_stack` holds each write's pre-write bytes — but the overlay never decodes it into a preview, so Discard is the more destructive of the two confirmations yet the less informative.
- **Polish:** Add a sampled, capped preview of the most significant reverts (mirroring Reset's diff), e.g. `model → gpt-4o · permissions.mode → custom · … (and 12 more)`, so the user confirms against impact, not just a file list.
- **Refs:** `crates/squeezy-tui/src/config_screen/render.rs:1253-1327`, `crates/squeezy-tui/src/config_screen.rs:235,239`

### 2. Density and Zen modes are invisible in the settings screen

- **Category · Severity · Effort:** Discoverability · Medium · S
- **Today:** Adaptive Density (`Auto`/`Compact`/`Default`/`Expanded`) and Zen mode are layout policies persisted at `[tui].density` and `[tui].zen`, but neither appears in `CONFIG_SECTIONS` — the "Verbosity & TUI" section exposes `theme`, `spinner`, `tick_rate`, `transcript_default`, etc., but not these two. They are reachable only via keybinding (`Ctrl+Alt+,` density cycle, `Ctrl+Alt+.` zen toggle).
- **Friction:** Two real customization knobs that change the whole surface layout have no discovery path in the centralized settings screen. A user who never learns the chords never finds them, and `/config` — the obvious place to look — silently omits them.
- **Polish:** Surface them in the existing "Verbosity & TUI" section. Even read-only info rows showing the current value plus the cycle/toggle chord (`density · auto · Ctrl+Alt+, to cycle`) close the discovery gap without inventing a new section or tier; making them editable Enum/Bool fields is the fuller fix.
- **Refs:** `crates/squeezy-tui/src/density.rs:37-50`, `crates/squeezy-tui/src/zen.rs:40-48`, `crates/squeezy-core/src/config_schema.rs:1017-1208`

### 3. Reset confirmation truncates its change preview at 12 rows

- **Category · Severity · Effort:** Friction · Medium · M
- **Today:** The per-tier Reset y/n overlay previews every key whose effective value will change after deleting the tier file, but the list is hard-capped at 12 rows with a trailing "… and N more". The overlay has no scroll, page, or expand affordance, even though the model picker and the live filter both scroll their lists.
- **Friction:** On a tier with many overrides the user cannot review the full set of changes before a destructive, file-deleting confirmation. Truncating the preview on the one screen whose job is to build confidence in a delete undercuts the safety the confirmation exists to provide.
- **Polish:** Make the preview scrollable inside the overlay (reuse the windowed-scroll helper the filter/picker already use), or paginate it ("showing 1–12 of 47"). Full visibility matters most for the destructive path.
- **Refs:** `crates/squeezy-tui/src/config_screen/render.rs:1176-1210`, `crates/squeezy-tui/src/config_screen.rs:1141`

### 4. A successful Space-cycle is silent while a failed one is noisy

- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** Space on a bool/enum field cycles the value through `save_field_silent` (`silent = true`), so the row repaints with the new value but no transcript message. When Space *can't* cycle a field, the handler pushes "Space doesn't cycle X — press Enter to edit." The two outcomes are thus asymmetric: success says nothing, refusal speaks up.
- **Friction:** The row value does update, so the change isn't fully invisible — but for a state-changing keypress the lack of any confirmation, paired with a loud refusal message on the adjacent fields, makes a working cycle feel less acknowledged than a no-op. Saves elsewhere on the screen (Enter commits) do emit a "saved …" line, so the cycle path is the odd one out.
- **Polish:** Emit a terse confirmation on a successful cycle, matching the Enter-commit path — e.g. `coalesce_tool_runs: false → true` — so the success is acknowledged and the success/refusal pair reads consistently.
- **Refs:** `crates/squeezy-tui/src/config_screen/keys.rs:232-243,272-282`, `crates/squeezy-tui/src/config_screen/save.rs:584-594,826-925`

### 5. MCP transport row reads as free text, not a cycle control

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** In the "Add MCP server" overlay the `transport` row renders its current value (`stdio`/`http`/`sse`) exactly like the free-text `name`/`command`/`url` rows above it — same column layout, same accent styling when focused. Space cycles it (and the footer says "Space cycles transport"), but the row itself gives no visual cue that it is a selector rather than a text field.
- **Friction:** The value *is* shown, so the user can read the current transport — but nothing distinguishes the one cyclable row from the three editable ones, so a user may type into it (inserting a literal space) or not realize Space is the way to change it.
- **Polish:** Render the transport row as an inline option set with the active choice bracketed, e.g. `[stdio]  http  sse`, so it reads as a cycle control at a glance and the live selection is unmistakable.
- **Refs:** `crates/squeezy-tui/src/config_screen/render.rs:736-765`, `crates/squeezy-tui/src/config_screen/keys.rs:1642-1644`

### 6. Model-picker capability tags have no legend

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The model picker appends green capability tags to each row — `[pcache] [rsn] [vis] [tools] [json]` — derived from the registry's per-model capabilities. The footer documents only navigation (`Type filter · ↑/↓ move · Enter commit · Tab all-providers · Esc cancel`); nothing explains what the abbreviations mean.
- **Friction:** A user comparing models sees cryptic four-letter tags with no key. `pcache` (prompt caching), `rsn` (reasoning effort), and `vis` (vision) are not self-evident, so the one place these capabilities surface is also the place they're least legible.
- **Polish:** Add a one-line legend to the picker footer (or expand the tags to short words on wide terminals), e.g. `tags: pcache=prompt-cache · rsn=reasoning · vis=vision · tools · json`.
- **Refs:** `crates/squeezy-tui/src/config_screen/render.rs:1677-1704`

### 7. Live filter ignores a single-character query

- **Category · Severity · Effort:** Smoothness · Low · S
- **Today:** The filter narrows results only once the query reaches `FILTER_MIN_QUERY = 2` characters; below that the box stays open showing the panel list and the hint "keep typing to filter…". Typing `p` shows the full panel list; typing `pr` narrows to the matching rows. Description matching is separately gated at `HELP_MIN_QUERY = 3`.
- **Friction:** Single-character name matches (`p` → `permissions`, `provider`) are highly selective, so the two-character floor is a small dead zone — the first keystroke visibly does nothing to the list. The floor is a deliberate choice (a stray first key shouldn't collapse the view to noise), so this is a tradeoff, not a bug.
- **Polish:** Lower `FILTER_MIN_QUERY` to 1 so name matching activates on the first keystroke, while leaving `HELP_MIN_QUERY = 3` to keep the permissive description/blurb matching (where a 1–2 char substring is too broad) from flooding the list.
- **Refs:** `crates/squeezy-tui/src/config_screen.rs:2000-2010,2035-2047`

### 8. New-theme name generation hard-stops at 1000

- **Category · Severity · Effort:** Friction · Low · S
- **Today:** Creating a theme auto-generates a free name by iterating `custom-theme`, `custom-theme-2`, … up to 999. If all are taken, the loop falls back to the bare `custom-theme`, which then fails the duplicate check on commit with "Theme custom-theme already exists."
- **Friction:** A pathological edge case (1000+ custom themes), but the failure is opaque: the error names a collision, not exhaustion, so the user can't tell why creation refused or what to do.
- **Polish:** Either remove the ceiling (loop until a free name is found) or, on exhaustion, return a self-explaining error ("Too many custom themes — delete some first") instead of a name guaranteed to collide.
- **Refs:** `crates/squeezy-tui/src/config_screen/keys.rs:959-971`

### 9. Editing a theme color from /config drops you into text entry, not the live channel picker

- **Category · Severity · Effort:** Smoothness · Low · M
- **Today:** Two color-editing experiences exist. The standalone Theme Editor (`OpenThemeEditor` keymap verb) is a channel-based picker: `↑↓` role, `←→` channel, `+/-` adjust, with the change applied as a *live preview* and reverted on close-without-save, plus mouse-draggable channel bars. The `/config` Themes section, by contrast, opens an inline `ThemeEditor::Rgb` that is a plain `r,g,b` text draft — type three comma-separated numbers and the change is visible only after Enter commits.
- **Friction:** A user tuning a color from the obvious place (the Themes section in `/config`) gets the clunkier, blind text path, while the discoverable-only standalone editor offers immediate visual feedback. The nicer experience also isn't surfaced from `/config`, so the user has no cue it exists.
- **Polish:** Either route the `/config` color row into the existing channel picker (live preview, no blind typing) or, at minimum, point at it from the color-row help ("Ctrl+Alt+… for the live RGB editor"). Reusing the standalone editor avoids maintaining two divergent RGB inputs.
- **Refs:** `crates/squeezy-tui/src/config_screen/keys.rs:846-858,1051-1072`, `crates/squeezy-tui/src/theme_editor.rs:292-309`, `crates/squeezy-tui/src/lib.rs:5207-5236`

### 10. API-key reveal is all-or-nothing, with no last-four check

- **Category · Severity · Effort:** Friction · Low · M
- **Today:** The secret-entry overlay's `reveal` is a single `bool`: F2 flips between fully masked (`••••`) and full plaintext. There is no partial mode. (The struct doc even claims "an optional last-four reveal", but the render only ever masks all or shows all.)
- **Friction:** To sanity-check a pasted key by its suffix — the common "did the right key land?" check — the user must expose the entire secret on screen. The standard password-manager middle ground (show only the tail) is absent.
- **Polish:** Replace the `bool` with a three-state reveal (`Hidden` / `LastFour` / `Full`) cycled by F2, rendering `••••…abc123` in the middle state, so the user can verify the tail without exposing the whole key.
- **Refs:** `crates/squeezy-tui/src/config_screen.rs:443-459`, `crates/squeezy-tui/src/config_screen/render.rs:1358-1365`, `crates/squeezy-tui/src/config_screen/keys.rs:1166-1168`

### 11. Gesture settings show the new value but not its behavioral effect

- **Category · Severity · Effort:** Smoothness · Low · M
- **Today:** The Gesture Settings editor adjusts scroll lines, dwell time, double-click action, and drag-select via `+/-`. Each adjustment updates the displayed value and status line live (`adjust_focused` returns the working settings), but the runtime gesture behavior changes only when the user commits — the key handlers don't apply the working value to the live scroll/hover/drag path.
- **Friction:** The numeric value updates, but the *feel* (how aggressive a 200ms dwell is, how far a wheel tick scrolls) can't be tested without committing, closing, trying it, and reopening to readjust. The edit-feel-readjust loop is slow for settings whose whole purpose is tactile.
- **Polish:** Apply the working gesture settings live while the editor is open (preview, reverted on cancel like the standalone theme editor does for color), or add a small in-overlay demo area that responds to wheel/hover under the current values, so the user feels the change as they tune it.
- **Refs:** `crates/squeezy-tui/src/gesture_settings.rs:457-470`, `crates/squeezy-tui/src/lib.rs:6165-6190,6213-6229`

---

**Dropped as inaccurate after grounding:** the draft's "env-shadowed fields lack a marker" (a magenta `[env]` badge *is* rendered via `inheritance_label`/`source_style`), "config saves don't trigger a redraw" (every config keypress sets `needs_redraw` at `lib.rs:2510`), "model-picker Tab switches scope invisibly" (the picker header shows `scope: <provider>` / `scope: all providers`), "keybinding conflict warnings are subtle" (rendered in bold `warn()` color at `lib.rs:26325-26336`), "MCP status animation is decoupled from wall-clock and flickers" (the tick is the 50ms event-loop cadence, the blink is a deliberate ~500ms, and it freezes only when unfocused — which also stops redraws), and "theme editor opens with no cancel/context affordance" (the inline editor already renders the token name and an `Esc to cancel` hint).
