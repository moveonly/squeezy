//! TUI key rebinding: maps user-supplied key specs in `[tui.keymap]`
//! to a small set of named actions and resolves them at runtime.
//!
//! The audit (`tui-003`) flagged the hardcoded `Ctrl+T` / `Ctrl+P` /
//! `Ctrl+Y` / `PageUp` / etc. bindings as unaccessible to users who
//! collide with their host terminal (tmux Ctrl+T) or use non-QWERTY
//! layouts. The substrate here lets the user write
//!
//! ```toml
//! [tui.keymap]
//! transcript_overlay = "Ctrl+o"
//! page_up = "Alt+k"
//! ```
//!
//! and have those override the compiled-in defaults. `/keymap` lists
//! the current resolution so the user can verify what's bound.
//!
//! Scope is deliberately narrow: only the auxiliary actions (scroll,
//! overlay, copy-last, restore-prompt, …) are rebindable. Composer
//! basics (Enter, Esc, Backspace, character input) stay hardcoded
//! because rebinding them breaks every workflow.
//!
//! Unknown action slugs or unparseable specs are kept and surfaced
//! via `/keymap` so the user sees the validation problem instead of
//! a silent miss.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyModifiers};

/// A named action a user can rebind. The slug used in
/// `[tui.keymap]` matches `Action::slug()` exactly so the config-file
/// surface stays stable as variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Action {
    /// Open / close the full-screen config browser (`F11` default).
    ToggleConfigScreen,
    /// Open / close the transcript overlay (`Ctrl+T` default).
    ToggleTranscriptOverlay,
    /// Expand or collapse the live task panel (`Ctrl+P` default).
    ToggleTaskPanel,
    /// Copy the last assistant response to the system clipboard
    /// (`Ctrl+Y` default).
    CopyLastAssistant,
    /// Restore the most recently cancelled prompt back into the
    /// composer (`Ctrl+R` default).
    RestoreCancelledPrompt,
    /// Scroll the transcript one page up (`PageUp` default).
    ScrollTranscriptPageUp,
    /// Scroll the transcript one page down (`PageDown` default).
    ScrollTranscriptPageDown,
    /// Jump to the top of the transcript when the composer is empty
    /// (`Home` default; falls through to line-start otherwise).
    TranscriptHome,
    /// Jump to the bottom of the transcript when the composer is
    /// empty (`End` default; falls through to line-end otherwise).
    TranscriptEnd,
}

impl Action {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::ToggleConfigScreen => "toggle_config_screen",
            Self::ToggleTranscriptOverlay => "transcript_overlay",
            Self::ToggleTaskPanel => "toggle_task_panel",
            Self::CopyLastAssistant => "copy_last_assistant",
            Self::RestoreCancelledPrompt => "restore_cancelled_prompt",
            Self::ScrollTranscriptPageUp => "page_up",
            Self::ScrollTranscriptPageDown => "page_down",
            Self::TranscriptHome => "transcript_home",
            Self::TranscriptEnd => "transcript_end",
        }
    }

    pub(crate) const ALL: &'static [Action] = &[
        Action::ToggleConfigScreen,
        Action::ToggleTranscriptOverlay,
        Action::ToggleTaskPanel,
        Action::CopyLastAssistant,
        Action::RestoreCancelledPrompt,
        Action::ScrollTranscriptPageUp,
        Action::ScrollTranscriptPageDown,
        Action::TranscriptHome,
        Action::TranscriptEnd,
    ];

    pub(crate) fn from_slug(slug: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.slug() == slug)
    }

    /// Compiled-in default keybinding for the action. Mirrors what
    /// `handle_key` previously hardcoded, so a fresh install behaves
    /// exactly like the pre-`/keymap` build.
    pub(crate) fn default_binding(self) -> KeyBinding {
        match self {
            Self::ToggleConfigScreen => KeyBinding::new(KeyCode::F(11), KeyModifiers::NONE),
            Self::ToggleTranscriptOverlay => {
                KeyBinding::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
            }
            Self::ToggleTaskPanel => KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            Self::CopyLastAssistant => KeyBinding::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            Self::RestoreCancelledPrompt => {
                KeyBinding::new(KeyCode::Char('r'), KeyModifiers::CONTROL)
            }
            Self::ScrollTranscriptPageUp => KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
            Self::ScrollTranscriptPageDown => {
                KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE)
            }
            Self::TranscriptHome => KeyBinding::new(KeyCode::Home, KeyModifiers::NONE),
            Self::TranscriptEnd => KeyBinding::new(KeyCode::End, KeyModifiers::NONE),
        }
    }
}

/// A normalised `(KeyCode, KeyModifiers)` pair. Modifiers are stored
/// with `SHIFT` stripped from `KeyCode::Char` because the shift bit
/// usually shows up on uppercase letters but not on punctuation
/// (terminal-dependent). The eq compares on the canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KeyBinding {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl KeyBinding {
    pub(crate) fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers: normalise_modifiers(code, modifiers),
        }
    }

    /// Human-facing description: `"Ctrl+T"`, `"PageUp"`, `"Alt+k"`.
    /// Used by `/keymap` so the listing reads back the same syntax
    /// the user typed in the TOML file.
    pub(crate) fn display(&self) -> String {
        format_binding(self.code, self.modifiers)
    }
}

