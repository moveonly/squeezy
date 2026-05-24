# Feedback and Reports

Squeezy has two consented maintainer-intake flows.

`/feedback` is for a short human-written note. The TUI accepts a few
sentences, shows the redacted preview, and only sends after `/feedback send`.
The CLI equivalent is:

```sh
squeezy feedback "what happened" --yes
```

Feedback text is redacted locally, capped by `[feedback].max_feedback_bytes`,
and sent to the configured Cloudflare Worker feedback endpoint. The Worker
stores the redacted text as a PostHog feedback event.

`/report` is for diagnostic bundles. The TUI builds a local redacted archive
for the current session (or an explicit session id), previews the sections,
size, and redaction count, and only uploads after `/report send`.

```sh
squeezy sessions report <session_id> --preview
squeezy sessions report <session_id> --send --yes
squeezy sessions report <session_id> --output /tmp/report.tar
```

Report archives contain redacted version, config, repo profile, session,
tool/cost, permission, diagnostic, and replay-pointer sections. Archives are
uploaded to private Cloudflare R2 storage; PostHog receives only report
metadata such as `report_id`, byte size, section names, platform, and redaction
count. If upload fails, Squeezy writes the archive locally instead.

Public GitHub issues should include only a sanitized summary and the returned
`feedback_id` or `report_id`, never the archive contents by default.
