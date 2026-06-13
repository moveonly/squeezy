//! Unit tests for the Universal Command Palette (§12.1.1) model: entry sourcing
//! from the keymap action registry + the slash-command help table, fuzzy
//! filtering and score ordering, cursor movement over the visible list, the query
//! buffer, disabled reasons by context, and the visible-index lookup the click
//! path uses. Pure over the model — no terminal, no `TuiApp`. The end-to-end
//! keyboard / mouse / render coverage lives in `lib_tests.rs`.

use std::collections::BTreeMap;

use super::*;
use crate::keymap::{Action, KeymapResolver};

fn resolver() -> KeymapResolver {
    KeymapResolver::from_overrides(&BTreeMap::new())
}

/// All feature gates on, so every slash command is offered — the baseline most
/// tests want when they are not exercising gating itself.
fn all_visible() -> crate::input::SlashMenuVisibility {
    crate::input::SlashMenuVisibility {
        checkpoints_enabled: true,
        reviewer_enabled: true,
    }
}

/// Build the palette with no running turn (the common idle case).
fn idle_palette() -> CommandPalette {
    CommandPalette::build(&resolver(), false, all_visible())
}

#[test]
fn build_sources_both_registries_and_omits_its_own_toggle() {
    let palette = idle_palette();
    // Every rebindable keymap action except the palette's own toggle, plus every
    // slash command, is listed — so the count is (actions - 1) + slash commands.
    let action_entries = palette
        .visible()
        .iter()
        .filter(|e| matches!(e.run, PaletteRun::Action(_)))
        .count();
    let slash_entries = palette
        .visible()
        .iter()
        .filter(|e| matches!(e.run, PaletteRun::Slash { .. }))
        .count();
    assert_eq!(
        action_entries,
        Action::ALL.len() - 1,
        "every keymap action except the palette toggle should be listed"
    );
    assert_eq!(slash_entries, crate::input::SLASH_COMMANDS.len());
    assert_eq!(palette.len(), action_entries + slash_entries);

    // The palette's own toggle must never appear inside it.
    assert!(
        !palette
            .visible()
            .iter()
            .any(|e| e.run == PaletteRun::Action(Action::ToggleCommandPalette)),
        "the palette must not list its own toggle"
    );
}

