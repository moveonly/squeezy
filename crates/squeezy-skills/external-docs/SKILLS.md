# Skills

Squeezy skills are local `SKILL.md` directories that add specialized instructions only when activated. Squeezy may advertise a small metadata-only catalog at session start, but inactive skill bodies are not added to provider requests.

## Layout

```text
skill-name/
└── SKILL.md
```

`SKILL.md` uses YAML-style frontmatter followed by Markdown instructions:

```markdown
---
name: rust-code-navigation
description: Use for Rust declarations, references, hierarchy, and impact tasks.
when_to_use: Rust source navigation and semantic graph inspection.
triggers:
  - Rust declaration
  - dependency path
---

# Rust Code Navigation
...
```

Required fields are `name` and `description`. Optional fields are `when_to_use` and `triggers`. Skill names must start with a lowercase ASCII letter and contain only lowercase letters, digits, `-`, or `_`.

### Supported frontmatter syntax

Squeezy ships a small purpose-built frontmatter reader, not a full YAML parser. The following subset is supported:

- `key: value` with optional surrounding double or single quotes (no escape sequences).
- Inline lists, e.g. `triggers: ["Rust declaration", "dependency path"]` (commas inside quoted items are not supported).
- Block lists with `-` items indented under a key:
  ```yaml
  triggers:
    - Rust declaration
    - dependency path
  ```
- Comments on their own line beginning with `#`.

Block scalars (`|`, `>`), trailing inline comments on a value line, escape sequences inside quoted strings, anchors/aliases, and nested mappings are not supported. Malformed `SKILL.md` files are skipped with a `tracing` warning and the rest of the catalog still loads.

## Discovery

Squeezy discovers skills from four locations, from lowest to highest precedence:

1. `~/.agents/skills/` (compat user)
2. `~/.squeezy/skills/` (native user)
3. `<workspace>/.agents/skills/` (compat project)
4. `<workspace>/.squeezy/skills/` (native project)

Higher tiers override lower tiers with the same skill name. Project tiers override user tiers; native tiers override compat tiers.

The user skill directories can be changed with `SQUEEZY_SKILLS_USER_DIR` and `SQUEEZY_SKILLS_COMPAT_USER_DIR`. The same fields are available in `~/.squeezy/settings.toml`:

```toml
[skills]
user_dir = "/path/to/squeezy-skills"
compat_user_dir = "/path/to/agent-skills"
active_budget_chars = 4000
active_body_cap_chars = 16000
preamble_enabled = true
preamble_budget_chars = 800

[[skills.config]]
name = "noisy-project-skill"
enabled = false

[[skills.config]]
path = "/path/to/project/.squeezy/skills/specific-skill"
enabled = true
```

`active_budget_chars` caps the rendered `<active_skills>` bundle for a turn. `active_body_cap_chars` replaces very large individual skill bodies with a compact stub and a `load_skill` hint. `preamble_enabled` controls the session-start metadata catalog; `preamble_budget_chars` caps that catalog.

`[[skills.config]]` entries enable or disable a skill by exact `name` or by skill directory / `SKILL.md` `path`. Use exactly one selector per entry. Entries are applied in order after discovery, so later matches win.

An example skill ships in `tests/artifacts/skills/rust-code-navigation/SKILL.md`.

## Activation

Skills can activate in four ways:

- Explicit user command: `/skill rust-code-navigation inspect this symbol` (a tab between `/skill` and the skill name also works)
- Trigger match: a configured trigger appears in the user task as a word-boundary substring, case-insensitively. For example, the trigger `rust` matches `Rust here` but not `trust this`.
- Model request: the model calls `list_skills`, then `load_skill`
- Implicit shell use: running a script under `<skill>/scripts/` or reading that skill's `SKILL.md` with ordinary shell readers can activate the skill for the next model request in the same turn.

Loaded skill bodies are cached for the lifetime of the process so repeat activations within a session do not re-read the SKILL.md from disk.

Loading a skill only injects instructions. It does not grant tools, bypass approvals, execute shell snippets, or change the session permission policy.

Triggers are intentional Squeezy behavior: they let project skills activate from ordinary task phrasing without requiring users to remember exact skill names. Future mention syntax, if added, should be additive and must not replace `triggers:`.

If two discovered skills with the same name have the same precedence, Squeezy logs a warning and skips trigger activation for that name. Use `/skill <name> ...` or `load_skill` to select the exposed skill explicitly.

## Built-In Squeezy Help

Squeezy also ships a built-in help surface for questions about Squeezy itself.
This is separate from user and project `SKILL.md` directories.

- Use `/help` in the TUI to list covered Squeezy help topics.
- Use `/help <topic>` for a local answer grounded in bundled
  `docs/external/` files and the current run's redacted `config inspect`
  output.
- Natural-language questions that clearly ask about Squeezy itself can be
  answered by the same local help path before model or MCP calls.
- If the local corpus does not cover the topic, Squeezy refuses to guess and
  points to the public website and repository, or to explicit external lookup
  tools when those are available.

Built-in Squeezy help does not fetch the network automatically. Current public
docs lookup remains a separate, explicit web/docs task.
