---
name: skill-creator
description: Author a new Squeezy `SKILL.md` instruction bundle that the catalog can discover and activate.
when_to_use: When the user wants to create, edit, or review a Squeezy skill — including frontmatter, triggers, and discovery layout.
triggers:
  - new skill
  - create a skill
  - author a skill
  - write a skill
  - skill.md
---

# Skill Creator

A Squeezy skill is a local directory with a required `SKILL.md` file that the
catalog discovers and the agent loads on demand. A skill can also carry
optional local files such as `skill.toml` metadata or helper scripts. Squeezy
does not download or install skills from any marketplace.

## Layout

```text
<skill-name>/
├── SKILL.md
├── skill.toml      # optional metadata
└── scripts/        # optional helpers and hooks
```

Place the directory under any of:

- `~/.squeezy/skills/<skill-name>` — user-wide native location.
- `~/.agents/skills/<skill-name>` — user-wide compat location for cross-tool reuse.
- Any configured `[skills].extra_roots` directory — shared native catalogs.
- `<workspace>/.squeezy/skills/<skill-name>` — project-local native location.
- `<workspace>/.agents/skills/<skill-name>` — project-local compat location.

Project entries override user entries; native entries override compat entries
with the same `name`. Extra roots sit above personal skills and below project
skills. When launched from a nested package, Squeezy also scans ancestor
workspace skill roots up to the git root so monorepo packages can inherit a
shared catalog.

## Frontmatter

```markdown
---
name: example-skill
description: One-sentence summary surfaced in the catalog and `<available_skills>` preamble.
when_to_use: Optional. Concrete situations where the skill is the right tool.
triggers:
  - phrase one
  - phrase two
# context: inline  # optional: inline | fork
---

# Body
```

Rules the catalog enforces:

- `name` must start with a lowercase ASCII letter and contain only lowercase
  letters, digits, `-`, or `_`.
- `description` is required and should fit on one line.
- `when_to_use`, `triggers`, and `context` are optional.
- `triggers` are matched word-bounded against the lowered user input.
- `context: fork` marks the skill for the separate fork-skill render path;
  missing, `inline`, empty, or unrecognized values behave as inline.
- Block scalars (`|`, `>`), trailing inline comments, anchors, and nested
  mappings are not supported — the parser is intentionally tiny.

## Optional sidecar

A `skill.toml` file next to `SKILL.md` adds catalog metadata without changing
the frontmatter identity:

```toml
tool_deps = ["load_skill", "mcp:docs"]
prompt_hint = "Load this skill before editing shared docs."
icon = "icon.png"
```

`tool_deps` are surfaced to the model and checked against the current tool and
MCP server inventory. Missing deps produce a warning block; they do not grant
tools or permissions. `prompt_hint` is rendered as a short activation hint.
`icon` is display metadata and is not rendered into the prompt.

## Body guidelines

- Keep the body focused on procedure, not background reading. Cite paths and
  commands the agent can run directly.
- Avoid huge dumps. By default `[skills].inline = false`, so active skills are
  advertised as metadata and the model must call `load_skill` for the body.
  In legacy inline mode, a body over `active_body_cap_chars` is replaced with
  a stub and a `load_skill` hint.
- Prefer deterministic recipes (exact commands, exact file globs) over advice.

## Activation modes

- Explicit: the user types `/skill <name> ...` in the prompt.
- Trigger: any `triggers` entry matches the user prompt with word boundaries.
- Model request: the model calls `list_skills`, then `load_skill`.
- Implicit: the agent runs a shell command that reads the skill's `SKILL.md`
  or runs a script under `<skill>/scripts/`.

Same-precedence name collisions mark that skill name ambiguous. Duplicate
trigger phrases across distinct skills also disable auto-activation for that
trigger. Users must disambiguate with `/skill <name>` or `load_skill` until one
side is renamed or disabled via `[[skills.config]]`.

## Hooks

`hooks:` frontmatter is parsed but inert unless `[skills].hooks_enabled = true`.
Hook commands run through `sh -c` with the Squeezy process privileges and read
the event payload from `SQUEEZY_HOOK_PAYLOAD`, so only enable hooks for trusted
skill catalogs.

```yaml
hooks:
  PreToolUse:
    - matcher: "edit"
      hooks:
        - type: command
          command: "scripts/check-edit.sh"
          once: false
```

Accepted hook events are `PreTurn`, `PreToolUse`, `PostToolUse`, `PostTool`,
`PreCompact`, `PostCompact`, `SubagentStart`, and `PermissionRequest`; snake
case aliases such as `pre_tool_use` are also accepted. Only `type: command` is
implemented.

## Validation and installation

Use `squeezy skills validate` to surface parse errors that normal discovery
would skip with a tracing warning. Use `squeezy skills list` to inspect the
effective catalog, including disabled and ambiguous skills.

Squeezy ships bundled sample skills in the binary. Install them under the user
skills directory with `squeezy skills install`, or initialize user settings with
`squeezy config init --user --with-bundled-skills`.
