# Trust, Privacy, And Safety Website Research

Audit date: 2026-06-05. Scope: local repository only, no network verification.
The prompt referenced `docs/external`; this checkout stores the bundled external
docs at `crates/squeezy-skills/external-docs/`.

This note is for website positioning. It separates public-safe claims from
implementation caveats so the site can tell a credible trust story without
overstating security guarantees.

## Short Answer

Squeezy can safely claim a local-first, reviewable operating posture:

- Local code navigation, sessions, checkpoints, shell audit logs, and redacted
  reports live in the workspace or local cache by default.
- Product telemetry is automatic and default-on, but is designed as anonymous
  aggregate session telemetry, not prompt/file/path/command collection.
- Feedback and diagnostic reports are separate consented flows with local
  redaction, preview, byte caps, and explicit send steps.
- Permission policy is the primary execution gate. Optional shell sandboxing is
  defense in depth where the host platform can enforce it.
- Checkpoints are an opt-in local undo aid for agent mutations, not a privacy
  scrubber and not a replacement for Git.

The strongest copy should use words like "reviewable", "permissioned",
"bounded", "local", "redacted", "opt-out telemetry", and "consented reports".
Avoid "secure sandbox", "private by default" for all data surfaces, and
"guaranteed rollback".

## Public-Safe Claims

