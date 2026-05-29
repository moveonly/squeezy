# Config Shell Escapes

`squeezy-core` resolves every TOML string value in `settings.toml` whose first
character is `!` by running the remainder through the user's shell
(`/bin/sh -c <cmd>` on Unix, `cmd.exe /C <cmd>` on Windows) at config-load
time and substituting the trimmed stdout as the value. This mirrors pi's
`resolve-config-value` flow and lets users wire in credential helpers like
`api_key = "!op read op://Personal/OpenAI/credential"` without persisting
secrets to disk. The escape only fires on strings that *start* with `!`;
values such as `prompt = "hello!"` are left intact. **The settings file is
therefore executable surface**: anything that can write to the user, project,
or per-repo `settings.toml` can run arbitrary commands as the invoking user
before the agent loop, the permission engine, or the shell sandbox come up.
Failures (non-zero exit, missing binary, non-UTF-8 stdout, empty `!`) abort
config-load with a `configuration error: <source>: <key>: shell escape …`
message so they cannot be silently downgraded to an empty secret. Treat
`settings.toml` like a shell rc-file: keep it on a trusted filesystem, audit
diffs from untrusted sources, and prefer `api_key_env` indirection or a
credential helper command over inline plaintext secrets.
