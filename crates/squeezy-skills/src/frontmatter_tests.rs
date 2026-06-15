use super::*;
use squeezy_hooks::HookEvent;

#[test]
fn non_ascii_trigger_does_not_panic_on_failed_boundary() {
    // Regression: a trigger that begins with a multi-byte UTF-8 character
    // ("ß" = bytes [195, 159]) preceded by a word byte in the input used to
    // advance the scan cursor by exactly one byte, landing inside the
    // multi-byte char and panicking on the next `lowered_input[cursor..]`
    // slice. The first occurrence here fails the word-boundary check
    // (preceded by 'a'); the second succeeds (preceded by a space).
    assert!(input_matches_trigger("aßx ßx", "ßx"));
    // And the all-failing case must simply return false rather than panic.
    assert!(!input_matches_trigger("aßxbßxc", "ßx"));
}

#[test]
fn ascii_trigger_word_boundaries_still_match() {
    assert!(input_matches_trigger("please run the build now", "build"));
    assert!(!input_matches_trigger("rebuilding things", "build"));
}

#[test]
fn parses_skill_frontmatter_and_body() {
    let (metadata, body) = parse_skill_file(
        r#"---
name: rust-nav
description: "Use Rust navigation"
when_to_use: "Rust symbols"
triggers:
  - "rust symbol"
  - cargo metadata
---
# Rust Nav
"#,
    )
    .expect("parse");

    assert_eq!(metadata.name, "rust-nav");
    assert_eq!(metadata.description, "Use Rust navigation");
    assert_eq!(metadata.when_to_use.as_deref(), Some("Rust symbols"));
    assert_eq!(metadata.triggers, vec!["rust symbol", "cargo metadata"]);
    assert_eq!(body.trim(), "# Rust Nav");
}

#[test]
fn parses_folded_block_scalar_description() {
    // `>-` folded block scalar: the canonical form shipped by skills written
    // for other agents. The continuation line must fold into the value rather
    // than be rejected as an "invalid frontmatter line".
    let (metadata, body) = parse_skill_file(
        "---\nname: example-skill\ndescription: >-\n  ALWAYS invoke this skill on the first prompt,\n  whether you are the Agent or a subagent.\n---\n# Body\n",
    )
    .expect("parse");

    assert_eq!(metadata.name, "example-skill");
    assert_eq!(
        metadata.description,
        "ALWAYS invoke this skill on the first prompt, whether you are the Agent or a subagent."
    );
    assert_eq!(body.trim(), "# Body");
}

#[test]
fn parses_literal_block_scalar_preserving_newlines() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: example-skill\ndescription: |\n  line one\n  line two\n---\nbody\n",
    )
    .expect("parse");

    // Literal `|` keeps the line break; default (clip) chomping keeps a single
    // trailing newline.
    assert_eq!(metadata.description, "line one\nline two\n");
}

#[test]
fn literal_block_scalar_strip_chomping_drops_trailing_newline() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: example-skill\ndescription: |-\n  line one\n  line two\n---\nbody\n",
    )
    .expect("parse");

    // `|-` strips every trailing line break.
    assert_eq!(metadata.description, "line one\nline two");
}

#[test]
fn literal_block_scalar_keep_chomping_preserves_trailing_blanks() {
    let (metadata, _body) =
        parse_skill_file("---\nname: example-skill\ndescription: |+\n  line one\n\n---\nbody\n")
            .expect("parse");

    // `|+` keeps the final line break plus the trailing blank line.
    assert_eq!(metadata.description, "line one\n\n");
}

#[test]
fn block_scalar_indicator_in_ordinary_value_is_not_treated_as_block() {
    // A value that merely starts with `>` but is not a bare block indicator
    // (here `>` followed by text) must stay an ordinary single-line scalar.
    let (metadata, _body) =
        parse_skill_file("---\nname: example-skill\ndescription: > 50% coverage\n---\nbody\n")
            .expect("parse");

    assert_eq!(metadata.description, "> 50% coverage");
}

#[test]
fn block_scalar_ends_at_dedented_next_key() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: example-skill\ndescription: >-\n  folded value\nwhen_to_use: after the block\n---\nbody\n",
    )
    .expect("parse");

    assert_eq!(metadata.description, "folded value");
    assert_eq!(metadata.when_to_use.as_deref(), Some("after the block"));
}

#[test]
fn folded_block_scalar_folds_blank_line_to_newline() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: example-skill\ndescription: >-\n  first paragraph\n\n  second paragraph\n---\nbody\n",
    )
    .expect("parse");

    assert_eq!(metadata.description, "first paragraph\nsecond paragraph");
}

#[test]
fn parses_skill_frontmatter_after_bom_and_leading_blanks() {
    let (metadata, body) = parse_skill_file(
        "\u{feff}\n\n---\nname: rust-nav\ndescription: Use Rust navigation\n---\n# Rust Nav\n",
    )
    .expect("parse");

    assert_eq!(metadata.name, "rust-nav");
    assert_eq!(metadata.description, "Use Rust navigation");
    assert_eq!(body.trim(), "# Rust Nav");
}