fn normalise_modifiers(code: KeyCode, modifiers: KeyModifiers) -> KeyModifiers {
    let mut out = modifiers;
    if let KeyCode::Char(ch) = code
        && ch.is_ascii_uppercase()
    {
        out.remove(KeyModifiers::SHIFT);
    }
    out
}

/// Compiled keymap: `key -> action` table plus the diagnostics needed
/// to render `/keymap` (per-action resolved binding, list of bad
/// overrides). Built once from `AppConfig` at TUI startup.
#[derive(Debug, Clone)]
pub(crate) struct KeymapResolver {
    by_key: HashMap<KeyBinding, Action>,
    bindings: BTreeMap<Action, KeyBinding>,
    /// Slugs that were not recognised as actions, kept verbatim so
    /// `/keymap` can warn instead of silently dropping them.
    pub(crate) unknown_actions: Vec<(String, String)>,
    /// Bindings the user supplied that did not parse as a keyspec,
    /// surfaced via `/keymap`.
    pub(crate) invalid_bindings: Vec<(String, String, String)>,
}

impl KeymapResolver {
    /// Build a resolver from a `[tui.keymap]` table (action_slug ->
    /// keyspec). Invalid entries are kept as diagnostics rather than
    /// hard-failing so a typo in one binding doesn't shadow every
    /// other one.
    pub(crate) fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut bindings: BTreeMap<Action, KeyBinding> = BTreeMap::new();
        for action in Action::ALL.iter().copied() {
            bindings.insert(action, action.default_binding());
        }
        let mut unknown_actions = Vec::new();
        let mut invalid_bindings = Vec::new();
        for (slug, spec) in overrides {
            let Some(action) = Action::from_slug(slug) else {
                unknown_actions.push((slug.clone(), spec.clone()));
                continue;
            };
            match parse_keyspec(spec) {
                Some(binding) => {
                    bindings.insert(action, binding);
                }
                None => {
                    invalid_bindings.push((slug.clone(), spec.clone(), action.slug().to_string()));
                }
            }
        }
        // Build the reverse lookup. If two actions land on the same
        // binding the alphabetically-earlier action wins so `/keymap`
        // and `lookup` agree on a deterministic pick; the loser keeps
        // its binding visible so `/keymap` can flag the collision.
        // Action's BTreeMap iteration is sorted, so the first insert
        // is the alphabetically-earliest.
        let mut by_key: HashMap<KeyBinding, Action> = HashMap::new();
        for (action, binding) in &bindings {
            by_key.entry(*binding).or_insert(*action);
        }
        Self {
            by_key,
            bindings,
            unknown_actions,
            invalid_bindings,
        }
    }

    pub(crate) fn lookup(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        let binding = KeyBinding::new(code, modifiers);
        self.by_key.get(&binding).copied()
    }

    pub(crate) fn binding(&self, action: Action) -> KeyBinding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or_else(|| action.default_binding())
    }

    /// True when more than one action resolves to the same key. Used
    /// by `/keymap` to flag conflicts; the resolver still picks a
    /// single winner via the reverse-lookup insertion order.
    pub(crate) fn collisions(&self) -> Vec<(KeyBinding, Vec<Action>)> {
        let mut groups: HashMap<KeyBinding, Vec<Action>> = HashMap::new();
        for (action, binding) in &self.bindings {
            groups.entry(*binding).or_default().push(*action);
        }
        let mut out: Vec<(KeyBinding, Vec<Action>)> = groups
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .collect();
        // Sort by the display string for deterministic `/keymap`
        // output across runs.
        out.sort_by_key(|entry| entry.0.display());
        for (_, actions) in &mut out {
            actions.sort();
        }
        out
    }
}

/// Parse a `"Ctrl+T"` / `"PageUp"` / `"Alt+k"` keyspec. Returns
/// `None` for anything we can't represent (so `/keymap` can flag it
/// and the default binding stays in effect).
pub(crate) fn parse_keyspec(spec: &str) -> Option<KeyBinding> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut key_part: Option<&str> = None;
    for raw_token in trimmed.split('+') {
        let token = raw_token.trim();
        if token.is_empty() {
            return None;
        }
        if eq_any_ignore_ascii_case(token, &["ctrl", "control"]) {
            modifiers |= KeyModifiers::CONTROL;
        } else if eq_any_ignore_ascii_case(token, &["alt", "meta", "opt", "option"]) {
            modifiers |= KeyModifiers::ALT;
        } else if token.eq_ignore_ascii_case("shift") {
            modifiers |= KeyModifiers::SHIFT;
        } else if eq_any_ignore_ascii_case(token, &["super", "cmd", "win", "windows"]) {
            modifiers |= KeyModifiers::SUPER;
        } else {
            if key_part.is_some() {
                // More than one non-modifier token isn't a valid
                // spec ("Ctrl+a+b" makes no sense).
                return None;
            }
            key_part = Some(token);
        }
    }
    let key = key_part?;
    let code = parse_keycode(key)?;
    Some(KeyBinding::new(code, modifiers))
}

