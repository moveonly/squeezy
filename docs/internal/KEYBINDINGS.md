# Keybindings

Squeezy's TUI key bindings are user-editable. Composer essentials
(`Enter`, `Backspace`, `Tab`, character input) stay hardcoded so submit
/ delete semantics behave the same under every focus. Everything in
the action namespace below resolves through a layered override surface
so a user whose terminal eats `Ctrl+T` (tmux is the canonical
offender) can pick a different key without forking the codebase.

## Layering

Resolution order, lowest precedence first:

1. **Compiled defaults** — `Action::default_binding` in
   `crates/squeezy-tui/src/keymap.rs`.
2. **`[tui.keymap]` in `settings.toml`** — merges across the user,
   project, and per-repo tiers using the existing config order.
3. **`~/.squeezy/keybindings.toml`** — a typed file whose entries are
   `[[bindings]]` rows with a `key` keyspec and an `action` slug:

   ```toml
   [[bindings]]
   key = "Ctrl+o"
   action = "transcript_overlay"

   [[bindings]]
   key = "Alt+k"
   action = "page_up"
   ```

The dedicated file wins on any action it touches; entries in the
`settings.toml` layer that the file does not touch survive unchanged.

Missing files are silent. A file that fails to parse, references an
unknown action slug, contains a malformed `key`, or tries to override
a reserved key surfaces a single typed error
(`crates/squeezy-tui/src/keymap_config.rs::KeybindingsError`). The TUI
logs the error and falls back to the base overrides — a broken
keybindings file must never block startup or half-apply a partial
config.

Path resolution looks for `$HOME/.squeezy/keybindings.toml`. On Windows,
where `$HOME` is typically unset, the loader falls back to
`$USERPROFILE/.squeezy/keybindings.toml`. When neither variable is set
(or both are empty — some CI sandboxes), the loader degrades to "no
user overrides" and the resolver runs on settings + defaults.

## Action namespace

Each action is a stable slug owned by `keymap::Action`. The slug
matches `Action::slug()` exactly so the file surface stays the same as
new variants are added.

