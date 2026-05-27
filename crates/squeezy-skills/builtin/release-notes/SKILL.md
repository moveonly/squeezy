---
name: release-notes
description: Draft terse, diff-grounded release notes from the recent git history of the current workspace.
when_to_use: When the user asks for release notes, a changelog entry, or a summary of recent commits since the last tag.
triggers:
  - release notes
  - changelog
  - what changed since
  - draft release
---

# Release Notes

Produce a short, neutral changelog entry from the local git history. Notes
should be diff-grounded: every bullet maps back to a real commit, not a guess.

## Inputs

- `since` reference (default: the most recent tag from `git describe --tags --abbrev=0`).
- `until` reference (default: `HEAD`).

## Recipe

1. Read commits with
   `git log --no-merges --pretty=format:'%h %s' <since>..<until>` and parse
   subject lines.
2. Group commits by Conventional Commit prefix when present
   (`feat`, `fix`, `perf`, `refactor`, `docs`, `chore`, `tui`, `agent`, ...);
   otherwise group by directory of the largest touched file via
   `git show --stat`.
3. Emit at most one bullet per commit using the form
   `- <area>: <imperative summary> (<short-sha>)`.
4. Surface the resolved `since`/`until` references and total commit count at
   the top of the answer so the user can verify the window.

## Style rules

- Imperative voice, no marketing language, no emojis.
- Prefer the commit subject verbatim when it already reads as a release note.
- Do not invent fixes or features that are not in the diff. If a commit has no
  user-visible effect, mark it `(internal)` instead of dropping it silently.
- Keep bullets under one line each; let the underlying commit speak for the
  details.
