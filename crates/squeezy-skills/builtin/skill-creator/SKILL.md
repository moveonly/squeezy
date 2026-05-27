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

A Squeezy skill is a directory containing a single `SKILL.md` file that the
agent loads on demand. Skills are local files only; Squeezy does not download
or install skills from any marketplace.

## Layout

```text
<skill-name>/
└── SKILL.md
```

Place the directory under any of:

- `~/.squeezy/skills/<skill-name>` — user-wide native location.
- `~/.agents/skills/<skill-name>` — user-wide compat location for cross-tool reuse.
- `<workspace>/.squeezy/skills/<skill-name>` — project-local native location.
- `<workspace>/.agents/skills/<skill-name>` — project-local compat location.

Project entries override user entries; native entries override compat entries
with the same `name`.

## Frontmatter

```markdown
---
name: example-skill
description: One-sentence summary surfaced in the catalog and `<available_skills>` preamble.
when_to_use: Optional. Concrete situations where the skill is the right tool.
triggers:
  - phrase one
  - phrase two
---

# Body
```

Rules the catalog enforces:

- `name` must start with a lowercase ASCII letter and contain only lowercase
  letters, digits, `-`, or `_`.
- `description` is required and should fit on one line.
- `triggers` are matched word-bounded against the lowered user input.
- Block scalars (`|`, `>`), trailing inline comments, anchors, and nested
  mappings are not supported — the parser is intentionally tiny.

## Body guidelines

- Keep the body focused on procedure, not background reading. Cite paths and
  commands the agent can run directly.
- Avoid huge dumps: an active skill body over the configured `active_body_cap_chars`
  is replaced with a stub and a `load_skill` hint, so the model never sees the
  detail in the prompt.
- Prefer deterministic recipes (exact commands, exact file globs) over advice.

## Activation modes

- Explicit: the user types `/skill <name> ...` in the prompt.
- Trigger: any `triggers` entry matches the user prompt with word boundaries.
- Implicit: the agent runs a shell command that reads the skill's `SKILL.md`
  or runs a script under `<skill>/scripts/`.

Trigger collisions at the same precedence are flagged as ambiguous; users must
disambiguate with `/skill <name>` until one side is renamed or disabled via
`[[skills.config]]`.