#[test]
fn parses_context_fork_frontmatter() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: review-spec
description: "Run a multi-step review"
context: fork
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Fork);
}

#[test]
fn missing_context_defaults_to_inline() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: inline-skill
description: "Plain skill"
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
}

#[test]
fn unrecognised_context_value_falls_back_to_inline() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: typo-skill
description: "Author typo in context"
context: bogus
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
}

#[test]
fn explicit_inline_context_parses() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: explicit-inline
description: "Author wrote inline explicitly"
context: "inline"
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
}

#[test]
fn parses_hooks_block_with_matchers_and_specs() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: validator\ndescription: \"validates bash\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/validate.sh\"\n          once: false\n        - type: command\n          command: \"scripts/log.sh\"\n          once: true\n  PostToolUse:\n    - matcher: \"*\"\n      hooks:\n        - type: command\n          command: \"scripts/audit.sh\"\n---\n# body\n",
    )
    .expect("parse");

    let pre = metadata
        .hooks
        .get(&HookEvent::PreToolUse)
        .expect("PreToolUse parsed");
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].matcher.as_deref(), Some("Bash"));
    assert_eq!(pre[0].hooks.len(), 2);
    assert_eq!(pre[0].hooks[0].command, "scripts/validate.sh");
    assert!(!pre[0].hooks[0].once);
    assert_eq!(pre[0].hooks[1].command, "scripts/log.sh");
    assert!(pre[0].hooks[1].once);

    let post = metadata
        .hooks
        .get(&HookEvent::PostToolUse)
        .expect("PostToolUse parsed");
    assert_eq!(post.len(), 1);
    // The literal `*` is normalised to `None` so the handler fires for
    // every payload of the event without per-call filter overhead.
    assert!(post[0].matcher.is_none());
    assert_eq!(post[0].hooks[0].command, "scripts/audit.sh");
}

#[test]
fn parses_hooks_block_with_omitted_matcher_as_match_all() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: validator\ndescription: \"validates all tools\"\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: \"scripts/all-tools.sh\"\n          once: true\n---\n# body\n",
    )
    .expect("parse");

    let hooks = metadata
        .hooks
        .get(&HookEvent::PreToolUse)
        .expect("PreToolUse parsed");
    assert_eq!(hooks.len(), 1);
    assert!(
        hooks[0].matcher.is_none(),
        "omitted matcher should match every payload for the event"
    );
    assert_eq!(hooks[0].hooks.len(), 1);
    assert_eq!(hooks[0].hooks[0].command, "scripts/all-tools.sh");
    assert!(hooks[0].hooks[0].once);
}

#[test]
fn parses_hooks_block_drops_unknown_event_without_failing_load() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  NoSuchEvent:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/x.sh\"\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/y.sh\"\n---\n# body\n",
    )
    .expect("parse");
    assert!(metadata.hooks.contains_key(&HookEvent::PreToolUse));
    assert_eq!(metadata.hooks.len(), 1);
}

#[test]
fn parses_hooks_block_accepts_all_hook_event_names_and_aliases() {
    let cases = [
        ("PreTurn", HookEvent::PreTurn),
        ("pre_turn", HookEvent::PreTurn),
        ("PreToolUse", HookEvent::PreToolUse),
        ("pre_tool_use", HookEvent::PreToolUse),
        ("PostToolUse", HookEvent::PostToolUse),
        ("post_tool_use", HookEvent::PostToolUse),
        ("PostToolUseFailure", HookEvent::PostToolUseFailure),
        ("post_tool_use_failure", HookEvent::PostToolUseFailure),
        ("PostTool", HookEvent::PostTool),
        ("post_tool", HookEvent::PostTool),
        ("PreCompact", HookEvent::PreCompact),
        ("pre_compact", HookEvent::PreCompact),
        ("PostCompact", HookEvent::PostCompact),
        ("post_compact", HookEvent::PostCompact),
        ("SubagentStart", HookEvent::SubagentStart),
        ("subagent_start", HookEvent::SubagentStart),
        ("SubagentStop", HookEvent::SubagentStop),
        ("subagent_stop", HookEvent::SubagentStop),
        ("PermissionRequest", HookEvent::PermissionRequest),
        ("permission_request", HookEvent::PermissionRequest),
        ("PermissionDenied", HookEvent::PermissionDenied),
        ("permission_denied", HookEvent::PermissionDenied),
        ("UserPromptSubmit", HookEvent::UserPromptSubmit),
        ("user_prompt_submit", HookEvent::UserPromptSubmit),
        ("SessionStart", HookEvent::SessionStart),
        ("session_start", HookEvent::SessionStart),
        ("Stop", HookEvent::Stop),
        ("stop", HookEvent::Stop),
        ("Setup", HookEvent::Setup),
        ("setup", HookEvent::Setup),
    ];

    for (key, event) in cases {
        let content = format!(
            "---\nname: validator\ndescription: \"d\"\nhooks:\n  {key}:\n    - matcher: \"*\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# body\n"
        );
        let (metadata, _body) = parse_skill_file(&content).expect("parse");
        assert!(
            metadata.hooks.contains_key(&event),
            "{key} should parse as {event:?}"
        );
    }
}
