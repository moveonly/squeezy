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
- Block scalars (`|` literal, `>` folded) with optional chomping/indent indicators for multi-line string values, e.g. a long `description` wrapped in `>-`.
- Comments on their own line beginning with `#`.

Trailing inline comments on a value line, escape sequences inside quoted strings, anchors/aliases, and nested mappings are not supported. Malformed `SKILL.md` files are skipped with a `tracing` warning and the rest of the catalog still loads.

## Discovery

Squeezy discovers skills from five locations, from lowest to highest precedence:

1. `~/.agents/skills/` (compat user)
2. `~/.squeezy/skills/` (native user)
3. Additional roots from `[skills].extra_roots`
4. `<workspace>/.agents/skills/` (compat project)
5. `<workspace>/.squeezy/skills/` (native project)

Higher tiers override lower tiers with the same skill name. Project tiers override user tiers; native tiers override compat tiers. In monorepos, Squeezy walks ancestor workspaces for project skill roots so a nested package can still inherit the nearest shared `.squeezy/skills/` catalog.

The user skill directories can be changed with `SQUEEZY_SKILLS_USER_DIR` and `SQUEEZY_SKILLS_COMPAT_USER_DIR`. The same fields are available in `~/.squeezy/settings.toml`:

```toml
[skills]
user_dir = "/path/to/squeezy-skills"
compat_user_dir = "/path/to/agent-skills"
extra_roots = ["/mnt/team-skills"]
active_budget_chars = 4000
active_body_cap_chars = 16000
preamble_enabled = true
preamble_budget_chars = 800
active_budget_mode = { context_percent = 2.0 }
preamble_budget_mode = { context_percent = 2.0 }
inline = false
hooks_enabled = false

[[skills.config]]
name = "noisy-project-skill"
enabled = false

[[skills.config]]
path = "/path/to/project/.squeezy/skills/specific-skill"
enabled = true
```

`extra_roots` adds shared catalogs above personal skills and below project skills. `active_body_cap_chars` replaces very large individual skill bodies with a compact stub and a `load_skill` hint. `active_budget_mode` and `preamble_budget_mode` default to `{ context_percent = 2.0 }`, scaling skill metadata budgets with the active model context window. The older `active_budget_chars` and `preamble_budget_chars` remain supported as absolute fallback caps. `inline = false` is the default: active skill bodies are advertised as metadata and fetched with `load_skill` only when needed. Set `inline = true` to restore legacy full-body injection. `hooks_enabled = false` keeps `hooks:` declarations inert unless explicitly enabled.

`[[skills.config]]` entries enable or disable a skill by exact `name` or by skill directory / `SKILL.md` `path`. Use exactly one selector per entry. Entries are applied in order after discovery, so later matches win.

An example skill ships in `tests/artifacts/skills/rust-code-navigation/SKILL.md`.

## Activation

Skills can activate in four ways:

- Explicit user command: `/skill rust-code-navigation inspect this symbol` (a tab between `/skill` and the skill name also works)
- Trigger match: a configured trigger appears in the user task as a word-boundary substring, case-insensitively. For example, the trigger `rust` matches `Rust here` but not `trust this`.
- Model request: the model calls `list_skills`, then `load_skill`
- Implicit shell use: running a script under `<skill>/scripts/` or reading that skill's `SKILL.md` with ordinary shell readers can activate the skill for the next model request in the same turn.

Loaded skill bodies are cached for the lifetime of the process so repeat activations within a session do not re-read the SKILL.md from disk. Settings hot-reload (external `settings.toml` edits) rebuilds the catalog automatically — adding a new `SKILL.md` or toggling `[[skills.config]]` no longer requires a session restart.

Loading a skill only injects instructions. It does not grant tools, bypass approvals, execute shell snippets, or change the session permission policy.

Triggers are intentional Squeezy behavior: they let project skills activate from ordinary task phrasing without requiring users to remember exact skill names. Future mention syntax, if added, should be additive and must not replace `triggers:`.

If two discovered skills with the same name have the same precedence, Squeezy logs a warning and skips trigger activation for that name. The same is true when two distinct skills declare the same trigger phrase — auto-activation is skipped for that ambiguous trigger. Use `/skill <name> ...` or `load_skill` to select the exposed skill explicitly.

## Fork-mode skills

A skill that declares `context: fork` in its frontmatter is surfaced in a separate `<fork_skills>` system block rather than merged into `<active_skills>`. The block includes the full skill body with an instruction telling the model to dispatch it as a focused `delegate` subagent task rather than executing the body as direct guidance for the parent turn. The body is still present in the parent system prompt inside `<fork_skills>`; it is structurally separated and accompanied by an explicit instruction not to act on it inline. The default metadata mode for ordinary active skills is unchanged.

## `tool_deps` enforcement

When a `skill.toml` sidecar declares `tool_deps`, activation now checks each entry against the advertised tool list and the live MCP status snapshot. Missing entries are reported in a `<skill_warnings>` system block that tells the model to refuse the skill rather than improvise. A `mcp:<server>` prefix matches against the MCP status snapshot's ready servers.

## Hooks (opt-in)

`hooks:` blocks declared in skill frontmatter only fire when the user sets `[skills] hooks_enabled = true`. Hook commands run via `sh -c` with the same privileges as the Squeezy process — treat enabling hooks for a skill catalog as equivalent to letting that catalog run the `shell` tool, and keep the gate off for untrusted skills.

## CLI

```sh
# Inspect the discovered catalog (add --json for machine-readable output).
squeezy skills list
squeezy skills validate

# Enable or disable a discovered skill via [[skills.config]]. Selector is
# --name XOR --path; scope is --user or --project.
squeezy skills enable --name rust-code-navigation --project
squeezy skills disable --path /shared/catalog/risky-skill --user

# Install the in-binary bundled sample skills under the user skills directory.
squeezy skills install
squeezy config init --user --with-bundled-skills
```

`squeezy doctor` includes a `skills` row that reports enabled/disabled counts, flags ambiguous same-precedence names as a warning, and notes when `hooks_enabled` is on.

## Built-In Squeezy Help

Squeezy also ships a built-in help surface for questions about Squeezy itself.
This is separate from user and project `SKILL.md` directories.

- Use `/help` in the TUI to list covered Squeezy help topics.
- Use `/help <topic>` for a local answer grounded in bundled
  `crates/squeezy-skills/external-docs/` files and the current run's redacted
  `config inspect` output.
- Natural-language questions that clearly ask about Squeezy itself can be
  answered by the same local help path before model or MCP calls.
- If the local corpus does not cover the topic, Squeezy refuses to guess and
  points to the public website and repository, or to explicit external lookup
  tools when those are available.

Built-in Squeezy help does not fetch the network automatically. Current public
docs lookup remains a separate, explicit web/docs task.