| Area | Public-safe claim | Caveats to keep visible | Source refs |
| --- | --- | --- | --- |
| Permission policy | Squeezy has capability-based permissions for read/search/edit/shell/web/MCP/git/compiler/destructive work. Default mode allows routine workspace work while asking for web, MCP, and destructive actions. | Default mode allows in-workspace edit, shell, git, and compiler actions; do not imply every mutation prompts. Out-of-workspace read/edit/shell targets escalate unless full-access is selected. | `crates/squeezy-core/src/lib.rs:6744-6865`, `crates/squeezy-core/src/lib.rs:6954-6971`, `crates/squeezy-core/src/lib.rs:7036-7120` |
| Auto-review | Auto-review is optional. When selected, a small/fast model can review bounded permission prompts, but auto-approval is limited by capability, risk, and workspace scope. | The shipped `default` preset does not enable the AI reviewer. Critical/destructive requests are not auto-approved; high-risk network/MCP requests and out-of-workspace requests fall through to a human. | `crates/squeezy-skills/external-docs/APPROVAL_POLICY.md:3-18`, `crates/squeezy-skills/external-docs/APPROVAL_POLICY.md:31-40`, `crates/squeezy-agent/src/ai_reviewer.rs:21-59`, `crates/squeezy-agent/src/ai_reviewer.rs:264-317` |
| Shell sandboxing | Approved shell commands can run with OS-level containment on supported macOS/Linux hosts, plus command classification, environment allowlisting, timeouts, output caps, sensitive-path checks, metadata-directory checks, and audit records. | Permission policy is the strong gate; sandboxing is best-effort defense in depth unless `mode = "required"` is set. `best_effort` can fall back without OS isolation when the host refuses the backend. | `crates/squeezy-skills/external-docs/SHELL_SANDBOXING.md:3-12`, `crates/squeezy-skills/external-docs/SHELL_SANDBOXING.md:19-61`, `crates/squeezy-tools/src/shell_sandbox.rs:377-400`, `crates/squeezy-tools/src/lib.rs:1632-1686` |
| Windows shell posture | On Windows, Squeezy uses a Job Object to kill the process tree on timeout/cancel. | Windows does not provide Squeezy filesystem or network isolation. The docs say `mode = "required"` denies pre-spawn on Windows; use `best_effort` or `external`. Make this caveat explicit anywhere "sandbox" appears. | `crates/squeezy-skills/external-docs/SHELL_SANDBOXING.md:75-86`, `crates/squeezy-tools/src/win_job.rs:1-5`, `crates/squeezy-tools/src/win_job.rs:23-69` |
| Network posture | In the shipped `default` and `auto_review` permission presets, sandbox network opens only when a command is classified as network and permission policy allows it. | `ShellSandboxConfig::default()` starts at `deny_by_default`, but the permission presets override to `allow_when_approved` unless explicitly configured. `full_access` turns the shell sandbox off. | `crates/squeezy-core/src/lib.rs:6322-6347`, `crates/squeezy-core/src/lib.rs:6849-6860`, `crates/squeezy-tools/src/shell_sandbox.rs:515-529` |
| Automatic product telemetry | Squeezy sends anonymous product telemetry by default and prints a first-use notice. Automatic telemetry is scoped to aggregate usage, reliability, cost, and performance summaries. | It is default-on, so do not say telemetry is opt-in. Users can opt out with `SQUEEZY_TELEMETRY=off` or equivalent disabled values. | `crates/squeezy-skills/external-docs/TELEMETRY.md:3-18`, `crates/squeezy-core/src/lib.rs:7275-7335`, `crates/squeezy-cli/src/main.rs:3199-3217` |
| Telemetry exclusions | Public copy can say automatic product telemetry does not send prompts, model responses, tool arguments, file contents, paths, repo names, shell commands/output, URLs, API keys, env values, or settings contents. | These exclusions apply to automatic product telemetry. Consented feedback/report flows have their own preview/redaction story. | `crates/squeezy-skills/external-docs/TELEMETRY.md:89-109`, `crates/squeezy-skills/external-docs/TELEMETRY.md:111-118` |
| Telemetry delivery | Product telemetry is reduced into bounded session summaries and sent through a Cloudflare Worker proxy; the binary does not contain the PostHog token. | The local telemetry ledger can hold safe facts until upload succeeds. If telemetry cannot persist a stable install id, it disables itself rather than minting a new identity per process. | `crates/squeezy-skills/external-docs/TELEMETRY.md:46-53`, `crates/squeezy-skills/external-docs/TELEMETRY.md:75-87`, `crates/squeezy-skills/external-docs/TELEMETRY.md:120-132`, `crates/squeezy-telemetry/src/lib.rs:101-149`, `crates/squeezy-telemetry/src/lib.rs:619-755` |
| Worker validation | The telemetry Worker validates strict envelopes, caps body/event counts, allows only `squeezy_*` product event names, sanitizes property names/values, and limits website CORS to `https://squeezyagent.com`. | Worker validation is a collection boundary, not a promise about third-party service retention or legal policy. Public privacy copy should still say data is forwarded to PostHog/R2 where applicable. | `infra/telemetry-worker/src/worker.ts:1-15`, `infra/telemetry-worker/src/worker.ts:81-121`, `infra/telemetry-worker/src/worker.ts:275-305`, `infra/telemetry-worker/src/worker.ts:460-520`, `infra/telemetry-worker/src/worker.ts:651-657` |
| Website telemetry | Website telemetry is separate from product telemetry and is limited to anonymous visitor/session IDs, site-local paths, coarse referrer kind, bounded UTM fields, and CTA/target identifiers. | The website endpoint is distinct from binary telemetry. Do not conflate site visitor telemetry with CLI/TUI session summaries. | `crates/squeezy-skills/external-docs/TELEMETRY.md:31-44`, `crates/squeezy-skills/external-docs/TELEMETRY.md:103-105`, `infra/telemetry-worker/src/worker.ts:125-170`, `infra/telemetry-worker/src/worker.ts:307-353` |
| Feedback | `/feedback` sends a short redacted user-written message only after preview/confirmation. | The redacted feedback message itself is forwarded to PostHog. Public copy should say "redacted feedback text" rather than "metadata only". | `crates/squeezy-skills/external-docs/FEEDBACK.md:3-16`, `crates/squeezy-telemetry/src/lib.rs:445-480`, `crates/squeezy-telemetry/src/lib.rs:548-580`, `infra/telemetry-worker/src/worker.ts:172-208` |
| Diagnostic reports | `/report` and `squeezy sessions report` build redacted diagnostic archives with preview, section list, byte size, and redaction count. Upload is explicit; CLI writes a local archive if upload fails. | Report archives can include redacted session and replay data. Archives are stored in private R2; PostHog receives report metadata, not the archive body. Users should not paste archive contents into public issues. | `crates/squeezy-skills/external-docs/FEEDBACK.md:17-35`, `crates/squeezy-store/src/reports.rs:79-254`, `crates/squeezy-cli/src/main.rs:1909-1964`, `infra/telemetry-worker/src/worker.ts:211-272` |
| Report contents and caps | Report sections include version, redacted config, repo profile, session metadata, events, tool/cost summaries, permissions, diagnostics, replay, and manifest. Sections and archive size are capped; oversized sections are omitted with reason metadata. | "Redacted" is not "secret-proof". The report builder runs the configured redactor and size caps, but users should preview before upload. | `crates/squeezy-store/src/reports.rs:99-206`, `crates/squeezy-store/src/reports.rs:208-237`, `crates/squeezy-store/src/reports.rs:340-401` |
| Local sessions | Sessions are local files with metadata, events, resume state, attachments, and replay tape. They support list/show/resume/replay/export/report/cleanup. | Session logs contain redacted user-visible session data and tool data. They are local artifacts, not automatically uploaded by session persistence itself. | `crates/squeezy-skills/external-docs/SESSIONS.md:3-24`, `crates/squeezy-skills/external-docs/SESSIONS.md:25-45`, `crates/squeezy-store/src/sessions.rs:585-631`, `crates/squeezy-store/src/sessions.rs:709-722` |
| Session retention | Live session logs default to 30 days. Cleanup can soft-archive sessions first; archived sessions are recoverable until archive retention removes them, and `--purge` hard-deletes. | "Cleanup" is user-controlled and local. Do not claim server-side deletion from local cleanup. | `crates/squeezy-skills/external-docs/SESSIONS.md:104-108`, `crates/squeezy-core/src/lib.rs:496-504`, `crates/squeezy-store/src/sessions.rs:724-760` |
| Session redaction | Prompt text, tool arguments, tool outputs, approval metadata, provider errors, and assistant text pass through shared redaction before session persistence. Large events/sessions are bounded. | Redaction is pattern-based and configurable; avoid saying it catches every possible secret. | `crates/squeezy-skills/external-docs/SESSIONS.md:74-80`, `crates/squeezy-agent/src/lib.rs:15046-15223`, `crates/squeezy-core/src/lib.rs:7455-7460`, `crates/squeezy-core/src/lib.rs:7535-7788` |
| Checkpoints | Optional checkpoints capture before/after trees around mutating tools and expose list/show/undo/revert commands. Turn-level `group_id` rollback can revert multi-tool turns, and rollback uses sha256 conflict checks so later user edits are not silently clobbered. | Checkpointing is disabled by default. Files over 2 MiB are not stored in checkpoint trees. Checkpoints protect workspace files only and are an undo aid, not a substitute for Git. | `crates/squeezy-skills/external-docs/CHECKPOINTS.md:3-8`, `crates/squeezy-skills/external-docs/CHECKPOINTS.md:34-61`, `crates/squeezy-skills/external-docs/CHECKPOINTS.md:63-79`, `crates/squeezy-vcs/src/lib.rs:926-990`, `crates/squeezy-vcs/src/lib.rs:1041-1109`, `crates/squeezy-vcs/src/lib.rs:1257-1369` |
| Checkpoint privacy | It is acceptable to say checkpoint state is local. | Do not imply checkpoint storage is redacted. The docs explicitly say the journal and shadow Git object store are written before redaction and can contain the same content as workspace files until retention cleanup prunes them. | `crates/squeezy-skills/external-docs/CHECKPOINTS.md:85-87`, `crates/squeezy-vcs/src/lib.rs:858-880`, `crates/squeezy-vcs/src/lib.rs:1485-1526` |
| Tool output redaction | Tool results are redacted before final result handling and before spillover output reaches model-visible surfaces. Shell audit records hash output bytes rather than storing raw stdout/stderr. | Checkpoints are a separate pre-redaction storage path. Shell audit records include a redacted/truncated command string and metadata, not raw output. | `crates/squeezy-tools/src/lib.rs:4766-4830`, `crates/squeezy-tools/src/lib.rs:1632-1686`, `crates/squeezy-skills/external-docs/SHELL_SANDBOXING.md:203-224` |

