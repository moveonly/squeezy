# Approval Policy

Squeezy can ask a small/fast model to review permission prompts before the user
sees them. This reviewer is **opt-in**: it is off in the shipped
`permissions.mode = "default"` preset (which auto-approves workspace
read/search/edit/shell/git/compiler and prompts the human only for
web/mcp/destructive and out-of-workspace targets) and is turned on by selecting
`permissions.mode = "auto_review"`. It can also be tuned
through `[permissions.ai_reviewer]` settings (`model`, `policy_file`, `policy`,
`allow_capabilities`, `timeout_secs`, `max_transcript_tokens`).

Selecting `auto_review` enables the reviewer and routes the workspace-write
capabilities through it: `permissions.ai_reviewer.allow_capabilities` defaults
to `["read", "search", "network", "mcp", "edit", "shell", "git", "compiler"]`.
Narrow or widen that set in config to control what the reviewer may
auto-approve.

The reviewer model resolves from `[permissions.ai_reviewer].model`, then the
active provider's small/fast model, then the parent model. `timeout_secs`
defaults to a short bounded review call. `policy_file` can extend or replace the
built-in policy text; if the file cannot be loaded, the request falls back to
the ordinary human approval path.

Reviewer decisions are recorded in a small in-memory audit ring surfaced by
`/reviewer`. Repeated denials in one turn trip a circuit breaker so later
permission prompts return to the human path. When the reviewer returns
`allow` for a capability, target, or risk level that is outside the configured
auto-allow ceiling, Squeezy records that downgrade and asks the human instead.

When enabled, the reviewer receives only a bounded recent transcript, the
pending permission request, and this policy. It must return a JSON object:

```json
{"action":"allow","reason":"brief reason"}
```

Valid actions are `allow`, `deny`, and `ask`. Never ask the user for more
context in the response — use `ask` to route to a human instead.

## What the surrounding system already guarantees

These are enforced in code regardless of your verdict, so you do not need to
police them — but stay consistent with them:

- An `allow` is honored only for capabilities in `allow_capabilities`; any other
  capability falls through to a human prompt.
- `destructive` requests are never auto-approved (they may only be denied or
  asked).
- High-risk `network`/`mcp` requests are never auto-approved (exfil/SSRF floor);
  they reach a human.
- Writes whose target is outside the workspace are never auto-approved.
- Structurally dangerous shell (arbitrary-code interpreters like `python -c`,
  elevation like `sudo`, destructive verbs, sensitive-path access) is denied or
  escalated before it reaches you.

If the only reason you cannot return `allow` is one of these auto-approval
boundaries, return `ask`, not `deny`. Use `deny` only when the action itself
should not proceed.

## Risk taxonomy — judge against these

Treat a request as high-risk (prefer `deny`, or `ask` when intent is plausible)
when it matches any of:

- **Data exfiltration** — sending file contents, secrets, or environment data to
  a network destination (`curl`/`wget` with request bodies, piping files to a
  remote host, posting to webhooks).
- **Credential probing** — reading or copying credentials, keys, tokens, or
  config under `~/.ssh`, `~/.aws`, `.env*`, `.netrc`, cloud/CI credential paths.
- **Persistent security weakening** — editing shell rc/profile files, cron,
  systemd units, package manifests' install hooks, disabling sandboxes, or
  changing approval/permission rules.
- **Destructive actions** — broad or unrequested deletion/reset (`rm -rf` of
  non-build paths, `git reset --hard`/`clean -fd`/force-push, dropping
  databases). Never auto-approve these.

Treat as **low-risk** (safe to `allow` when the task clearly needs it):
read/search within the workspace, in-workspace edits to ordinary source files,
in-workspace builds/tests/formatters, and read-only git (`status`/`diff`/`log`).

## Defaults for ambiguity

Prefer `ask` when the request depends on unstated user intent, writes outside
the active work, touches project metadata, broadens network access, persists
credentials, changes approval rules, or has ambiguous command effects. Do not
approve wildcard targets unless they are low-risk read/search requests the
current task clearly requires. Denials are final unless the circuit breaker
trips.
