//! Universal Command Palette (§12.1.1).
//!
//! One discoverable, fuzzy-searchable modal that lists *every* app command in a
//! single place: the rebindable keymap [`crate::keymap::Action`] registry (every
//! overlay toggle, copy verb, jump, layout knob, and diagnostic chord) plus the
//! slash-command help table ([`crate::input::SLASH_COMMANDS`]). It opens over the
//! fullscreen surface through the normal `render()` path, filters as the user
//! types, shows each command's label / description / current binding, and runs
//! the highlighted command with Enter (keyboard) or a click (mouse). Slash
//! commands that take a parameter are handed back to the composer as a
//! "second step" — the spec's parameter flow — rather than spawning a separate UI.
//!
//! This module is the pure, terminal-free model: it owns the entry list, the
//! query buffer, the fuzzy filter/order, and the cursor. It depends only on the
//! leaf modules ([`crate::keymap`], [`crate::input`], [`crate::fuzzy`]) and never
//! on `lib.rs`'s `TuiApp`, so every method is a pure function over model state and
//! is unit-testable without a `TuiApp` or a terminal. The wiring (a keybinding, a
//! render call, a dispatch arm, the overlay-open flag) lives in `lib.rs`.
//!
//! Zero idle cost: the palette is built only when the user opens it (the resting
//! state is a closed overlay that paints and allocates nothing), and the fuzzy
//! filter runs only while it is open.

use crate::fuzzy;
use crate::input::{SLASH_COMMANDS, SlashMenuVisibility};
use crate::keymap::{Action, KeymapResolver};

/// What running a palette entry does. Resolved at run time in `lib.rs`:
/// [`PaletteRun::Action`] re-dispatches the keymap action through the same
/// `dispatch_keymap_action` path the keybinding uses (so keyboard / palette /
/// click can never diverge); [`PaletteRun::Slash`] hands the command name back to
/// the composer (a parameterless command is ready to send, a parameter command is
/// pre-seeded for the user to complete — the spec's "second palette step").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteRun {
    /// Run the given rebindable keymap action.
    Action(Action),
    /// Hand the named slash command back to the composer. `has_parameter` is true
    /// when the command takes an argument (so the caller leaves a trailing space
    /// and parks the user in the composer instead of sending immediately).
    Slash {
        name: &'static str,
        has_parameter: bool,
    },
}

/// A single command offered by the palette. Carries the human label, the stable
/// id / short description, the current binding string (empty for a slash command,
/// which is invoked by name), an optional disabled reason, and the
/// [`PaletteRun`] that executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandEntry {
    /// The human-facing label shown in the list (e.g. "Toggle session timeline").
    pub(crate) label: String,
    /// A short, stable description: the action slug or the slash-command help.
    pub(crate) description: String,
    /// The current binding (e.g. "Ctrl+T", "Alt+9"), or empty for a slash command.
    pub(crate) binding: String,
    /// `Some(reason)` when the command cannot run in the current context; the row
    /// is shown dimmed and Enter/click report the reason instead of running it.
    pub(crate) disabled_reason: Option<String>,
    /// What running this entry does.
    pub(crate) run: PaletteRun,
}

impl CommandEntry {
    /// The text the fuzzy filter scores against: label + description + binding, so
    /// a query can match the human label ("timeline"), the slug ("session_timeline"),
    /// or the chord ("alt+9").
    fn haystack(&self) -> String {
        format!("{} {} {}", self.label, self.description, self.binding)
    }
}

/// The palette model: the full entry list, the live query buffer, and the cursor
/// into the *visible* (filtered) list. Built fresh on open via [`Self::build`];
/// the resting state holds nothing, so a never-opened palette costs zero.
#[derive(Debug, Clone)]
pub(crate) struct CommandPalette {
    entries: Vec<CommandEntry>,
    query: String,
    /// Cursor into the visible (filtered) list, clamped on every query change.
    selected: usize,
}

impl CommandPalette {
    /// Build the palette from the keymap resolver (for current bindings) and the
    /// compiled-in action / slash registries. The order is stable: keymap actions
    /// first (in [`Action::ALL`] order), then slash commands (in
    /// [`SLASH_COMMANDS`] order), so an empty query always lists the same set in
    /// the same order. `during_task` gates a slash command's availability the same
    /// way the composer menu does, surfacing a disabled reason rather than hiding it.
    /// `visibility` applies the same feature gates as the slash menu (e.g. checkpoint
    /// commands when checkpointing is off, `/reviewer` when Auto-review is off), so a
    /// gated command stays out of this parallel discovery surface too.
    pub(crate) fn build(
        keymap: &KeymapResolver,
        during_task: bool,
        visibility: SlashMenuVisibility,
    ) -> Self {
        let mut entries: Vec<CommandEntry> = Vec::new();
        for action in Action::ALL.iter().copied() {
            // Never list the palette's own toggle inside the palette — running it
            // from within would just close the surface you are looking at.
            if action == Action::ToggleCommandPalette {
                continue;
            }
            entries.push(CommandEntry {
                label: humanize_slug(action.slug()),
                description: action.slug().to_string(),
                binding: keymap.binding(action).display(),
                disabled_reason: None,
                run: PaletteRun::Action(action),
            });
        }
        for command in SLASH_COMMANDS.iter().copied() {
            // Commands gated behind a disabled feature (checkpoints off, reviewer
            // off) are hidden here exactly as they are in the slash menu, so the
            // palette never offers a command that cannot do anything yet.
            if !command.visible(visibility) {
                continue;
            }
            // A command that is not available during a task is disabled while a turn
            // is running (`during_task`); the row stays listed with an honest reason
            // rather than vanishing, matching the spec's "disabled reasons".
            let disabled_reason = (during_task && !command.available_during_task)
                .then(|| "unavailable while a turn is running".to_string());
            entries.push(CommandEntry {
                label: command.name.to_string(),
                description: command.description.to_string(),
                binding: String::new(),
                disabled_reason,
                run: PaletteRun::Slash {
                    name: command.name,
                    has_parameter: command.parameter_hint.is_some(),
                },
            });
        }
        Self {
            entries,
            query: String::new(),
            selected: 0,
        }
    }