## Caveats To Preserve

- **Windows sandbox limit:** Windows support is real for process-tree cleanup,
  but not for filesystem/network isolation. Any page that says "sandbox" should
  include a platform caveat or use "shell guardrails" instead.
- **Permission defaults are not zero-trust:** default mode is designed for
  everyday coding momentum. It allows in-workspace read/search/edit/shell/git/
  compiler and asks for web/MCP/destructive actions.
- **Auto-review is not shipped-on:** it is opt-in through
  `permissions.mode = "auto_review"` and still has hard ceilings.
- **Telemetry is default-on, not opt-in:** the privacy-positive point is the
  data minimization and opt-out path, not consent-before-every-event.
- **Reports are consented but can be content-bearing:** feedback text goes to
  PostHog after redaction; reports upload a redacted tar archive to R2 and only
  metadata to PostHog.
- **Redaction is not a legal guarantee:** say "redacted preview" and "designed
  to avoid sending..." rather than "secrets can never leak".
- **Checkpoints are not private storage:** they are local recovery artifacts and
  may contain unredacted workspace content until retention pruning.
- **Network permissions and sandbox network are separate:** a command can be
  classified/approved as network, and the sandbox posture determines whether the
  network namespace opens on supported platforms.

## Trust Story For The Website

Use a three-part story:

