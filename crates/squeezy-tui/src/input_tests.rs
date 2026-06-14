use super::*;

#[test]
fn match_slash_command_prefix_returns_command_length() {
    assert_eq!(match_slash_command_prefix("/help"), Some(5));
    assert_eq!(
        match_slash_command_prefix("/help changing the model"),
        Some(5)
    );
    assert_eq!(
        match_slash_command_prefix("/config permissions"),
        Some("/config".len())
    );
}

#[test]
fn match_slash_command_prefix_prefers_longest_match() {
    // `/task-cancel foo` must resolve to `/task-cancel`, not `/task`.
    assert_eq!(
        match_slash_command_prefix("/task-cancel abc"),
        Some("/task-cancel".len())
    );
}

#[test]
fn match_slash_command_prefix_requires_word_boundary() {
    // `/helpme` is not `/help`.
    assert_eq!(match_slash_command_prefix("/helpme"), None);
    // `/options` is a hidden compatibility alias, not a surfaced completion.
    assert_eq!(match_slash_command_prefix("/options"), None);
}

#[test]
fn match_slash_command_prefix_rejects_unknown_or_non_slash() {
    assert_eq!(match_slash_command_prefix("/notacommand"), None);
    assert_eq!(match_slash_command_prefix("help"), None);
    assert_eq!(match_slash_command_prefix(""), None);
}

fn find_command(name: &str) -> &'static SlashCommand {
    SLASH_COMMANDS
        .iter()
        .find(|cmd| cmd.name == name)
        .unwrap_or_else(|| panic!("slash command {name} not registered"))
}

#[test]
fn slash_commands_declare_expected_capabilities() {
    // Anchors the audited capability mapping so future edits to the catalog
    // stay deliberate rather than accidentally silent.
    // `/help` is answered locally for curated topics; no capability badge.
    // Unknown topics can still fall back to the model, but the common path is local.
    assert_eq!(find_command("/help").capabilities, &[]);
    assert_eq!(
        find_command("/compact").capabilities,
        &[PermissionCapability::Network]
    );
    assert_eq!(
        find_command("/feedback").capabilities,
        &[PermissionCapability::Network]
    );
    assert_eq!(
        find_command("/report").capabilities,
        &[PermissionCapability::Network]
    );
    assert_eq!(
        find_command("/attach").capabilities,
        &[PermissionCapability::Read]
    );
    assert_eq!(
        find_command("/diff").capabilities,
        &[PermissionCapability::Git, PermissionCapability::Read]
    );
    assert_eq!(
        find_command("/config").capabilities,
        &[PermissionCapability::Edit]
    );
    assert_eq!(
        find_command("/model").capabilities,
        &[PermissionCapability::Edit]
    );
    assert_eq!(
        find_command("/theme").capabilities,
        &[PermissionCapability::Edit]
    );
    assert_eq!(
        find_command("/undo").capabilities,
        &[
            PermissionCapability::Edit,
            PermissionCapability::Destructive
        ]
    );
    assert_eq!(
        find_command("/revert-turn").capabilities,
        &[
            PermissionCapability::Edit,
            PermissionCapability::Destructive
        ]
    );
}

#[test]
fn config_is_surfaced_without_options_duplicate() {
    assert!(
        SLASH_COMMANDS.iter().any(|cmd| cmd.name == "/config"),
        "expected /config to be the visible settings command"
    );
    assert!(
        !SLASH_COMMANDS.iter().any(|cmd| cmd.name == "/options"),
        "/options is a legacy parser alias and should not duplicate /config in the menu"
    );
}

#[test]
fn purely_informational_slash_commands_declare_no_capabilities() {
    // `/cost`, `/context`, `/tasks`, `/pin`, etc. only read in-memory state.
    // Showing capability badges on them would dilute the signal for commands
    // that actually touch the world.
    for name in [
        "/cost",
        "/context",
        "/tasks",
        "/task",
        "/task-cancel",
        "/pins",
        "/pin",
        "/unpin",
        "/plan",
        "/build",
    ] {
        assert!(
            find_command(name).capabilities.is_empty(),
            "expected {name} to have no capability badges, got {:?}",
            find_command(name).capabilities,
        );
    }
}