| Slug                          | Default binding | Purpose                                                                 |
| ----------------------------- | --------------- | ----------------------------------------------------------------------- |
| `toggle_config_screen`        | `F11`           | Open or close the full-screen config browser.                           |
| `transcript_overlay`          | `Ctrl+T`        | Toggle the transcript overlay.                                          |
| `open_search`                 | `/`             | Open incremental transcript search on the active surface.               |
| `toggle_task_panel`           | `Ctrl+P`        | Expand or collapse the live task panel.                                 |
| `copy_last_assistant`         | `Ctrl+Y`        | Copy the last assistant response to the system clipboard.               |
| `copy_focused_entry`          | `Alt+C`         | Copy the current/focused transcript entry to the clipboard.             |
| `copy_tool_output`            | `Alt+O`         | Copy the current/nearest tool output to the clipboard.                  |
| `copy_code_block`             | `Alt+K`         | Copy the fenced code block under the cursor to the clipboard.           |
| `copy_all_code`               | `Alt+J`         | Copy every fenced code block of the focused entry (else the transcript).|
| `copy_viewport`               | `Alt+V`         | Copy the rows visible in the main viewport to the clipboard.            |
| `copy_full_transcript`        | `Alt+A`         | Copy the entire transcript to the clipboard.                            |
| `copy_selection`              | `Alt+Y`         | Copy the active visual selection to the clipboard.                      |
| `quote_selection_to_compose`  | `>`             | Quote the active selection into the composer as a Markdown blockquote.  |
| `add_selection_to_set`        | `Alt+D`         | Commit the live selection into the disjoint multi-selection set.        |
| `copy_multi_selection`        | `Ctrl+Alt+Y`    | Copy every committed disjoint range plus the live one as one payload.   |
| `save_snippet_from_selection` | `Alt+3`         | Save the active selection as a reusable named prompt snippet.           |
| `toggle_snippets`             | `Ctrl+Alt+S`    | Toggle the saved-snippets picker overlay.                               |
| `restore_cancelled_prompt`    | `Ctrl+R`        | Restore the last cancelled prompt into the composer.                    |
| `page_up`                     | `PageUp`        | Scroll the transcript one page up.                                      |
| `page_down`                   | `PageDown`      | Scroll the transcript one page down.                                    |
| `transcript_home`             | `Home`          | Jump to the top of the transcript when the composer is empty.           |
| `transcript_end`              | `End`           | Jump to the bottom of the transcript when the composer is empty.        |
| `jump_prev_user_turn`         | `Alt+Up`        | Jump the transcript to the previous user turn.                          |
| `jump_next_user_turn`         | `Alt+Down`      | Jump the transcript to the next user turn.                              |
| `jump_prev_assistant`         | `Alt+Left`      | Jump the transcript to the previous assistant answer.                   |
| `jump_next_assistant`         | `Alt+Right`     | Jump the transcript to the next assistant answer.                       |
| `jump_prev_tool_call`         | `Alt+,`         | Jump the transcript to the previous tool call.                          |
| `jump_next_tool_call`         | `Alt+.`         | Jump the transcript to the next tool call.                              |
| `jump_prev_error`             | `Alt+[`         | Jump the transcript to the previous error.                             |
| `jump_next_error`             | `Alt+]`         | Jump the transcript to the next error.                                 |
| `focus_prev_entry`            | `Ctrl+Up`       | Move the focused-entry cursor to the previous transcript entry.         |
| `focus_next_entry`            | `Ctrl+Down`     | Move the focused-entry cursor to the next transcript entry.             |
| `toggle_focused_fold`         | `Ctrl+O`        | Toggle the collapsed state of the focused inline transcript entry.      |
| `open_focused_in_detail`      | `Ctrl+Enter`    | Open the focused transcript entry in the detail overlay.                |
| `queue_undo`                  | `u`             | Undo the most recent prompt-queue mutation (queue reorder overlay only).|
| `toggle_latency_overlay`      | `Ctrl+Alt+L`    | Toggle the hidden per-interaction latency-budget overlay.               |
| `toggle_dogfood_metrics`      | `Ctrl+Alt+M`    | Toggle the hidden dogfood-telemetry `/metrics` snapshot overlay.        |
| `set_jump_mark`               | `Alt+M`         | Set a jump mark at the entry at the top of the viewport.                |
| `jump_to_mark`                | `Alt+'`         | Jump back to the most recently set jump mark.                           |
| `toggle_minimap`              | `Alt+R`         | Toggle the minimap turn rail.                                           |
| `toggle_soft_wrap`            | `Alt+W`         | Toggle the main view between soft-wrap and no-wrap horizontal scroll.   |
| `scroll_block_left`           | `Alt+H`         | Pan the no-wrap main view left one step.                                |
| `scroll_block_right`          | `Alt+L`         | Pan the no-wrap main view right one step.                               |
| `cycle_density`               | `Ctrl+Alt+X`    | Cycle the Adaptive Density override (auto → compact → default → expanded).|
| `cycle_dock_panel`            | `Ctrl+Alt+F`    | Cycle the dockable auxiliary panel across edges and back to undocked.   |
| `toggle_hyperlinks`           | `Alt+8`         | Cycle the OSC 8 hyperlink mode for rendered URLs/file paths.            |
| `toggle_clipboard_history`    | `Alt+P`         | Open or close the in-app clipboard-history picker.                      |
| `build_session_bundle`        | `Alt+B`         | Build a shareable session bundle (keyboard twin of `/bundle`).          |
| `open_composer_in_editor`     | `Alt+E`         | Open the composer text in the user's `$VISUAL`/`$EDITOR`.               |
| `cycle_semantic_filter`       | `Alt+F`         | Cycle the main-view Semantic Filter forward through its categories.     |
| `toggle_transcript_index`     | `Alt+I`         | Open or close the Local Transcript Index overlay.                       |
| `toggle_related_links`        | `Alt+G`         | Open or close the Related-Entry Links overlay.                          |
| `toggle_duplicate_folds`      | `Alt+U`         | Open or close the Duplicate-Output Folds overlay.                       |
| `toggle_error_lens`           | `Alt+X`         | Open or close the Error Lenses overlay.                                 |
| `toggle_health_markers`       | `Alt+N`         | Open or close the Transcript Health Markers overlay.                    |
| `toggle_turn_outline`         | `Alt+S`         | Open or close the Semantic Turn Outline overlay.                        |
| `toggle_lane_fold`            | `Alt+Z`         | Open or close the Collapsible Reasoning/Tool Lanes overlay.             |
| `toggle_pinned_compare`       | `Alt+T`         | Open or close the Pinned Compare View.                                  |
| `drop_bookmark`               | `Alt+;`         | Drop a Reading Position Bookmark at the top-of-viewport entry.          |
| `toggle_bookmarks`            | `Alt+Q`         | Open or close the Reading Position Bookmarks overlay.                   |
| `toggle_session_timeline`     | `Alt+9`         | Open or close the Session Timeline overlay.                            |
| `toggle_subagent_timeline`    | `Alt+5`         | Open or close the Subagent Timeline Panel.                              |
| `annotate_entry`              | `Alt+/`         | Annotate the focused transcript entry with a short private note.        |
| `toggle_annotations`          | `Alt+\`         | Open or close the Entry Annotations overlay.                            |
| `toggle_changes_since`        | `Alt+0`         | Mark a "What Changed Since Here?" point and open its delta overlay.     |
| `open_action_palette`         | `Alt+Enter`     | Open the Contextual Action Palette for the focused transcript unit.     |
| `toggle_command_palette`      | `Ctrl+Alt+P`    | Open or close the Universal Command Palette.                            |
| `toggle_hover_preview`        | `Alt+1`         | Open or close the Hover Preview popover for the focused unit.           |
| `toggle_hover_intent`         | `Ctrl+Alt+H`    | Toggle Mouse Hover Intent emphasis on the card under the pointer.       |
| `toggle_breadcrumbs`          | `Alt+2`         | Show or hide the Clickable Breadcrumbs strip.                           |
| `rename_focused_entry`        | `Ctrl+Alt+R`    | Rename/label the focused transcript entry inline.                       |
| `dismiss_first_run_hint`      | `Ctrl+Alt+N`    | Dismiss the gentle First-Run Interaction Hint currently shown.          |
| `preview_subagent`            | `Alt+6`         | Preview the selected subagent timeline row.                            |
| `jump_to_subagent`            | `Ctrl+Alt+D`    | Jump to the selected subagent's transcript/detail pane.                 |
| `promote_subagent_result`     | `Ctrl+Alt+Q`    | Promote the selected subagent's result into a follow-up prompt.         |
| `jump_to_attention`           | `Ctrl+Alt+Z`    | Quick-jump to the subagent that most needs attention.                   |
| `toggle_subagent_compare`     | `Alt+7`         | Open the Compare Subagent Outputs view over the two marked subagents.   |
| `toggle_review_board`         | `Ctrl+Alt+O`    | Open or close the Live Review Board overlay.                            |
| `toggle_tool_actions`         | `Ctrl+Alt+A`    | Open or close the Actionable Tool Outputs overlay for the focused result.|
| `toggle_scratchpad`           | `Alt+4`         | Open or close the Scratchpad Pane.                                      |
| `toggle_templates`            | `Ctrl+Alt+T`    | Open or close the Prompt Templates picker.                              |
| `toggle_macro_record`         | `Ctrl+Alt+K`    | Arm or disarm Replayable Interaction Macro recording.                   |
| `replay_macro`                | `Ctrl+Alt+J`    | Replay the most recently recorded macro.                                |
| `toggle_keybinding_editor`    | `Ctrl+Alt+B`    | Open or close the Keybinding Editor UI overlay.                         |
| `open_theme_editor`           | `Ctrl+Alt+E`    | Open or close the interactive theme color editor.                       |
| `open_workspace_profile`      | `Ctrl+Alt+W`    | Open or close the per-workspace UI-profile overlay.                     |
| `open_terminal_profile`       | `Ctrl+Alt+G`    | Open or close the per-terminal profile editor.                         |
| `open_gesture_settings`       | `Ctrl+Alt+I`    | Open or close the gesture-settings editor.                              |
| `open_glyph_mode`             | `Ctrl+Alt+U`    | Open or close the Minimal Glyph Mode picker.                            |
| `toggle_smart_split`          | `Ctrl+Alt+V`    | Open or close the Smart Split Panes layout inspector.                   |
| `toggle_presentation`         | `Ctrl+Alt+C`    | Toggle Presentation Mode (screen-share/demo display).                   |
| `toggle_zen_mode`             | `Ctrl+Alt+.`    | Toggle Zen Mode, the distraction-free layout.                           |
| `restore_terminal`            | `Ctrl+Alt+,`    | Forcibly return a wedged terminal to a sane state.                      |
| `toggle_layout_fallback_diag` | `Ctrl+Alt+/`    | Toggle the Last-Known-Good Layout Fallback diagnostics line.            |
| `accept_degraded_suggestion`  | `Ctrl+Alt+;`    | Accept the proactively-shown degraded-mode suggestion.                  |
| `dismiss_degraded_suggestion` | `Ctrl+Alt+'`    | Dismiss the proactively-shown degraded-mode suggestion.                 |
| `open_session_checkpoint`     | `Ctrl+Alt+[`    | Open or close the read-only session checkpoint status overlay.          |

