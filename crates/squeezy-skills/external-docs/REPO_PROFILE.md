# Repo Profile

Squeezy keeps user-authored project policy separate from generated repo knowledge.

Project configuration belongs in `squeezy.toml`. It is the right place for
manual choices that may be committed, such as graph languages, include/exclude
rules, permission rules, cache paths, and preferred verification policy.

Generated repo profiles live in `~/.squeezy/repos.toml` by default. This file is
machine-local and keyed by canonical repo path. It records compact facts Squeezy
detected so later sessions can avoid repeated project-shape exploration:
languages, package/build systems, likely commands, config files, Git state,
ignored/indexing coverage, semantic support, and suggested `squeezy.toml`
settings. Each profile also stores a stable `repo_id` derived from the
canonical repo path.

Per-repo user-authored overrides live at
`~/.squeezy/projects/<repo-id>/settings.toml`. Use this file for personal
machine paths that should not be committed, such as extra shell sandbox
`read_roots` or `write_roots`. Shared team policy still belongs in the
project's committed `squeezy.toml`.

The generated profile does not store source contents, secrets, raw command
output, or a long repo map. Squeezy refreshes it on first run, explicit
`squeezy repo refresh`, or when the cheap repo fingerprint changes. Unchanged
later sessions reuse it silently. When a profile is created or refreshed, CLI
and TUI startup show a compact onboarding summary, and startup also appends the
current profile summary into model instructions so the agent can use that
machine-local project context without re-exploring it every session.

Set `SQUEEZY_REPOS_PATH` to use a different registry path, which is useful for
tests and isolated runs. Set `SQUEEZY_PROJECTS_DIR` to use a different
per-repo settings directory.

Useful commands:

```bash
squeezy repo inspect
squeezy repo refresh
squeezy repo recommendations
```

Use `squeezy repo recommendations` to review settings that may be worth copying
into `squeezy.toml`; Squeezy does not auto-edit project config.
