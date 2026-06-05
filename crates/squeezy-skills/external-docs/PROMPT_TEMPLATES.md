# Prompt Templates

Prompt templates are reusable prompt patterns stored as plain Markdown files.
Each template expands into a prompt when activated via `/prompt-template <name>`
in the TUI composer.

## Discovery Locations

Squeezy looks for templates in two places:

| Scope | Location | Precedence |
|-------|----------|------------|
| User | `~/.squeezy/prompts/*.md` | Lower |
| Project | `<workspace>/.squeezy/prompts/*.md` | Higher |

Project templates shadow user templates with the same name, so a team can
ship workspace-specific templates that override personal defaults.

## File Format

Each template is a plain Markdown `.md` file. The filename (without the `.md`
extension) becomes the template name.

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
/prompt-template review
```

Squeezy inserts the full template text as the user's prompt for that turn.

## Argument Substitution

Templates may use `{{arg}}` placeholders. When activated with arguments, each
positional word after the template name is substituted for `{{1}}`, `{{2}}`,
etc. Named placeholders (`{{target}}`) require a `key=value` syntax.

Example template `test-coverage.md`:

```markdown
Add tests for {{1}} with coverage for error cases and edge conditions.
```

Activated as `/prompt-template test-coverage MyModule` expands to:

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
  use `/prompt-template <name>` for lightweight prompt expansion.
