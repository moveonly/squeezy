---
name: beads-workflow
description: Beads (`bd`) task-tracker recipes for claiming, updating, and closing issues from inside Squeezy.
when_to_use: When the user mentions Beads, `bd`, claiming or closing an issue, or asks for the durable task tracker conventions used in this repo.
triggers:
  - bd ready
  - bd show
  - bd update
  - bd close
  - bd remember
  - beads issue
---

# Beads Workflow

Squeezy treats Beads (`bd`) as the durable task tracker. Use it for every issue
update; do not invent ad hoc markdown TODO lists or memory files.

## Common commands

```bash
bd ready             # list issues ready to pick up
bd show <id>         # print full issue details
bd update <id> --claim
bd close <id>        # mark completed
bd remember "<note>" # persist project memory
bd prime             # refresh local Beads context after a long gap
```

## Recipe: pick up the next ticket

1. Run `bd ready` to find an open issue at the right priority.
2. Run `bd show <id>` and read description, acceptance, and metadata.
3. Run `bd update <id> --claim` before starting work.
4. When done, run `bd close <id>` with a commit message referencing the id.

## Notes

- Run `bd prime` if the local Beads context looks stale or commands fail with
  "missing index".
- Keep durable project memory in Beads via `bd remember`; do not add side-files
  for the same purpose.
- Reference the issue id in commits using the form `Closes <id>` so downstream
  tooling can link the commit to the ticket.
