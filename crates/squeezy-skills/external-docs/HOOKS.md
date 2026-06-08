# Hooks

Hooks are lifecycle points fired by the Squeezy agent loop. The internal hook
engine supports typed mutation hooks, but user-authored skill hooks are a
smaller opt-in shell-script surface: they receive the event payload in an
environment variable and can allow or deny selected actions by exit status.
Skill hook scripts do not read stdout for mutations.

## Hook Events

The internal hook enum includes these lifecycle events:

| Event | When it fires | Enforcement capability |
|-------|--------------|------------------------|
| `PreTurn` | Before the model receives the user prompt | Observation only (typed internal handlers can append `extra_instructions`) |
| `UserPromptSubmit` | When the user submits a prompt | Observation only (typed internal handlers can rewrite the prompt) |
| `PreToolUse` | Before a tool call executes | **Can deny the call** (non-zero exit) |
| `PostToolUse` | After any tool call completes (success or failure) | Observation only |
| `PostToolUseFailure` | After a tool call returns a non-success status | Observation only |
| `PostTool` | After any tool call result is appended to the conversation | Observation only |
| `PreCompact` | Before context compaction runs | Observation only |
| `PostCompact` | After context compaction completes | Observation only |
| `SubagentStart` | When a subagent is spawned | Observation only |
| `SubagentStop` | When a subagent finishes | Observation only |
| `PermissionRequest` | When a tool asks for permission | **Can deny the request** (non-zero exit) |
| `PermissionDenied` | When a permission request is denied | Observation only |
| `SessionStart` | When a session begins | Observation only |
| `Stop` | When the session ends | Observation only |
| `Setup` | On first startup / initial configuration | Observation only |

## Mutation Capabilities

Two events support typed internal mutations. These mutations are applied by
in-process handlers registered against the `AgentHookBus`, not by skill hook
scripts — skill hook script stdout is always ignored.

**`PreTurn`** — an internal handler can return `extra_instructions` in its
`HookResult::mutate` value. Squeezy appends that string to the system prompt
for the current turn. Skill hook scripts cannot trigger this mutation.

**`UserPromptSubmit`** — an internal handler can return a replacement `prompt`
in its `HookResult::mutate` value. Squeezy replaces the user's submitted text
with the returned value. Skill hook scripts cannot trigger this mutation.

### Skill hook scripts

Skill hooks run as shell commands. Their stdout is ignored. The only mechanism
available to skill hooks is exit status:

- **Exit 0** — allow the event to proceed.
- **Non-zero exit** — deny the action at enforcement-capable events
  (`PreToolUse` and `PermissionRequest`); at observation-only events, a
  non-zero exit is logged but does not block the action.

## Hook Scripts

Hook scripts are executables placed in a skill's `scripts/` directory. They are
registered when the skill is activated, and are called with a JSON payload
delivered via the `SQUEEZY_HOOK_PAYLOAD` environment variable (not stdin, unlike
the legacy convention).

A minimal shell hook:

```sh
#!/usr/bin/env sh
# scripts/pre-turn.sh
# Inspect the payload and allow the event.
printf '%s\n' "$SQUEEZY_HOOK_PAYLOAD" >> "$SQUEEZY_SKILL_DIR/hooks.log"
exit 0
```

Declare hooks in the skill's `SKILL.md` frontmatter:

```yaml
hooks:
  PreTurn:
    - matcher: "*"
      hooks:
        - type: command
          command: scripts/pre-turn.sh
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts/audit-shell.sh
```

The skill frontmatter parser accepts all event keys listed in the table above
(PascalCase or their `snake_case` aliases): `PreTurn`, `PreToolUse`,
`PostToolUse`, `PostToolUseFailure`, `PostTool`, `PreCompact`, `PostCompact`,
`SubagentStart`, `SubagentStop`, `PermissionRequest`, `PermissionDenied`,
`UserPromptSubmit`, `SessionStart`, `Stop`, and `Setup`.

- `matcher` is a tool-name filter for payloads that include `tool_name`. Use
  `"*"` or omit it to match all payloads for the event.
- `once: true` causes the hook to fire only on its first successful invocation
  per session.

## Configuration

Hooks are disabled by default. When `[skills].hooks_enabled = true`, all
non-disabled discovered skills that declare `hooks:` frontmatter have their
handlers registered against the session's `HookRegistry` at agent startup.
Hook commands run through `sh -c` with the privileges of the Squeezy process,
so only enable this for trusted skill catalogs.

The `[skills]` section controls skill discovery:

```toml
[skills]
user_dir = "~/.squeezy/skills"
# project skills live under <workspace>/.squeezy/skills/
hooks_enabled = true
```

## Environment Variables In Hook Scripts

Scripts receive the following environment:

- `SQUEEZY_HOOK_PAYLOAD` — the full JSON event payload (see event table above).
- `SQUEEZY_SKILL_DIR` — absolute path to the skill's base directory.
- `SQUEEZY_SKILL_NAME` — the skill's registered name.

## Use Cases

- **Audit logging**: write tool calls or session events to a local log file.
- **Policy enforcement**: deny shell commands that match a blocklist on
  `PreToolUse`.
- **Observability**: emit structured telemetry events to an internal system.

See [SKILLS.md](SKILLS.md) for the full skill authoring guide.
