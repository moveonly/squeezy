# Skills Scope

Squeezy skills are local filesystem instruction bundles. They are not plugins,
marketplace packages, remote extensions, or a second tool runtime.

Supported scope:

- Discover `SKILL.md` directories from configured filesystem roots.
- Render a small metadata catalog and load full bodies only when activated.
- Activate by `/skill`, trigger match, `load_skill`, or implicit use of files
  already inside a discovered skill directory.
- Let configuration enable or disable discovered local skills by name or path.

Out of scope:

- Remote marketplace install, upgrade, sync, or curated allowlists.
- Plugin manifests, plugin namespaces, extension APIs, or contributor traits.
- Bundled system-skill installers that write embedded skills to disk.
- Skill hooks that shell out on lifecycle events.

If Squeezy needs "plugin" behavior later, model it as extra filesystem skill
roots only. Extra roots must be local paths, not URLs, and must be peers of the
user skill root in discovery. Tool composition belongs inside `squeezy-tools`
Rust interfaces, not in a separate extension runtime.