#[test]
fn palette_hides_gated_commands_until_their_feature_is_enabled() {
    // The palette is a second discovery surface (Ctrl-shortcut), so it must apply
    // the same feature gates as the slash menu — otherwise a newcomer reaches a
    // command that cannot do anything yet.
    let slash_names = |palette: &CommandPalette| -> Vec<String> {
        palette
            .visible()
            .iter()
            .filter_map(|entry| match entry.run {
                PaletteRun::Slash { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect()
    };

    let all_off = crate::input::SlashMenuVisibility {
        checkpoints_enabled: false,
        reviewer_enabled: false,
    };
    let gated_off = CommandPalette::build(&resolver(), false, all_off);
    let off_names = slash_names(&gated_off);
    for hidden in [
        "/reviewer",
        "/checkpoints",
        "/checkpoint",
        "/undo",
        "/revert-turn",
    ] {
        assert!(
            !off_names.contains(&hidden.to_string()),
            "{hidden} must not be listed in the palette while its feature is off: {off_names:?}"
        );
    }
    // Ungated commands are unaffected.
    assert!(off_names.contains(&"/cost".to_string()));

    let gated_on = CommandPalette::build(&resolver(), false, all_visible());
    let on_names = slash_names(&gated_on);
    for shown in ["/reviewer", "/checkpoints", "/undo", "/revert-turn"] {
        assert!(
            on_names.contains(&shown.to_string()),
            "{shown} should be listed once its feature is on"
        );
    }
}

#[test]
fn action_entries_carry_humanized_label_and_current_binding() {
    let palette = idle_palette();
    let entry = palette
        .visible()
        .iter()
        .find(|e| e.run == PaletteRun::Action(Action::ToggleSessionTimeline))
        .copied()
        .expect("session timeline action listed");
    // The slug `toggle_session_timeline` humanizes to "Toggle session timeline".
    assert_eq!(entry.label, "Toggle session timeline");
    assert_eq!(entry.description, "toggle_session_timeline");
    // The binding column reflects the resolver's current binding (Alt+9 default).
    assert_eq!(entry.binding, "Alt+9");
    assert!(entry.disabled_reason.is_none());
}

#[test]
fn binding_column_reflects_user_override() {
    let mut overrides = BTreeMap::new();
    overrides.insert("toggle_session_timeline".to_string(), "Ctrl+G".to_string());
    let palette = CommandPalette::build(
        &KeymapResolver::from_overrides(&overrides),
        false,
        all_visible(),
    );
    let entry = palette
        .visible()
        .iter()
        .find(|e| e.run == PaletteRun::Action(Action::ToggleSessionTimeline))
        .copied()
        .expect("session timeline action listed");
    assert_eq!(
        entry.binding, "Ctrl+G",
        "the palette must show the user's overridden binding"
    );
}

#[test]
fn empty_query_lists_everything_in_build_order() {
    let palette = idle_palette();
    assert_eq!(palette.visible_len(), palette.len());
    // First entry is the first keymap action in ALL order that is not the palette
    // toggle — ToggleConfigScreen sorts first in ALL.
    let first = palette.visible()[0];
    assert_eq!(first.run, PaletteRun::Action(Action::ToggleConfigScreen));
}

#[test]
fn fuzzy_query_filters_and_orders_by_score() {
    let mut palette = idle_palette();
    for ch in "timeline".chars() {
        palette.push_char(ch);
    }
    let visible = palette.visible();
    assert!(!visible.is_empty(), "‘timeline’ should match something");
    // Every surviving entry is a genuine fuzzy match of the query.
    for entry in &visible {
        assert!(
            crate::fuzzy::score(
                &format!("{} {} {}", entry.label, entry.description, entry.binding),
                "timeline"
            )
            .is_some(),
            "{:?} should fuzzy-match the query",
            entry.label
        );
    }
    // The Session Timeline command (label + slug both contain "timeline") should be
    // the top match.
    assert_eq!(
        visible[0].run,
        PaletteRun::Action(Action::ToggleSessionTimeline),
        "the exact ‘timeline’ command should rank first"
    );
    // The filtered set is strictly smaller than the full list.
    assert!(palette.visible_len() < palette.len());
}

#[test]
fn query_can_match_the_binding_chord() {
    // A user who remembers the chord but not the name can search by binding.
    let mut palette = idle_palette();
    for ch in "alt+9".chars() {
        palette.push_char(ch);
    }
    assert!(
        palette
            .visible()
            .iter()
            .any(|e| e.run == PaletteRun::Action(Action::ToggleSessionTimeline)),
        "searching the chord ‘alt+9’ should surface its command"
    );
}

#[test]
fn no_match_yields_empty_visible_and_safe_cursor() {
    let mut palette = idle_palette();
    for ch in "zzqqxx-not-a-command".chars() {
        palette.push_char(ch);
    }
    assert_eq!(palette.visible_len(), 0);
    assert!(palette.visible().is_empty());
    // The cursor and selected entry are safe on an empty filtered list.
    assert_eq!(palette.selected(), 0);
    assert!(palette.selected_entry().is_none());
    // Moving the cursor on an empty list never panics or escapes.
    palette.move_down();
    palette.move_up();
    assert_eq!(palette.selected(), 0);
}

#[test]
fn cursor_moves_and_clamps_within_visible_list() {
    let mut palette = idle_palette();
    assert_eq!(palette.selected(), 0);
    palette.move_up();
    assert_eq!(palette.selected(), 0, "move_up clamps at the top");

    palette.move_down();
    assert_eq!(palette.selected(), 1);

    // Walk far past the end; the cursor clamps to the last visible row.
    for _ in 0..(palette.len() + 10) {
        palette.move_down();
    }
    assert_eq!(palette.selected(), palette.visible_len() - 1);
}

#[test]
fn paging_and_home_end_jump_within_visible_list() {
    let mut palette = idle_palette();
    let last = palette.visible_len() - 1;

    // End jumps to the last visible row, Home back to the first.
    palette.move_to_bottom();
    assert_eq!(palette.selected(), last);
    palette.move_to_top();
    assert_eq!(palette.selected(), 0);

    // A page down advances a fixed step (10) and clamps at the bottom.
    palette.page(true);
    assert_eq!(palette.selected(), 10);
    palette.page(false);
    assert_eq!(palette.selected(), 0, "page up clamps at the top");
    for _ in 0..palette.len() {
        palette.page(true);
    }
    assert_eq!(palette.selected(), last, "page down clamps at the bottom");
    palette.page(false);
    assert_eq!(palette.selected(), last.saturating_sub(10));
}

#[test]
fn paging_and_home_end_are_safe_on_an_empty_list() {
    let mut palette = idle_palette();
    for ch in "zzqqxx-not-a-command".chars() {
        palette.push_char(ch);
    }
    assert_eq!(palette.visible_len(), 0);
    palette.move_to_bottom();
    assert_eq!(palette.selected(), 0);
    palette.page(true);
    palette.page(false);
    palette.move_to_top();
    assert_eq!(palette.selected(), 0);
}

#[test]
fn typing_reparks_cursor_at_top_of_filtered_list() {
    let mut palette = idle_palette();
    palette.move_down();
    palette.move_down();
    assert_eq!(palette.selected(), 2);
    // Typing narrows the list and re-parks the cursor at the best match.
    palette.push_char('t');
    assert_eq!(palette.selected(), 0);
    // Backspace widens the list and likewise re-parks the cursor.
    palette.move_down();
    palette.pop_char();
    assert_eq!(palette.selected(), 0);
}

#[test]
fn pop_char_on_empty_query_is_a_noop() {
    let mut palette = idle_palette();
    assert_eq!(palette.query(), "");
    palette.pop_char();
    assert_eq!(palette.query(), "");
    assert_eq!(palette.visible_len(), palette.len());
}

#[test]
fn entry_at_resolves_the_visible_index_for_the_click_path() {
    let mut palette = idle_palette();
    for ch in "copy".chars() {
        palette.push_char(ch);
    }
    let visible = palette.visible();
    assert!(!visible.is_empty());
    let last = visible.len() - 1;
    let by_index = palette.entry_at(last).expect("entry at last visible index");
    assert_eq!(&by_index, visible[last]);
    // Out-of-range index resolves to nothing (the click path then reports it).
    assert!(palette.entry_at(visible.len()).is_none());
}

#[test]
fn slash_commands_are_disabled_during_a_task_with_an_honest_reason() {
    // A command that is NOT available during a task must carry a disabled reason
    // when the palette is built while a turn is running, and be runnable when idle.
    let blocked = crate::input::SLASH_COMMANDS
        .iter()
        .find(|c| !c.available_during_task)
        .expect("at least one command is task-blocked");

    let during_task = CommandPalette::build(&resolver(), true, all_visible());
    let entry = during_task
        .visible()
        .iter()
        .find(|e| {
            e.run
                == PaletteRun::Slash {
                    name: blocked.name,
                    has_parameter: blocked.parameter_hint.is_some(),
                }
        })
        .copied()
        .expect("task-blocked command still listed during a task");
    assert!(
        entry.disabled_reason.is_some(),
        "a task-blocked command must show a disabled reason during a task"
    );

    let idle = idle_palette();
    let idle_entry = idle
        .visible()
        .iter()
        .find(|e| {
            e.run
                == PaletteRun::Slash {
                    name: blocked.name,
                    has_parameter: blocked.parameter_hint.is_some(),
                }
        })
        .copied()
        .expect("task-blocked command listed when idle");
    assert!(
        idle_entry.disabled_reason.is_none(),
        "the same command must be runnable when no turn is running"
    );
}

#[test]
fn slash_run_records_whether_the_command_takes_a_parameter() {
    let palette = idle_palette();
    // `/help` takes a parameter; find its entry and confirm the run payload.
    let help = palette
        .visible()
        .iter()
        .find(|e| e.label == "/help")
        .copied()
        .expect("/help listed");
    match help.run {
        PaletteRun::Slash {
            name,
            has_parameter,
        } => {
            assert_eq!(name, "/help");
            assert!(has_parameter, "/help takes a [topic] parameter");
        }
        PaletteRun::Action(_) => panic!("/help should be a slash run"),
    }
}
