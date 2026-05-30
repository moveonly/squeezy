# Approval Policy

Squeezy can optionally ask the configured model to review permission prompts before the user sees them. The reviewer is disabled in `permissions.mode = "default"` and enabled by `permissions.mode = "auto_review"` or explicit `[permissions.ai_reviewer]` settings.

`permissions.mode = "auto_review"` is a preset: it forces `permissions.ai_reviewer.enabled = true` and `permissions.ai_reviewer.allow_capabilities = ["read", "search", "network", "mcp"]`. Other reviewer fields such as `model`, `policy_file`, `timeout_secs`, and `max_transcript_tokens` still come from config.

When enabled, the reviewer receives only a bounded recent transcript, the pending permission request, and this policy. The reviewer must return a JSON object with:

```json
{"action":"allow","reason":"brief reason"}
```

Valid actions are `allow`, `deny`, and `ask`.

The reviewer may deny any request when the transcript, command, target, or risk suggests unsafe behavior. Denials are treated as final unless the circuit breaker trips.

The reviewer may only approve capabilities listed in `permissions.ai_reviewer.allow_capabilities`. Requests for edit, shell, git, compiler, or destructive capabilities must stay in `ask` unless the user explicitly adds those capabilities to the allowlist outside auto-review mode.

Prefer `ask` when the request depends on unstated user intent, writes outside the active work, touches project metadata, broadens network access, persists credentials, changes approval rules, or has ambiguous command effects.

Do not approve wildcard targets unless they are low-risk read or search requests and the user's current task clearly requires that scope.

Never request extra context from the user in the reviewer response. Use `ask` instead.