fn parse_keycode(token: &str) -> Option<KeyCode> {
    if eq_any_ignore_ascii_case(token, &["enter", "return"]) {
        Some(KeyCode::Enter)
    } else if token.eq_ignore_ascii_case("tab") {
        Some(KeyCode::Tab)
    } else if eq_any_ignore_ascii_case(token, &["backtab", "shift-tab", "shifttab"]) {
        Some(KeyCode::BackTab)
    } else if eq_any_ignore_ascii_case(token, &["esc", "escape"]) {
        Some(KeyCode::Esc)
    } else if token.eq_ignore_ascii_case("space") {
        Some(KeyCode::Char(' '))
    } else if eq_any_ignore_ascii_case(token, &["backspace", "bs"]) {
        Some(KeyCode::Backspace)
    } else if eq_any_ignore_ascii_case(token, &["delete", "del"]) {
        Some(KeyCode::Delete)
    } else if eq_any_ignore_ascii_case(token, &["insert", "ins"]) {
        Some(KeyCode::Insert)
    } else if token.eq_ignore_ascii_case("home") {
        Some(KeyCode::Home)
    } else if token.eq_ignore_ascii_case("end") {
        Some(KeyCode::End)
    } else if eq_any_ignore_ascii_case(token, &["pageup", "pgup"]) {
        Some(KeyCode::PageUp)
    } else if eq_any_ignore_ascii_case(token, &["pagedown", "pgdn"]) {
        Some(KeyCode::PageDown)
    } else if token.eq_ignore_ascii_case("left") {
        Some(KeyCode::Left)
    } else if token.eq_ignore_ascii_case("right") {
        Some(KeyCode::Right)
    } else if token.eq_ignore_ascii_case("up") {
        Some(KeyCode::Up)
    } else if token.eq_ignore_ascii_case("down") {
        Some(KeyCode::Down)
    } else {
        // Function keys: F1..F24.
        if let Some(rest) = token.strip_prefix('f').or_else(|| token.strip_prefix('F')) {
            if let Ok(n) = rest.parse::<u8>()
                && (1..=24).contains(&n)
            {
                return Some(KeyCode::F(n));
            }
            return None;
        }
        // Single character: keep the user's casing so shifted
        // letters round-trip through `display()` cleanly.
        let mut chars = token.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(KeyCode::Char(ch))
    }
}

fn eq_any_ignore_ascii_case(token: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| token.eq_ignore_ascii_case(candidate))
}

fn format_binding(code: KeyCode, modifiers: KeyModifiers) -> String {
    let key = format_keycode(code);
    let mut out = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Alt");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Shift");
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str("Super");
    }
    if out.is_empty() {
        key
    } else {
        out.reserve(key.len() + 1);
        out.push('+');
        out.push_str(&key);
        out
    }
}

fn format_keycode(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(ch) => {
            let upper = ch.to_ascii_uppercase();
            upper.to_string()
        }
        other => format!("{other:?}"),
    }
}

/// Build the `/keymap` transcript card text — sorted list of
/// `action: KeySpec` rows plus a hint about how to override and a
/// validation block for any bad entries.
pub(crate) fn format_keymap_command(resolver: &KeymapResolver) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("Key bindings".to_string());
    lines.push("(override in settings.toml under [tui.keymap])".to_string());
    lines.push(String::new());
    let mut rows: Vec<(String, String, bool)> = Vec::new();
    for action in Action::ALL.iter().copied() {
        let binding = resolver.binding(action);
        let default = action.default_binding();
        rows.push((
            action.slug().to_string(),
            binding.display(),
            binding != default,
        ));
    }
    let max_slug = rows.iter().map(|(s, _, _)| s.len()).max().unwrap_or(0);
    for (slug, display, is_override) in &rows {
        let marker = if *is_override { " (override)" } else { "" };
        lines.push(format!(
            "{:<width$}  {}{}",
            slug,
            display,
            marker,
            width = max_slug
        ));
    }
    let collisions = resolver.collisions();
    if !collisions.is_empty() {
        lines.push(String::new());
        lines.push("Collisions:".to_string());
        for (binding, actions) in collisions {
            let mut names = String::new();
            for action in actions {
                if !names.is_empty() {
                    names.push_str(", ");
                }
                names.push_str(action.slug());
            }
            lines.push(format!("  {} → {}", binding.display(), names));
        }
    }
    if !resolver.unknown_actions.is_empty() {
        lines.push(String::new());
        lines.push("Unknown action names (ignored):".to_string());
        for (slug, spec) in &resolver.unknown_actions {
            lines.push(format!("  {slug} = {spec:?}"));
        }
    }
    if !resolver.invalid_bindings.is_empty() {
        lines.push(String::new());
        lines.push("Invalid key specs (default kept):".to_string());
        for (slug, spec, _) in &resolver.invalid_bindings {
            lines.push(format!("  {slug} = {spec:?}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "keymap_tests.rs"]
mod tests;
