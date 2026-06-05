---
name: customize-squeezy
description: Edit Squeezy's TOML configuration (`~/.squeezy/settings.toml`, `squeezy.toml`, per-repo overrides) using the canonical schema.
when_to_use: When the user asks how to change Squeezy's behavior, set a provider/model, add an MCP server, write a permission rule, point at a skill directory, or edit any `settings.toml`/`squeezy.toml`.
triggers:
  - settings.toml
  - squeezy.toml
  - configure squeezy
  - customize squeezy
  - squeezy config
  - mcp.servers
  - permissions.rules
---

# Customize Squeezy

Squeezy reads one merged TOML configuration from a fixed precedence chain.
Edit the right file for the scope of the change, then verify with
`squeezy config inspect`.

## File locations and precedence

Later sources override earlier ones:

1. Built-in defaults.
2. User settings: `~/.squeezy/settings.toml`.
3. Project settings: `squeezy.toml` at the workspace root (committed; shared).
4. Per-repo user overrides:
   `~/.squeezy/projects/<repo-id>/settings.toml` (machine-local).
5. Environment variables (e.g. `SQUEEZY_PROVIDER`, `SQUEEZY_MODEL`).
6. CLI flags.

`.squeezy/` inside a repo is local runtime state and is git-ignored; do not
put shared project config there. `SQUEEZY_SETTINGS_PATH` redirects the user
settings file.

Authoritative reference: `crates/squeezy-skills/external-docs/CONFIGURATION.md`.
The bundled help index exposes the same content via `/help configuration`.

## Common edits

### Set provider, model, profile

```toml
[model]
provider = "openai"
model = "gpt-5.5"
profile = "balanced"
reasoning_effort = "medium"  # low | medium | high | xhigh (provider-gated)
# max_output_tokens = 64000  # optional; unset lets the provider cap apply
```

`reasoning_effort` is only forwarded to providers/models whose registry entry
declares native reasoning controls.

### Configure a provider

```toml
[providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
default_model = "gpt-5.5"
```

Squeezy never writes secret values into TOML; it stores the *name* of the
environment variable that holds the key. On macOS, missing env vars fall
back to the Keychain account matching the provider id (`openai`,
`anthropic`, `google`, `azure_openai`).

### Add an MCP server (stdio)

```toml
[mcp.servers.docs]
enabled = true
transport = "stdio"
command = "docs-mcp"
args = []

[mcp.servers.docs.permissions]
default = "ask"  # allow | ask | deny
```

Remote MCP servers use `transport = "http"` or `"sse"` with a `url` field
instead of `command`/`args`. Disable a server with `enabled = false` to
keep its config without advertising its tools.

Use the CLI shortcuts instead of hand-editing when possible:

```sh
squeezy mcp list
squeezy mcp add docs --project --transport stdio --command docs-mcp
squeezy mcp disable docs --project
```

### Permission rules

Use `[permissions].mode` for the broad policy shape, `[permissions.custom]`
for granular custom-mode defaults, and per-target rules in
`[[permissions.rules]]` for exceptions. Later matching rules win.

```toml
[permissions]
mode = "custom" # default | auto_review | full_access | custom

[permissions.custom]
read = "allow"
search = "allow"
edit = "allow"
shell = "allow"
ignored_search = "allow"
network = "ask"
mcp = "ask"
git = "allow"
compiler = "allow"
destructive = "ask"

[permissions.ai_reviewer]
# auto_review forces enabled=true and allow_capabilities to this set.
enabled = false
allow_capabilities = ["read", "search", "network", "mcp"]

[permissions.shell_sandbox]
# default/auto_review use allow_when_approved unless explicitly configured;
# full_access turns the shell sandbox off.
network = "allow_when_approved"

[[permissions.rules]]
capability = "shell"
target = "cargo test:*"
action = "allow"
source = "user"

[[permissions.rules]]
capability = "network"
target = "shell:curl:*"
action = "ask"
source = "project"
```

Target prefixes Squeezy understands:

- `path:<rel-path>` for `edit`.
- `domain:<host>` for `webfetch` network.
- `search:<provider>` for search network (`search:exa`).
- `workspace:*`, `ignored:*` for read/search scope.
- `tool:<name>` for arbitrary tools without their own scope.
- `<mcp-server>/<tool-name>` for MCP tools.
- `<cmd-prefix>:*` for shell/git/compiler (`cargo test:*`, `rm:*`).
- `shell:<cmd-prefix>:<host>` for parsed network commands.

Allow rules on `destructive` and `*`/`**` wildcard targets are refused at
config load, at session-approval persistence, and at runtime evaluation.
Use narrower targets.

### Add a skills directory or disable a skill

```toml
[skills]
user_dir = "/path/to/squeezy-skills"
compat_user_dir = "/path/to/agent-skills"
extra_roots = ["/mnt/team-skills"]
active_budget_chars = 4000
active_body_cap_chars = 16000
preamble_enabled = true
preamble_budget_chars = 800
active_budget_mode = { context_percent = 2.0 }
preamble_budget_mode = { context_percent = 2.0 }
inline = false
hooks_enabled = false

[[skills.config]]
name = "noisy-project-skill"
enabled = false

[[skills.config]]
path = ".squeezy/skills/specific-skill"
enabled = true
```

Skills live at one of `~/.squeezy/skills/`, `~/.agents/skills/`,
configured `extra_roots`, `<workspace>/.squeezy/skills/`, or
`<workspace>/.agents/skills/`. Project tiers override user tiers; native tiers
override compat tiers. Ancestor workspace skill roots are discovered for nested
monorepo packages.
`[[skills.config]]` selects by exact `name` OR by `path` (never both).

### Budgets and limits

```toml
[budgets]
max_parallel_tools = 8
max_tool_calls_per_turn = 64
max_tool_bytes_read_per_turn = 20000000
max_search_files_per_turn = 50000
```

### Session mode and logs

```toml
[session]
mode = "build"            # build | plan
log_dir = ".squeezy/sessions"
log_retention_days = 30
```

`Shift+Tab` toggles modes in the TUI; `/plan` and `/build` force a mode.
Plan mode advertises only read/search/navigation tools and refuses
edit/shell/git/network/MCP/compiler before normal permission checks.

## Schema sections (high-level)

`[model]`, `[providers.<id>]`, `[agent]`, `[session]`, `[context]`,
`[subagents]`, `[budgets]`, `[permissions]`, `[permissions.ai_reviewer]`,
`[permissions.shell_sandbox]`, `[[permissions.rules]]`, `[hardening]`,
`[mcp.servers.<name>]`, `[mcp.servers.<name>.permissions]`,
`[[mcp.servers.<name>.permissions.rules]]`, `[telemetry]`, `[feedback]`,
`[redaction]`, `[web]`, `[graph]`, `[cache]`, `[tools]`, `[tui]`,
`[skills]`, `[[skills.config]]`.

See `crates/squeezy-skills/external-docs/CONFIGURATION.md` for the per-field
reference, defaults, and apply-tier (immediate / next prompt / restart required).

## Workflow

1. Decide scope: user-wide (`~/.squeezy/settings.toml`), shared repo
   (`squeezy.toml`), or per-repo machine-local
   (`~/.squeezy/projects/<repo-id>/settings.toml`).
2. Use `squeezy config init --user` or `--project` to generate a commented
   skeleton; `--force` is required to overwrite an existing file.
3. Make the edit. Keep secret values out of TOML; reference an env var
   name via `api_key_env`.
4. Run `squeezy config inspect` to verify the effective merged config and
   confirm which source supplied each value.
5. Run `squeezy doctor` to validate the merged config.
6. If the field's apply tier is "restart required" (e.g. `[graph]`,
   `[session].log_dir`), restart the TUI; the config screen also surfaces a
   "restart required" notification.

Unknown fields, invalid enum values, and invalid numeric limits are
warned and stripped, not silently accepted. Provider config errors stay
provider-config errors — they do not get swallowed at load time.
