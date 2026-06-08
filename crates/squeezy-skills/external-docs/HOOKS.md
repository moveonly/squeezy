# Hooks

Hooks are lifecycle points fired by the Squeezy agent loop. The internal hook
engine supports typed mutation hooks, but user-authored skill hooks are a
smaller opt-in shell-script surface: they receive the event payload in an
environment variable and can allow or deny selected actions by exit status.
Skill hook scripts do not read stdout for mutations.

## Hook Events

The internal hook enum includes these lifecycle events:

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

Typed internal handlers can mutate two events:

**`PreTurn`** — the hook script can return a JSON object with an
`extra_instructions` key. Squeezy appends that string to the system prompt for
the current turn. Use this to inject per-turn context, enforce policy, or add
dynamic instructions.

**`UserPromptSubmit`** — the hook script can return a JSON object with a
`prompt` key. Squeezy replaces the user's submitted text with the returned
value. Use this for prompt enrichment, templating, or redaction before the
model sees the input.

Skill hook scripts cannot currently return those mutations. Their stdout is
ignored; a zero exit status allows execution to continue, and a non-zero exit
status returns a deny result at the dispatch site.

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

The skill frontmatter parser currently accepts only these event keys:
`PreTurn`, `PreToolUse`, `PostToolUse`, `PostTool`, `PreCompact`,
`PostCompact`, `SubagentStart`, and `PermissionRequest` (or their snake_case
aliases).

- `matcher` is a tool-name filter for payloads that include `tool_name`. Use
  `"*"` or omit it to match all payloads for the event.
- `once: true` causes the hook to fire only on its first successful invocation
  per session.

## Configuration

Hooks are disabled by default. When `[skills].hooks_enabled = true` and a skill
with `hooks:` is activated, its hook handlers are registered against the
session's `HookRegistry`. Hook commands run through `/bin/sh -c` (absolute path
on POSIX; `sh` on Windows) with the privileges of the Squeezy process, so only
enable this for trusted skill catalogs.

> **Warning**: setting `hooks_enabled = true` is a high-trust operation.
> `squeezy doctor` will flag this configuration as a warning so it is visible
> in CI smoke runs. Use it only with skill catalogs you fully control.

The `[skills]` section controls skill discovery:

```toml
[skills]
user_dir = "~/.squeezy/skills"
# project skills live under <workspace>/.squeezy/skills/
hooks_enabled = true
```

### Per-hook options

In addition to `command` and `once`, each hook spec accepts:

- `timeout` — maximum seconds to wait before killing the hook process and
  returning a deny result. Defaults to 30 seconds. A timed-out hook returns
  deny so it does not silently pass while blocking the turn.
- `fail_open` — when `false` (fail-closed), a spawn error (e.g. `/bin/sh` not
  found, file-descriptor exhaustion) returns a deny result instead of silently
  allowing execution. Defaults to `true` for backward compatibility.

```yaml
hooks:
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts/audit-shell.sh
          timeout: 10
          fail_open: false
```

## Environment Variables In Hook Scripts

Scripts receive the following environment:

- `SQUEEZY_HOOK_PAYLOAD` — the full JSON event payload (see event table above).
- `SQUEEZY_SKILL_DIR` — absolute path to the skill's base directory.
- `SQUEEZY_SKILL_NAME` — the skill's registered name.

## Exit Codes and Diagnostics

Hook scripts communicate intent through exit status:

| Exit code | Meaning | Squeezy message |
|-----------|---------|-----------------|
| `0` | Allow; hook succeeded | (no message) |
| `1`–`125` | Deny the action | `skill '<name>' hook denied the action` |
| `126` | Command not executable | `skill '<name>' hook: command not executable (exit 126)` |
| `127` | Interpreter or command not found | `skill '<name>' hook: interpreter or command not found (exit 127)` |

Exit codes 126 and 127 appear when the hook script path is correct but either
the executable bit is not set (`chmod +x`) or the shebang interpreter is not
found. Run `squeezy doctor` to detect these issues before starting a session.

## Shell Behavior on Linux

Squeezy invokes hook commands as `/bin/sh -c "<command>"` on POSIX platforms.
`/bin/sh` varies by distribution:

- **Debian / Ubuntu** — `dash` (POSIX shell, strict mode)
- **Fedora / RHEL** — `bash` in POSIX mode
- **Alpine** — `ash` / `busybox sh`

Write hooks in portable POSIX sh unless you explicitly need Bash features. If
a hook requires Bash, use an explicit shebang in the script file:

```sh
#!/usr/bin/env bash
```

Inline shell snippets (containing `|`, `&&`, `;`, `>`, etc.) are allowed but
depend on the distro's `/bin/sh` semantics. `squeezy doctor --hooks` notes
inline snippets so you can verify they are portable.

## Timeout and Process Cleanup

Each hook runs with a timeout (default: 30 seconds, configurable per hook with
`timeout: <secs>`). On timeout, Squeezy sends `SIGKILL` to the hook's process
group so any grandchild processes spawned by the hook script are also
terminated. The hook returns a deny result on timeout.

## Linux Examples

### Block sudo calls

```yaml
hooks:
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts/no-sudo.sh
          fail_open: false
```

```sh
#!/bin/sh
# scripts/no-sudo.sh
# Deny any shell command that invokes sudo.
payload="$SQUEEZY_HOOK_PAYLOAD"
cmd=$(printf '%s' "$payload" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('arguments',{}).get('command',''))" 2>/dev/null || true)
case "$cmd" in
  *sudo*) exit 1 ;;
esac
exit 0
```

### Audit all tool calls

```yaml
hooks:
  PreToolUse:
    - matcher: "*"
      hooks:
        - type: command
          command: scripts/audit.sh
```

```sh
#!/bin/sh
# scripts/audit.sh
printf '%s\t%s\n' "$(date -Iseconds)" "$SQUEEZY_HOOK_PAYLOAD" >> "$SQUEEZY_SKILL_DIR/audit.log"
exit 0
```

### Block systemctl writes

```yaml
hooks:
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts/no-systemctl-write.sh
          fail_open: false
```

```sh
#!/bin/sh
# scripts/no-systemctl-write.sh
# Deny systemctl start/stop/enable/disable/mask.
payload="$SQUEEZY_HOOK_PAYLOAD"
cmd=$(printf '%s' "$payload" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('arguments',{}).get('command',''))" 2>/dev/null || true)
case "$cmd" in
  *systemctl\ start*|*systemctl\ stop*|*systemctl\ enable*|*systemctl\ disable*|*systemctl\ mask*)
    exit 1 ;;
esac
exit 0
```

## Use Cases

- **Audit logging**: write tool calls or session events to a local log file.
- **Policy enforcement**: deny shell commands that match a blocklist on
  `PreToolUse`.
- **Observability**: emit structured telemetry events to an internal system.
- **Package-manager guardrails**: block `apt install`, `pip install --user`, or
  `npm install -g` on `PreToolUse` to enforce workspace-local dependency
  management.
- **Workspace-bound shell auditing**: log every shell command with its
  arguments to a structured audit trail for compliance workflows.

See [SKILLS.md](SKILLS.md) for the full skill authoring guide.