#[test]
fn legacy_attachment_commands_are_hidden_from_slash_menu() {
    assert!(!SLASH_COMMANDS.iter().any(|cmd| cmd.name == "/attachments"));
    assert!(!SLASH_COMMANDS.iter().any(|cmd| cmd.name == "/detach"));
}

#[test]
fn copy_is_hidden_from_slash_menu() {
    assert!(!SLASH_COMMANDS.iter().any(|cmd| cmd.name == "/copy"));
}

#[test]
fn terminal_reset_is_registered_and_available_during_a_turn() {
    // The Terminal Restore Command's slash twin must be discoverable in the menu /
    // palette, and available mid-turn — a wedged terminal is most likely while a
    // turn is running, so a recovery verb that vanished during a task would be
    // useless exactly when it is needed.
    let cmd = find_command("/terminal-reset");
    assert!(cmd.available_during_task);
    assert!(cmd.parameter_hint.is_none());
    assert!(cmd.capabilities.is_empty());
}

#[test]
fn checkpoint_commands_hidden_until_checkpointing_enabled() {
    // Checkpointing is off by default, so its commands must not be offered
    // (browse or fuzzy) until the user enables it — a newcomer should never see
    // a command that cannot do anything yet.
    let off = SlashMenuVisibility {
        checkpoints_enabled: false,
        reviewer_enabled: true,
    };
    let on = SlashMenuVisibility {
        checkpoints_enabled: true,
        reviewer_enabled: true,
    };
    for checkpoint_command in ["/checkpoints", "/checkpoint", "/undo", "/revert-turn"] {
        let cmd = find_command(checkpoint_command);
        assert!(
            !cmd.visible(off),
            "{checkpoint_command} should be hidden while checkpointing is disabled"
        );
        assert!(
            cmd.visible(on),
            "{checkpoint_command} should be visible once checkpointing is enabled"
        );
    }
}

#[test]
fn reviewer_command_hidden_until_auto_review_enabled() {
    let off = SlashMenuVisibility {
        checkpoints_enabled: true,
        reviewer_enabled: false,
    };
    let on = SlashMenuVisibility {
        checkpoints_enabled: true,
        reviewer_enabled: true,
    };
    assert!(
        !find_command("/reviewer").visible(off),
        "/reviewer should be hidden until the AI reviewer is enabled"
    );
    assert!(find_command("/reviewer").visible(on));
}

#[test]
fn ungated_commands_stay_visible_regardless_of_flags() {
    let all_off = SlashMenuVisibility {
        checkpoints_enabled: false,
        reviewer_enabled: false,
    };
    for name in ["/help", "/cost", "/config", "/pin", "/clear"] {
        assert!(
            find_command(name).visible(all_off),
            "{name} should never be gated"
        );
    }
}

#[test]
fn capability_badges_match_capability_as_str() {
    let cmd = find_command("/diff");
    assert_eq!(cmd.capability_badges(), vec!["git", "read"]);
    let cmd = find_command("/help");
    // `/help` has no capability badges (curated topics are local).
    assert!(
        cmd.capability_badges().is_empty(),
        "expected /help to have no capability badges, got {:?}",
        cmd.capability_badges()
    );
    let cmd = find_command("/undo");
    assert_eq!(cmd.capability_badges(), vec!["edit", "destructive"]);
}

#[test]
fn capability_badge_labels_are_stable() {
    // Order-independent guarantee that every variant has a short label so a
    // future capability added to squeezy_core surfaces visibly rather than
    // panicking the renderer at run time.
    let variants = [
        PermissionCapability::Read,
        PermissionCapability::Search,
        PermissionCapability::Edit,
        PermissionCapability::Shell,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
        PermissionCapability::Git,
        PermissionCapability::Compiler,
        PermissionCapability::Destructive,
    ];
    for cap in variants {
        let label = capability_badge_label(cap);
        assert!(!label.is_empty(), "{cap:?} produced an empty badge");
    }
}

#[test]
fn slash_suggestions_match_substring_not_just_prefix() {
    // Substring after `/` should match — `/com` resolves `/compact`.
    let names = slash_suggestions("/com")
        .into_iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();
    assert!(
        names.contains(&"/compact"),
        "expected /compact in {names:?}"
    );

    // A non-prefix subsequence still resolves (`atc` → `/attach`).
    let names = slash_suggestions("/atc")
        .into_iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"/attach"), "expected /attach in {names:?}");
}

