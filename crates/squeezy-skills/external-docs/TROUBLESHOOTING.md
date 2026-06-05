# Troubleshooting

Use `squeezy doctor` first when startup or configuration looks wrong. It
validates configuration without opening the TUI, then runs a small set of
checks (config sources, repo profile, configured provider credential,
session store writeability, shell-sandbox tool availability) and reports
each as `ok`, `warn`, or `fail`. Pass `--json` for machine-readable
output suitable for scripts and CI smoke tests. Exit code is `0` on
success (including warnings) and non-zero on a hard failure such as a
broken config or an unwriteable session store.

## Provider Or Model Errors

Run:

```sh
squeezy config inspect
squeezy providers list --configured
squeezy providers info <provider>
squeezy auth status
```

Check that the selected provider has an API key environment variable configured
or a supported local credential source. `config inspect` redacts
secret-looking values, so it can show which key name is configured without
exposing the value. Use `squeezy refresh-models --provider <provider>` when an
OpenAI-compatible provider's live catalog changed and the startup picker still
shows an older cached list.

## Unexpected Permissions

Inspect `[permissions]`, `[[permissions.rules]]`, and
`[permissions.shell_sandbox]` in `squeezy config inspect`. Permission policy
decides whether an operation may start; shell sandboxing is an additional local
execution boundary for approved shell commands.

With no explicit `[permissions]` overrides, `permissions.mode = "default"`
allows workspace read/search/edit plus local shell, git, and compiler commands,
while web, MCP, destructive actions, and outside-workspace file paths still ask
the human. `permissions.mode = "auto_review"` is opt-in and routes eligible
permission prompts through the AI reviewer. In `default` and `auto_review`, the
shell sandbox network posture is
`network = "allow_when_approved"` unless explicitly configured.

If a shell command fails only under sandboxing, compare the command's file and
network needs with configured `read_roots`, `write_roots`, and
`network = "allow_when_approved"` or `network = "deny_by_default"`.

## Help Does Not Know A Topic

Built-in help is intentionally closed-corpus. If `/help <topic>` says the topic
is not covered, use another listed local topic, check the public docs website,
or ask Squeezy to search the public repository when web tools are enabled.

## Graph Or Language Results Look Missing

Check [Language Coverage](LANGUAGES.md). Unsupported languages and excluded
files fall back to bounded search/read tools. Generated, vendored, dependency,
binary, lockfile, large, and hidden files may be excluded from graph indexing by
default unless project configuration includes them.

For Rust compiler-derived facts, run the `refresh_compiler_facts` tool or a
normal explicit Cargo verification command. Navigation tools do not run Cargo
implicitly.

## Session Or Report Issues

Use:

```sh
squeezy sessions list
squeezy sessions show <session_id>
squeezy sessions export <session_id>
squeezy sessions report <session_id> --preview
```

Reports are previewed and redacted before sending. If upload fails, Squeezy
writes the archive locally instead.

## Turn Routing

If the router sends an easy turn to the wrong model tier, toggle it off for the
session with `/router off`, or disable `[routing].llm_judge` to skip the model
classifier and use the static heuristic only. Use `squeezy config inspect` to
see the configured `cheap_model`, `judge_model`, and `expensive_models` regex
for the active provider.

## TUI Theme Or Display Issues

If the TUI looks wrong on your terminal, try `/theme default` or `/theme bright`
to switch to a different built-in theme. Custom themes require valid 6-digit hex
color values. `[tui.tick_rate_ms]` controls the TUI poll interval; raise it on
slow terminals.
