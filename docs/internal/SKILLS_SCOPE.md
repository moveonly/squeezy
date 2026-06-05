# Skills Scope

Squeezy skills are local filesystem instruction bundles. They are not plugins,
marketplace packages, remote extensions, or a second tool runtime.

Supported scope:

- Discover `SKILL.md` directories from configured filesystem roots.
- Render a small metadata catalog and load full bodies only when activated.
- Activate by `/skill`, trigger match, `load_skill`, or implicit use of files
  already inside a discovered skill directory.
- Let configuration enable or disable discovered local skills by name or path.
- Surface fork-mode (`context: fork`) skills in a dedicated `<fork_skills>`
  system block so the model dispatches them through a focused subagent
  (currently via `delegate`) instead of executing the body inline.
- Optionally install in-binary bundled sample skills under the user skills
  directory via `squeezy skills install` or `squeezy config init --user
  --with-bundled-skills`. The on-disk catalog remains authoritative; the
  installer never overwrites a user-edited `SKILL.md`.
- Run `hooks:` declared in skill frontmatter when the user explicitly opts in
  via `[skills] hooks_enabled = true`. Default is off because handlers shell
  out via `sh -c` with the same trust boundary as the `shell` tool.

Out of scope:

- Remote marketplace install, upgrade, sync, or curated allowlists.
- Plugin manifests, plugin namespaces, extension APIs, or contributor traits.
- Sandboxed execution of skill `hooks:` commands separate from the rest of the
  agent process. Opt-in is the trust boundary; treat enabling hooks for a
  skill catalog as equivalent to running `sh -c` from that catalog.

If Squeezy needs "plugin" behavior later, model it as extra filesystem skill
roots only. Extra roots must be local paths, not URLs, and must be peers of the
user skill root in discovery. Tool composition belongs inside `squeezy-tools`
Rust interfaces, not in a separate extension runtime.