Use `/keymap` inside the TUI to inspect the live resolution and see which
entries are overrides. The card also reports unknown action names, malformed
key specs, and binding collisions from the `[tui.keymap]` layer; the dedicated
`keybindings.toml` file remains all-or-nothing and is ignored as a whole if it
fails validation.

### `[terminal-dependent]` markers

`/keymap` annotates each row whose default is known to be intercepted by the
host terminal, tmux, or SSH with a `[terminal-dependent]` marker, and prints
a footer pointing at `[tui.keymap]` so the user knows how to remap. The
annotation is informational only — the binding still works when the terminal
allows it through. Today the marked defaults are `toggle_config_screen`
(`F11`, often eaten by the window manager), `transcript_overlay` (`Ctrl+T`,
collides with custom tmux prefixes), `toggle_task_panel` (`Ctrl+P`, common
editor binding), and `page_up` / `page_down` (`PageUp` / `PageDown`,
intercepted by some emulators for their own scrollback and unreliable over
SSH). The remaining defaults (`copy_last_assistant`, `restore_cancelled_prompt`,
`transcript_home`, `transcript_end`) are unmarked because they are broadly
portable across Linux terminals.

For a deeper view of the runtime — TTY state, `$TERM`, multiplexer detection,
synchronized-output policy, mouse-capture state, clipboard backend,
notification backend, and effective shell — run `/terminal` inside the TUI.
That command also lists the canonical remedies for `tmux` OSC52 passthrough,
mouse capture, and shell selection.