1. Local-first operation: Squeezy tries tree-sitter graph navigation, bounded
   reads, sessions, and local tool output shaping before spending more model
   context.
2. Reviewable execution: permissions, shell classification, optional OS
   sandboxing, audit records, and checkpoints keep agent actions visible and
   reversible where possible.
3. Clear data boundaries: automatic telemetry is aggregate and excludes prompts,
   file contents, paths, commands, URLs, and secrets; feedback/report uploads
   are separate, redacted, previewed, capped, and explicitly sent.

## Website Copy Ideas

Hero-supporting trust line:

> Local code understanding first. Reviewable edits, shell commands, web access,
> and reports when the work needs them.

Privacy section heading:

> Telemetry without prompts, files, paths, or commands

Privacy section body:

> Squeezy sends anonymous aggregate product telemetry by default so maintainers
> can see reliability, cost, and performance trends. It does not send prompts,
> model responses, file contents, file paths, repository names, shell commands,
> command output, URLs, API keys, or environment values. Opt out with
> `SQUEEZY_TELEMETRY=off`.

Feedback/report copy:

> Feedback and diagnostic reports are separate from automatic telemetry. Squeezy
> builds a local redacted preview first, shows the archive size, sections, and
> redaction count, and uploads only after you choose to send.

Permissions copy:

> File edits, shell commands, web access, MCP tools, git/compiler actions, and
> destructive operations pass through capability permissions. Default mode keeps
> routine workspace work moving while asking before web, MCP, and destructive
> actions.

Sandbox copy:

> Shell commands get layered guardrails: parser-backed command classification,
> environment allowlisting, timeouts, output caps, sensitive-path checks, audit
> records, and OS sandboxing where the host can enforce it.

Sandbox caveat copy:

> On Windows, Squeezy uses Job Objects for process-tree cleanup but does not
> provide filesystem or network isolation. Use required sandbox mode on macOS or
> Linux when fail-closed isolation matters.

Checkpoint copy:

> Optional local checkpoints capture agent edits so recent changes can be
> inspected, undone, or reverted by turn. Hash checks report conflicts instead
> of clobbering files you changed after the checkpoint.

Session copy:

> Sessions are local, resumable, exportable records of coding work. They store
> redacted events, resume state, attachments, and replay data so long tasks
> survive restarts and can be inspected later.

## Copy To Avoid

- "Fully sandboxed coding agent" - false on Windows and too strong for
  best-effort host backends.
- "No data leaves your machine" - false because product telemetry is default-on
  and providers receive model requests during normal use.
- "Private by default" - ambiguous and too broad; use specific data-boundary
  claims instead.
- "Secrets never leave your machine" - redaction is best-effort and providers
  still receive user/model context needed for agent work.
- "One-click safe rollback" - checkpoints are opt-in, have file-size limits,
  and cannot restore outside-workspace shell mutations.
- "Telemetry is opt-in" - product telemetry is default-on with opt-out.

## Page Placement Ideas

- Add a compact "Trust and control" band on the homepage after the local
  navigation/value proposition: three columns for Permissions, Telemetry, and
  Reports.
- Add a deeper "Privacy and safety" docs page with the caveats above. This page
  should link from the footer/privacy page and from the permissions docs.
- Keep legal/privacy-policy wording distinct from product copy. Product copy can
  summarize implementation behavior; the privacy page should say exactly which
  surfaces send data and where.
