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
fn browse_menu_hides_advanced_commands_and_groups_by_category() {
    let suggestions = slash_suggestions("/");
    // Progressive disclosure: advanced commands are withheld from the bare-`/`
    // browse list (they still surface when a needle is typed — see
    // `advanced_commands_surface_when_filtered`).
    assert!(
        suggestions.iter().all(|cmd| !cmd.is_advanced()),
        "browse list must contain only primary commands"
    );
    assert!(
        suggestions.iter().any(|cmd| cmd.name == "/pin"),
        "a primary command like /pin should be in the browse list"
    );
    assert!(
        !suggestions.iter().any(|cmd| cmd.name == "/revert-turn"),
        "an advanced command like /revert-turn should be withheld from browse"
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
fn advanced_commands_surface_when_filtered() {
    // `/revert-turn` is advanced (hidden from browse) but must stay fuzzy-findable.
    let names = slash_suggestions("/revert")
        .into_iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"/revert-turn"), "{names:?}");
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
fn advanced_partition_only_covers_registered_commands() {
    // Every command is either primary or advanced; this just sanity-checks that
    // both halves are non-empty so a future refactor can't collapse the
    // progressive-disclosure split by accident.
    let advanced = SLASH_COMMANDS.iter().filter(|c| c.is_advanced()).count();
    let primary = SLASH_COMMANDS.len() - advanced;
    assert!(
        advanced > 0 && primary > 0,
        "primary={primary} advanced={advanced}"
    );
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
