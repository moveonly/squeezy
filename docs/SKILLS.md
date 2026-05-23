# Skills

Squeezy skills are local `SKILL.md` directories that add specialized instructions only when activated. Inactive skills are discovered as metadata and do not add their descriptions or bodies to provider requests.

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
```

An example skill ships in `tests/artifacts/skills/rust-code-navigation/SKILL.md`.

## Activation

Skills can activate in three ways:

- Explicit user command: `/skill rust-code-navigation inspect this symbol` (a tab between `/skill` and the skill name also works)
- Trigger match: a configured trigger appears in the user task as a word-boundary substring, case-insensitively. For example, the trigger `rust` matches `Rust here` but not `trust this`.
- Model request: the model calls `list_skills`, then `load_skill`

Loaded skill bodies are cached for the lifetime of the process so repeat activations within a session do not re-read the SKILL.md from disk.

Loading a skill only injects instructions. It does not grant tools, bypass approvals, execute shell snippets, or change the session permission policy.
