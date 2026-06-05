# Hooks

Hooks are observation and mutation points fired at key lifecycle events in the
Squeezy agent loop. Skills, telemetry, and MCP integration use hooks internally;
you can also write your own hook scripts in a skill's `scripts/` directory.

## Hook Events

Each event fires at a specific point in the agent loop:

| Event | When it fires | Mutation capability |
|-------|--------------|---------------------|
| `PreTurn` | Before the model receives the user prompt | Append `extra_instructions` to the system prompt |
| `UserPromptSubmit` | When the user submits a prompt | Rewrite or augment the prompt text |
| `PreToolUse` | Before a tool call executes | Can deny the call (non-zero exit) |
| `PostToolUse` | After a tool call succeeds | Observation only |
| `PostToolUseFailure` | After a tool call fails | Observation only |
| `PostTool` | After any tool call completes (success or failure) | Observation only |
| `PreCompact` | Before context compaction runs | Observation only |
| `PostCompact` | After context compaction completes | Observation only |
| `SubagentStart` | When a subagent is spawned | Observation only |
| `SubagentStop` | When a subagent finishes | Observation only |
| `PermissionRequest` | When a tool asks for permission | Can deny the request |
| `PermissionDenied` | When a permission request is denied | Observation only |
| `SessionStart` | When a session begins | Observation only |
| `Stop` | When the session ends | Observation only |
| `Setup` | On first startup / initial configuration | Observation only |

## Mutation Capabilities

Two events support mutations that affect agent behavior:

**`PreTurn`** — the hook script can return a JSON object with an
`extra_instructions` key. Squeezy appends that string to the system prompt for
the current turn. Use this to inject per-turn context, enforce policy, or add
dynamic instructions.

**`UserPromptSubmit`** — the hook script can return a JSON object with a
`prompt` key. Squeezy replaces the user's submitted text with the returned
value. Use this for prompt enrichment, templating, or redaction before the
model sees the input.

For all other events, the hook runs as an observer. Returning a non-zero exit
code on `PreToolUse` or `PermissionRequest` denies the action.

## Hook Scripts

Hook scripts are executables placed in a skill's `scripts/` directory. They are
registered when the skill is activated, and are called with a JSON payload
delivered via the `SQUEEZY_HOOK_PAYLOAD` environment variable (not stdin, unlike
the legacy convention).

A minimal shell hook:

```sh
#!/usr/bin/env sh
# scripts/pre-turn.sh
# Append an instruction on every PreTurn event.
echo '{"extra_instructions": "Always cite sources."}'
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

- `matcher` is a tool-name filter. Use `"*"` or omit it to match all payloads
  for the event.
- `once: true` causes the hook to fire only on its first successful invocation
  per session.

## Configuration

Hooks are enabled automatically through the skill system. When a skill with
hooks is activated, its hook handlers are registered against the session's
`HookRegistry`. No separate configuration key is needed.

The `[skills]` section controls skill discovery:

```toml
[skills]
user_dir = "~/.squeezy/skills"
# project skills live under <workspace>/.squeezy/skills/
```

## Environment Variables In Hook Scripts

Scripts receive the following environment:

- `SQUEEZY_HOOK_PAYLOAD` — the full JSON event payload (see event table above).
- `SQUEEZY_SKILL_DIR` — absolute path to the skill's base directory.
- `SQUEEZY_SKILL_NAME` — the skill's registered name.

## Use Cases

- **Audit logging**: write tool calls or session events to a local log file.
- **Prompt enrichment**: inject dynamic instructions (date, project context,
  policy reminders) on every `PreTurn`.
- **Policy enforcement**: deny shell commands that match a blocklist on
  `PreToolUse`.
- **Observability**: emit structured telemetry events to an internal system.
- **Prompt rewriting**: normalize or template user input on `UserPromptSubmit`.

See [SKILLS.md](SKILLS.md) for the full skill authoring guide.
