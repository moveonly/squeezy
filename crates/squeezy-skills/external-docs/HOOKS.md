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
in-process handlers registered against the `HookRegistry`, not by skill hook
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

**Windows — PowerShell 7 (`pwsh`) or Windows PowerShell 5 (`powershell`):**

Squeezy resolves the shell and invokes it with `-NoProfile -Command <your command>`.
Write the `command` field as a PowerShell expression, not as a `pwsh` invocation:

```powershell
# scripts/pre-turn.ps1
$payload = $env:SQUEEZY_HOOK_PAYLOAD | ConvertFrom-Json
$payload | ConvertTo-Json | Add-Content "$env:SQUEEZY_SKILL_DIR\hooks.log"
exit 0
```

```yaml
hooks:
  PreTurn:
    - matcher: "*"
      hooks:
        - type: command
          command: "& .\\scripts\\pre-turn.ps1"
```

The `& .\scripts\...` call-operator syntax works with both `pwsh` and
`powershell`; Squeezy picks whichever shell it finds first on `PATH`.

**Windows — cmd.exe:**

```yaml
hooks:
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts\audit-shell.cmd
```

**Windows — cmd.exe:**

```yaml
hooks:
  PreTurn:
    - matcher: "*"
      hooks:
        - type: command
          command: scripts\pre-turn.cmd
```

Inside a `.cmd` script, use `%SQUEEZY_HOOK_PAYLOAD%`:

```cmd
@echo off
echo %SQUEEZY_HOOK_PAYLOAD% >> %SQUEEZY_SKILL_DIR%\hooks.log
exit /b 0
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
Hook commands run with the privileges of the Squeezy process, so only enable
this for trusted skill catalogs.

> **Warning**: setting `hooks_enabled = true` is a high-trust operation.
> `squeezy doctor` will flag this configuration as a warning so it is visible
> in CI smoke runs. Use it only with skill catalogs you fully control.

**Shell selection**: on POSIX platforms, hooks run through `/bin/sh -c`. On
Windows, Squeezy tries `pwsh` (PowerShell 7), then `powershell` (Windows
PowerShell 5), then `cmd /C`, using the first one found on `PATH`. Run
`squeezy doctor` to confirm which shell will be used — the `hooks:shell` row
reports the resolved shell or warns when none is available.

Spawn failures (including "shell not found" on Windows) are fail-open: the
action is allowed and a warning is emitted. A hook that should deny an action
must actually execute and exit non-zero; if the hook shell is missing it cannot
block.

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
- `failure_policy` — set to `deny` to make spawn failures deny the action.
  Defaults to `allow` for backward compatibility.

### Windows note

On Windows, `.ps1` scripts can be used when `pwsh` or `powershell` is available
in `PATH`; otherwise Squeezy falls back to `cmd /C`. PowerShell-native syntax
will not run under the `cmd` fallback.

If no hook shell is available and `hooks_enabled = true`, Squeezy will warn in
`squeezy doctor` and hook dispatch will fail to spawn. To make a policy hook
deny the action on spawn failure, add `failure_policy: deny` to the hook spec:

```yaml
hooks:
  PreToolUse:
    - matcher: shell
      hooks:
        - type: command
          command: scripts/audit-shell.sh
          timeout: 10
          fail_open: false
          failure_policy: deny
```

Without `failure_policy: deny`, a spawn failure silently allows the action
(backward-compatible default). PowerShell-native hook support (`pwsh -File ...`)
is on the roadmap.

## Environment Variables In Hook Scripts

Scripts receive the following environment:

- `SQUEEZY_HOOK_PAYLOAD` — the full JSON event payload (see event table above).
- `SQUEEZY_HOOK_PAYLOAD_FILE` — path to a temp file containing the same JSON,
  set when the payload exceeds ~8 KiB. Scripts can read from either source.
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

**Windows environment variable syntax:**

| Shell | Payload access |
|-------|---------------|
| PowerShell (`pwsh` / `powershell`) | `$env:SQUEEZY_HOOK_PAYLOAD` |
| cmd.exe | `%SQUEEZY_HOOK_PAYLOAD%` |
| sh / bash | `$SQUEEZY_HOOK_PAYLOAD` |

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
