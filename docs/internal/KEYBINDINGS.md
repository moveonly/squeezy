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

Path resolution looks for `$HOME/.squeezy/keybindings.toml`. When
`$HOME` is unset (some CI sandboxes), the loader degrades to "no user
overrides" and the resolver runs on settings + defaults.

## Action namespace

Each action is a stable slug owned by `keymap::Action`. The slug
matches `Action::slug()` exactly so the file surface stays the same as
new variants are added.

| Slug                                | Default binding | Purpose                                                                 |
| ----------------------------------- | --------------- | ----------------------------------------------------------------------- |
| `toggle_config_screen`              | `F11`           | Open or close the full-screen config browser.                           |
| `transcript_overlay`                | `Ctrl+T`        | Toggle the transcript overlay.                                          |
| `toggle_task_panel`                 | `Ctrl+P`        | Expand or collapse the live task panel.                                 |
| `copy_last_assistant`               | `Ctrl+Y`        | Copy the last assistant response to the system clipboard.               |
| `restore_cancelled_prompt`          | `Ctrl+R`        | Restore the last cancelled prompt into the composer.                    |
| `page_up`                           | `PageUp`        | Scroll the transcript one page up.                                      |
| `page_down`                         | `PageDown`      | Scroll the transcript one page down.                                    |
| `transcript_home`                   | `Home`          | Jump to the top of the transcript when the composer is empty.           |
| `transcript_end`                    | `End`           | Jump to the bottom of the transcript when the composer is empty.        |

Use `/keymap` inside the TUI to inspect the live resolution and see
which entries are overrides.

Transcript-overlay expansion is not currently part of the editable keymap
namespace. `transcript_overlay` (`Ctrl+T` by default) opens the expanded
overlay, expands a folded subagent overlay in place, and closes an already
expanded overlay. Keep any future expansion-specific binding in
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
