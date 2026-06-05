# Feedback and Reports

Squeezy has two consented maintainer-intake flows.

`/feedback` is for a short human-written note. The TUI accepts a few
sentences, shows the redacted preview, and only sends after `/feedback send`.
The CLI equivalent is:

```sh
squeezy feedback "what happened" --yes
```

Feedback text is redacted locally, capped by `[feedback].max_feedback_bytes`
(16 KiB by default), and sent to the configured Cloudflare Worker feedback
endpoint. The Worker stores the redacted message body as the PostHog feedback
event's `message` property.

`/report` is for diagnostic bundles. The TUI builds a local redacted archive
for the current session (or an explicit session id), previews the sections,
size, and redaction count, and only uploads after `/report send`.

```sh
squeezy sessions report <session_id> --preview
squeezy sessions report <session_id> --send --yes
squeezy sessions report <session_id> --output /tmp/report.tar
```

Report archives contain redacted version, config, repo profile, session,
tool/cost, permission, diagnostic, and replay sections. The replay section uses
the redacted shareable tape or omits it when size limits require truncation; raw
local-only traces are never included in report archives. Archives are uploaded
to private Cloudflare R2 storage; PostHog receives only report
metadata such as `report_id`, byte size, section names, platform, and redaction
count. If upload fails, Squeezy writes the archive locally instead.

Feedback and report upload can be disabled with `SQUEEZY_FEEDBACK=off`. Test or
staging collectors can override endpoints with `SQUEEZY_FEEDBACK_ENDPOINT` and
`SQUEEZY_REPORT_ENDPOINT`. Report archives are capped by
`[feedback].max_report_bytes` (2 MiB by default).

Public GitHub issues should include only a sanitized summary and the returned
`feedback_id` or `report_id`, never the archive contents by default.
