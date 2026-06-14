# Prompt Templates

Prompt templates are reusable slash macros stored as plain Markdown files.
Each template expands into a prompt when its slash name is submitted in the TUI.
For example, `~/.squeezy/prompts/review.md` is activated with `/review`.

## Discovery Locations

Squeezy looks for templates in these places:

| Scope | Location | Precedence |
|-------|----------|------------|
| User | `~/.squeezy/prompts/*.md` | Lower |
| User | `$XDG_CONFIG_HOME/squeezy/prompts/*.md` (or `~/.config/squeezy/prompts/*.md`) | Lower |
| Project | `<workspace>/.squeezy/prompts/*.md` | Higher |

When the XDG location resolves to the same path as `~/.squeezy/prompts/`, it is
not scanned twice.

Project templates shadow user templates with the same name, so a team can
ship workspace-specific templates that override personal defaults.

## File Format

Each template is a plain Markdown `.md` file. The filename (without the `.md`
extension) becomes the slash name. Names must start with an ASCII alphanumeric
character and may contain ASCII letters, digits, `-`, or `_`.

Templates may start with optional YAML-style frontmatter:

```markdown
---
description: Review the current diff
argument-hint: "[path]"
args: [target]
---
Review {target}. Focus on correctness, edge cases, and compatibility.
```

Supported frontmatter keys are `description`, `argument-hint` (or
`argument_hint`), and `args`. If `description` is omitted, Squeezy uses the
first non-empty body line, truncated for the slash menu.

Example — `~/.squeezy/prompts/review.md`:

```markdown
Review the changes in the current diff. Focus on:
- Correctness and edge cases
- Performance implications
- Compatibility with existing interfaces

Be concise. Use bullet points.
```

Activate it with:

```
/review
```

Squeezy inserts the full template text as the user's prompt for that turn.

## Argument Substitution

Template arguments are split with shell-style quoting. The body supports:

- `{1}`, `{2}`, etc. for positional arguments.
- `{name}` for names declared in the `args` frontmatter list.
- `{ARGUMENTS}` for all arguments joined with spaces.
- `$1`, `$2`, `$@`, `$ARGUMENTS`, `${@:N}`, and `${@:N:L}` for compatibility
  with shell-style templates.

Unresolved tokens are left unchanged so literal braces can pass through.

Example template `test-coverage.md`:

```markdown
Add tests for {1} with coverage for error cases and edge conditions.
```

Activated as `/test-coverage MyModule` expands to:

```
Add tests for MyModule with coverage for error cases and edge conditions.
```

## Scope Precedence

When a user template and a project template share the same name, the project
template wins. This lets a workspace override a personal default for
project-specific workflows without changing the user's global files.

## Use Cases

- **Reusable review prompts**: standardize code-review requests across a team.
- **Task templates**: common patterns like "add tests for X", "document Y",
  "refactor Z to use W".
- **Policy reminders**: embed compliance or style reminders into frequently-used
  prompts so they are applied consistently.
- **Onboarding workflows**: ship project-specific templates that guide new
  contributors through standard tasks.

## Related

- [SKILLS.md](SKILLS.md) — for more powerful per-skill instruction injection.
- Use `/skill <name>` to activate a full skill with structured instructions;
  use a template slash name such as `/review` for lightweight prompt expansion.