Transcript-overlay expansion is not currently part of the editable keymap
namespace. `transcript_overlay` (`Ctrl+T` by default) opens the expanded
overlay, expands a folded subagent overlay in place, and closes an already
expanded overlay. While the overlay is open, `Esc`, `PageUp`, `PageDown`, arrow
keys, `Home`, `End`, and bare `m` are modal overlay controls rather than
editable keymap actions. Keep any future expansion-specific binding in
`Action::ALL` before documenting it here.

## Key spec grammar

A `key` value is a `+`-separated chain of zero or more modifiers
followed by a single key token. Modifier names are case-insensitive;
single-character tokens keep their casing so shifted letters round-trip
cleanly through the report.

- Modifiers: `Ctrl` (also `Control`), `Alt` (also `Meta`, `Opt`,
  `Option`), `Shift`, `Super` (also `Cmd`, `Win`, `Windows`).
- Named keys: `Enter` / `Return`, `Tab`, `BackTab` /
  `Shift-Tab` / `ShiftTab`, `Esc` / `Escape`, `Space`,
  `Backspace` / `Bs`, `Delete` / `Del`, `Insert` / `Ins`, `Home`, `End`,
  `PageUp` / `PgUp`, `PageDown` / `PgDn`, `Left`, `Right`, `Up`, `Down`,
  `F1`..`F24`.
- Single characters: any one printable code point (`a`, `O`, `;`).

Examples: `Ctrl+T`, `Alt+k`, `Shift+Ctrl+P`, `F11`, `PageUp`, `Esc`.

## Reserved bindings

The application owns a small set of keys whose meaning is enforced
globally regardless of focus. The user keybindings file cannot rebind
any action onto them; the loader returns a typed `ReservedKey` error
and the TUI keeps the compiled-in defaults. Character matching is
case-insensitive so `Ctrl+C` and `Ctrl+c` are both refused.

| Reserved key | Reason                                      |
| ------------ | ------------------------------------------- |
| `Ctrl+C`     | Cancel the running turn / exit when idle.   |
| `Esc`        | Dismiss overlays and cancel chord prefixes. |
| `Ctrl+D`     | Composer EOF / exit when input is empty.    |

These are the only emergency exits from the TUI. Silently rebinding
any of them would strand the user with no way out, so the loader
refuses the override loudly instead.

The reserved set lives in `RESERVED_BINDINGS` in
`crates/squeezy-tui/src/keymap_config.rs`. Any change to the
locked-down keys must update this document.
