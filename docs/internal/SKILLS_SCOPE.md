# Skills Scope

Squeezy skills are local filesystem instruction bundles. They are not plugins,
marketplace packages, remote extensions, or a second tool runtime.

## Implemented Scope

- Discover `SKILL.md` directories from local filesystem roots. Current sources
  include project skills, user skills, compatibility project/user roots, and
  configured `[skills].extra_roots`. Project skills have precedence over
  shared extra roots and user-level catalogs.
- Parse the supported frontmatter subset: `name`, `description`,
  `when_to_use`, `triggers`, `context`, and `hooks`. Unsupported YAML features
  are not a public contract.
- Attach optional `skill.toml` metadata such as `tool_deps`, `icon`, and
  `prompt_hint`. The agent warns when an activated skill declares unavailable
  tool or MCP dependencies.
- Render a small available-skills preamble and keep full bodies out of the
  prompt until activation. The default active-skill rendering is metadata-only
  (`[skills].inline = false`); the model can call `load_skill` when it needs
  the body. Setting `inline = true` restores full-body injection.
- Scale active and preamble budgets from the model context window by default
  (`context_percent = 2.0`), with legacy absolute char caps still available.
- Activate by `/skill`, unique trigger match, `load_skill`, or implicit use of
  files already inside a discovered skill directory. Ambiguous skill names and
  ambiguous triggers do not auto-load; users must select explicitly.
- Let `[[skills.config]]` enable or disable discovered local skills by exact
  name or by skill directory / `SKILL.md` path. Catalogs are rebuilt on settings
  reload, so toggles and new files do not require a process restart.
- Surface fork-mode (`context: fork`) skills in a dedicated `<fork_skills>`
  system block so the model dispatches them through a focused subagent
  (currently via `delegate`) instead of treating the body as direct parent-turn
  instructions.
- Optionally install in-binary bundled sample skills under the user skills
  directory via `squeezy skills install` or `squeezy config init --user
  --with-bundled-skills`. The on-disk catalog remains authoritative; the
  installer never overwrites a user-edited `SKILL.md`.
- Run `hooks:` declared in skill frontmatter when the user explicitly opts in
  via `[skills] hooks_enabled = true`. Default is off because handlers shell
  out via `sh -c` with the same trust boundary as the `shell` tool.

## Out Of Scope

- Remote marketplace install, upgrade, sync, or curated allowlists.
- Plugin manifests, plugin namespaces, extension APIs, or contributor traits.
- Sandboxed execution of skill `hooks:` commands separate from the rest of the
  agent process. Opt-in is the trust boundary; treat enabling hooks for a
  skill catalog as equivalent to running `sh -c` from that catalog.
- Non-local skill roots such as URLs, registries, or downloaded bundles.

If Squeezy needs "plugin" behavior later, model it as extra filesystem skill
roots only. Extra roots must be local paths, not URLs, and must be peers of the
user skill root in discovery. Tool composition belongs inside `squeezy-tools`
Rust interfaces, not in a separate extension runtime.
