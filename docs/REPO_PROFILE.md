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
settings.

The generated profile does not store source contents, secrets, raw command
output, or a long repo map. Squeezy refreshes it on first run, explicit
`squeezy repo refresh`, or when the cheap repo fingerprint changes. Unchanged
later sessions reuse it silently. When a profile is created or refreshed, CLI
and TUI startup show a compact onboarding summary.

Set `SQUEEZY_REPOS_PATH` to use a different registry path, which is useful for
tests and isolated runs.

Useful commands:

```bash
squeezy repo inspect
squeezy repo refresh
squeezy repo recommendations
```

Use `squeezy repo recommendations` to review settings that may be worth copying
into `squeezy.toml`; Squeezy does not auto-edit project config.