#[test]
fn slash_suggestions_orders_prefix_matches_before_fuzzy_matches() {
    // `/co` should list prefix matches (`/config`, `/cost`, `/compact`,
    // `/context`) before subsequence-only hits.
    // `/options` is a hidden compatibility alias, so it must not duplicate
    // the visible settings command in the menu.
    let names = slash_suggestions("/co")
        .into_iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();
    let first_four: Vec<&str> = names.iter().take(4).copied().collect();
    let mut expected = vec!["/compact", "/config", "/context", "/cost"];
    expected.sort();
    let mut got = first_four.clone();
    got.sort();
    assert_eq!(got, expected, "first four should be /co* prefix hits");
    assert!(
        !names.contains(&"/options"),
        "/options should not be suggested alongside /config"
    );
}

#[test]
fn slash_suggestions_returns_no_matches_for_unrelated_input() {
    assert!(slash_suggestions("/zzz").is_empty());
}

#[test]
fn browse_lists_every_registered_command_grouped_by_category() {
    // Browse mode (bare `/` at the prompt start) is a complete, categorized index
    // of the command vocabulary: every registered command appears, so a user
    // discovers `/pins`, `/unpin`, `/diff`, etc. by browsing rather than by
    // stumbling onto them. Feature gating is layered on top by
    // `slash_suggestions_visible`; this pre-gate list is the full set.
    let suggestions = slash_suggestions("/");
    let names: Vec<&str> = suggestions.iter().map(|cmd| cmd.name).collect();
    for command in SLASH_COMMANDS {
        assert!(
            names.contains(&command.name),
            "{} must be discoverable in the bare-`/` browse list",
            command.name
        );
    }
    assert_eq!(
        names.len(),
        SLASH_COMMANDS.len(),
        "browse lists each command exactly once"
    );
    // Grouped by category order, so categories never interleave.
    let order: Vec<usize> = suggestions
        .iter()
        .map(|cmd| cmd.category().order_index())
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(
        order, sorted,
        "browse list must be grouped by category order"
    );
}

#[test]
fn browse_sorts_commands_alphabetically_within_each_category() {
    // Within a group the commands are ordered by name so the group is easy to
    // scan; this guards the `.then(left.name.cmp(right.name))` tiebreak in the
    // browse sort, which the category-order check above would not catch on its own.
    let suggestions = slash_suggestions("/");
    let mut per_category: std::collections::BTreeMap<usize, Vec<&str>> =
        std::collections::BTreeMap::new();
    for cmd in &suggestions {
        per_category
            .entry(cmd.category().order_index())
            .or_default()
            .push(cmd.name);
    }
    for (order_index, names) in &per_category {
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            names, &sorted,
            "commands in category #{order_index} must be alphabetical: {names:?}"
        );
    }
}

#[test]
fn default_browse_shows_every_command_except_features_that_are_off() {
    // The contract: every command shows up by default under its category — unless
    // its feature is disabled (checkpointing and the AI reviewer both default
    // off), in which case it stays hidden until enabled rather than dangling as a
    // command that cannot do anything yet.
    let default_vis = SlashMenuVisibility {
        checkpoints_enabled: false,
        reviewer_enabled: false,
    };
    let visible: Vec<&str> = slash_suggestions_visible("/", "/".len(), default_vis)
        .iter()
        .map(|cmd| cmd.name)
        .collect();
    for command in SLASH_COMMANDS {
        assert_eq!(
            visible.contains(&command.name),
            command.visible(default_vis),
            "{}: shown_by_default={} but visible()={}",
            command.name,
            visible.contains(&command.name),
            command.visible(default_vis),
        );
    }
    // Gating is not a no-op: the default list is a *strict* subset of the registry
    // (the checkpoint + reviewer commands are withheld), so a regression that
    // stopped applying the gates — or one that dropped a non-gated command from
    // browse — breaks the equivalence above rather than silently passing.
    assert!(
        visible.len() < SLASH_COMMANDS.len(),
        "default visibility must hide the feature-gated commands, got all {}",
        visible.len()
    );
    // Concretely: the feature-gated commands are absent by default…
    for hidden in [
        "/checkpoints",
        "/checkpoint",
        "/undo",
        "/revert-turn",
        "/reviewer",
    ] {
        assert!(
            !visible.contains(&hidden),
            "{hidden} should be hidden by default"
        );
    }
    // …while every previously-"advanced" command is now discoverable by default.
    for shown in ["/pins", "/unpin", "/diff", "/tasks", "/bundle", "/keymap"] {
        assert!(
            visible.contains(&shown),
            "{shown} should be shown by default"
        );
    }
}

