# Config Shell Escapes

`squeezy-core` resolves every parsed TOML string value in Squeezy settings whose
first character is `!` by running the remainder through the user's shell
(`/bin/sh -c <cmd>` on Unix, `cmd /C <cmd>` on Windows) at config-load time and
substituting the command's trimmed stdout as the value. The escape lets users
wire in credential helpers like
`api_key = "!op read op://Personal/OpenAI/credential"` without persisting
secrets to disk.

The escape only fires on strings that *start* with `!`; values such as
`prompt = "hello!"` are left intact. It applies before config values are merged
into `AppConfig`, so user, project, and per-repo settings files are all
executable surface. Anything that can write one of those files can run arbitrary
commands as the invoking user before the agent loop, the permission engine, or
the shell sandbox comes up.

Failures abort config load instead of silently becoming empty values:

- Empty escape: `!` or whitespace after `!`.
- Shell spawn failure.
- Non-zero command exit, with trimmed stderr included in the config error.
- Non-UTF-8 stdout.

Treat Squeezy settings like a shell rc-file: keep them on a trusted filesystem,
audit diffs from untrusted sources, and prefer `api_key_env` indirection or a
credential helper command over inline plaintext secrets.