    /// The full (unfiltered) entry count — diagnostic / test aid.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The current query buffer.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// The visible (fuzzy-filtered, score-ordered) entries for the current query.
    /// An empty query returns every entry in build order; a non-empty query keeps
    /// only entries whose `haystack` is a fuzzy match, ordered by descending score
    /// (ties broken by the stable build order, so the list never jitters).
    pub(crate) fn visible(&self) -> Vec<&CommandEntry> {
        if self.query.is_empty() {
            return self.entries.iter().collect();
        }
        let mut scored: Vec<(i32, usize, &CommandEntry)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                fuzzy::score(&entry.haystack(), &self.query).map(|score| (score, index, entry))
            })
            .collect();
        // Higher score first; ties keep the original (build) order via the index.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, entry)| entry).collect()
    }

    /// The number of visible (filtered) entries for the current query.
    pub(crate) fn visible_len(&self) -> usize {
        if self.query.is_empty() {
            self.entries.len()
        } else {
            self.entries
                .iter()
                .filter(|entry| fuzzy::score(&entry.haystack(), &self.query).is_some())
                .count()
        }
    }

    /// The cursor index into the visible list, clamped to a valid row.
    pub(crate) fn selected(&self) -> usize {
        let count = self.visible_len();
        if count == 0 {
            0
        } else {
            self.selected.min(count - 1)
        }
    }

    /// The cursor index clamped to a caller-supplied visible `count`. The render
    /// path computes the visible list once and passes its length here, so the
    /// cursor is clamped without a second full re-score of every entry.
    pub(crate) fn selected_within(&self, count: usize) -> usize {
        if count == 0 {
            0
        } else {
            self.selected.min(count - 1)
        }
    }

    /// The currently highlighted visible entry, if any.
    pub(crate) fn selected_entry(&self) -> Option<CommandEntry> {
        let selected = self.selected();
        self.visible().get(selected).map(|entry| (*entry).clone())
    }

    /// The visible entry at `index` (the click path resolves a row this way).
    pub(crate) fn entry_at(&self, index: usize) -> Option<CommandEntry> {
        self.visible().get(index).map(|entry| (*entry).clone())
    }

    /// Move the cursor up one visible row (clamped at the top).
    pub(crate) fn move_up(&mut self) {
        let selected = self.selected();
        self.selected = selected.saturating_sub(1);
    }

    /// Move the cursor down one visible row (clamped at the bottom).
    pub(crate) fn move_down(&mut self) {
        let count = self.visible_len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let selected = self.selected();
        self.selected = (selected + 1).min(count - 1);
    }

    /// Jump the cursor to the first visible row.
    pub(crate) fn move_to_top(&mut self) {
        self.selected = 0;
    }

    /// Jump the cursor to the last visible row (clamped via `visible_len`).
    pub(crate) fn move_to_bottom(&mut self) {
        self.selected = self.visible_len().saturating_sub(1);
    }

    /// Page the cursor a fixed step (10 rows) up or down, clamped to the visible
    /// list — a single PgUp/PgDn covers a screenful of the long command registry.
    pub(crate) fn page(&mut self, down: bool) {
        let count = self.visible_len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected();
        self.selected = if down {
            (cur + 10).min(count - 1)
        } else {
            cur.saturating_sub(10)
        };
    }

    /// Append a character to the query and re-park the cursor at the top of the
    /// freshly filtered list (the best match), so a narrowed list never strands
    /// the cursor past its end.
    pub(crate) fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
    }

    /// Delete the last query character (a no-op on an empty query) and re-park the
    /// cursor at the top of the re-filtered list.
    pub(crate) fn pop_char(&mut self) {
        self.query.pop();
        self.selected = 0;
    }
}

/// Turn a stable action slug into a human label: replace `_` with spaces and
/// capitalize the first letter (`transcript_overlay` -> "Transcript overlay").
/// Deterministic and allocation-light; derived from the slug so a new action
/// needs no separate label table to stay in sync.
fn humanize_slug(slug: &str) -> String {
    let spaced: String = slug
        .chars()
        .map(|ch| if ch == '_' { ' ' } else { ch })
        .collect();
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

#[cfg(test)]
#[path = "command_palette_tests.rs"]
mod tests;
