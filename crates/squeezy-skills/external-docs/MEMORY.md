# Memory

Squeezy keeps a small, durable memory across sessions so it doesn't have to
re-learn who you are and what a project is about every time you start. Memory is
**on by default**. It's plain Markdown on disk that both you and the agent can
read and edit.

## What it remembers

Each memory is one fact in its own file, tagged with a type:

- **user** — your role, expertise, and durable working preferences.
- **feedback** — how you like the agent to work with you (corrections and
  confirmations).
- **project** — ongoing work, decisions, or context for a specific repository
  that isn't derivable from the code or git history.
- **reference** — where information lives in an external system (issue tracker,
  dashboard, channel).

It deliberately does **not** store things it can re-derive — code patterns, file
paths, architecture, git history, fix recipes, anything already in `AGENTS.md` —
and never stores secrets, credentials, or personal data.

## How it works

- **Recall is automatic.** At the start of every session the memory index is
  loaded into the model's context, so the agent already knows your saved facts —
  you don't have to ask it to "remember."
- **Capture is automatic.** After a substantive turn, a cheap background pass
  distills durable facts from the conversation into memory. It's gated so it
  stays cheap (it only runs when there's new substantive input and doesn't
  re-run if the agent already saved something that turn), and it never runs in
  the eval/benchmark harnesses.
- **You can also be explicit.** Just say *"remember that …"* or *"forget …"* and
  the agent saves or removes a memory immediately.

When the agent saves something automatically you'll see a quiet
`✎ memory: …` line in the transcript, so it's never silent.

## Scope: global vs. project

The **type decides where a memory is stored** — the agent never picks a location:

| Type | Scope | Lives in |
| --- | --- | --- |
| `user`, `feedback` | global (all your projects) | `~/.squeezy/memory/` |
| `project`, `reference` | this repository only | `<repo>/.squeezy/memory/` |

So facts about *you* follow you everywhere, while facts about *a repo* stay with
that repo. The per-project `<repo>/.squeezy/` directory is automatically
git-ignored so memory never shows up in your `git status`.

## Where it's stored

Each scope has the same layout under its base directory:

```
~/.squeezy/MEMORY.md          # global index (loaded into the prompt)
~/.squeezy/memory/<slug>.md   # one global memory per file

<repo>/.squeezy/MEMORY.md         # this project's index
<repo>/.squeezy/memory/<slug>.md  # one project memory per file
```

`MEMORY.md` is a one-line-per-memory table of contents. Each topic file is one
paragraph with YAML frontmatter (`name`, `description`, `metadata.type`).

## Checking, adding, and removing memory

- **Check** what's remembered: run `/memory` in the TUI (lists everything,
  grouped by scope), or just read the files above (`cat ~/.squeezy/MEMORY.md`).
- **Add**: let it happen automatically, or say *"remember that …"*.
- **Update / correct**: tell the agent the new fact — it overwrites the stale
  memory rather than keeping both. If two memories ever disagree, the newer one
  supersedes the older.
- **Remove**: say *"forget …"*, or delete the topic file and its line in
  `MEMORY.md` by hand.

Because memory is just Markdown, hand-editing is fully supported — open the files
in any editor.

## Contradictions

When something you say contradicts a saved memory (you changed your mind, a
decision was reversed), the agent **replaces** the stale memory — it overwrites
or deletes it rather than asking first, so memory stays coherent without
interrupting your work. The `✎ memory:` line and `/memory` let you see and
correct any change.

## Turning it off

Memory is governed by `context.user_memory_max_bytes`:

- `> 0` (default `16384`) — memory is on.
- `0` — disables the whole surface (no prompt injection, no automatic capture).

To keep file memory but stop the automatic extraction pass (e.g. to avoid its
extra model call), set `context.memory_auto_extract = false`. Both are editable
from `/config`, or via the `SQUEEZY_CONTEXT_USER_MEMORY_MAX_BYTES` /
`SQUEEZY_CONTEXT_MEMORY_AUTO_EXTRACT` environment variables.

Memory is separate from the structured `notes_remember` / `notes_recall` store,
which is a queryable per-project notebook; see `/help tools`.
