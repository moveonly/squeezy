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
    // `/help` is local-first for known topics, but the model fallback for
    // unknown topics is still network-capable.
    assert_eq!(
        find_command("/help").capabilities,
        &[PermissionCapability::Network]
    );
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
fn checkpoint_commands_hide_when_checkpointing_is_disabled() {
    let names = slash_suggestions("/")
        .into_iter()
        .filter(|cmd| cmd.visible_with_checkpoints(false))
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();

    for checkpoint_command in ["/checkpoints", "/checkpoint", "/undo", "/revert-turn"] {
        assert!(
            !names.contains(&checkpoint_command),
            "{checkpoint_command} should be hidden when checkpointing is disabled"
        );
    }
}

#[test]
fn checkpoint_commands_show_when_checkpointing_is_enabled() {
    let names = slash_suggestions("/")
        .into_iter()
        .filter(|cmd| cmd.visible_with_checkpoints(true))
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>();

    for checkpoint_command in ["/checkpoints", "/checkpoint", "/undo", "/revert-turn"] {
        assert!(
            names.contains(&checkpoint_command),
            "{checkpoint_command} should be visible when checkpointing is enabled"
        );
    }
}

#[test]
fn capability_badges_match_capability_as_str() {
    let cmd = find_command("/diff");
    assert_eq!(cmd.capability_badges(), vec!["git", "read"]);
    // `/help` is local-first: no network badge.
    let cmd = find_command("/help");
    assert_eq!(cmd.capability_badges(), Vec::<&str>::new());
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
fn slash_suggestion_count_matches_full_suggestions() {
    for input in ["/", "/co", "please /", "please /att", "/zzz"] {
        assert_eq!(
            slash_suggestion_count_at(input, input.len(), true),
            slash_suggestions(input).len(),
            "{input:?}"
        );
    }

    let checkpoint_disabled_count = slash_suggestion_count_at("/", 1, false);
    let filtered_count = slash_suggestions("/")
        .into_iter()
        .filter(|cmd| cmd.visible_with_checkpoints(false))
        .count();
    assert_eq!(checkpoint_disabled_count, filtered_count);
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
