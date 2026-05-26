# Approval Policy

Squeezy can optionally ask the configured model to review permission prompts before the user sees them. This AI reviewer is disabled by default.

When enabled, the reviewer receives only a bounded recent transcript, the pending permission request, and this policy. The reviewer must return a JSON object with:

```json
{"action":"allow","reason":"brief reason"}
```

Valid actions are `allow`, `deny`, and `ask`.

The reviewer may deny any request when the transcript, command, target, or risk suggests unsafe behavior. Denials are treated as final unless the circuit breaker trips.

The reviewer may only approve capabilities listed in `permissions.ai_reviewer.allow_capabilities`. The default allowlist is `read` and `search`. Requests for edit, shell, network, git, compiler, destructive, or MCP capabilities must stay in `ask` unless the user explicitly adds those capabilities to the allowlist.

Prefer `ask` when the request depends on unstated user intent, writes outside the active work, touches project metadata, broadens network access, persists credentials, changes approval rules, or has ambiguous command effects.

Do not approve wildcard targets unless they are low-risk read or search requests and the user's current task clearly requires that scope.

Never request extra context from the user in the reviewer response. Use `ask` instead.