#[test]
fn enabling_a_feature_adds_its_commands_to_browse() {
    // With every gate on, the browse list is exactly the complete registry — the
    // checkpoint and reviewer commands join their categories.
    let all_on = SlashMenuVisibility {
        checkpoints_enabled: true,
        reviewer_enabled: true,
    };
    let visible: Vec<&str> = slash_suggestions_visible("/", "/".len(), all_on)
        .iter()
        .map(|cmd| cmd.name)
        .collect();
    assert_eq!(visible.len(), SLASH_COMMANDS.len());
    for shown in [
        "/checkpoints",
        "/checkpoint",
        "/undo",
        "/revert-turn",
        "/reviewer",
    ] {
        assert!(
            visible.contains(&shown),
            "{shown} should appear once its feature is on"
        );
    }
}

#[test]
fn every_slash_command_has_a_category() {
    // `category()` hits `unreachable!` for an unassigned command; exercising it
    // over the whole registry turns that into a test failure rather than a
    // runtime panic when the menu renders.
    for command in SLASH_COMMANDS {
        let _ = command.category();
    }
}

#[test]
fn every_category_in_order_has_at_least_one_command() {
    // No header should ever render with nothing under it: each category in the
    // browse order must own at least one command. Paired with
    // `every_slash_command_has_a_category`, this keeps the category↔command
    // mapping a total partition into non-empty groups.
    for category in SlashCategory::ORDER {
        let count = SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.category() == category)
            .count();
        assert!(count > 0, "category {} has no commands", category.title());
    }
}

#[test]
fn slash_suggestions_mid_prompt_only_show_inline_commands() {
    let names = slash_suggestions("please /")
        .into_iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();

    for expected in ["/attach", "/build", "/help", "/plan"] {
        assert!(
            names.contains(&expected),
            "expected inline command {expected} in {names:?}"
        );
    }
    for prefix_only in ["/config", "/cost", "/model", "/theme"] {
        assert!(
            !names.contains(&prefix_only),
            "prefix-only command {prefix_only} should not appear mid-prompt: {names:?}"
        );
    }
    // Mid-prompt is not browse mode (no category headers render), so the list
    // must stay alphabetical rather than slipping into category-grouped order —
    // which would look like an arbitrary reshuffle without the headers.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "mid-prompt suggestions stay alphabetical");
}

#[test]
fn inline_dispatch_ignores_prefix_only_commands() {
    assert_eq!(
        find_inline_slash_dispatch_command("please /plan the refactor")
            .map(|occurrence| occurrence.command.name),
        Some("/plan")
    );
    assert!(
        find_inline_slash_dispatch_command("please /cost").is_none(),
        "/cost remains a start-only command"
    );
}

#[test]
fn slash_command_ranges_highlight_inline_commands_only() {
    assert_eq!(
        slash_command_ranges("please /attach Cargo.toml"),
        vec![(7, 14)]
    );
    assert!(slash_command_ranges("please /cost").is_empty());
    assert_eq!(slash_command_ranges("/cost"), vec![(0, 5)]);
}

#[test]
fn inline_slash_scan_continues_past_unusable_commands() {
    let input = "please /cost then /attach Cargo.toml";
    let attach_start = input.find("/attach").expect("fixture");
    assert_eq!(
        find_inline_slash_dispatch_command(input).map(|occurrence| (
            occurrence.start,
            occurrence.end,
            occurrence.command.name
        )),
        Some((attach_start, attach_start + "/attach".len(), "/attach"))
    );
    assert_eq!(
        slash_command_ranges(input),
        vec![(attach_start, attach_start + "/attach".len())]
    );
}
