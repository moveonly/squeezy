# Troubleshooting

Use `squeezy --health` first when startup or configuration looks wrong. It
validates the merged configuration without opening the TUI and prints the source
chain that contributed settings.

## Provider Or Model Errors

Run:

```sh
squeezy config inspect
squeezy --list-providers
squeezy --list-models
```

Check that the selected provider has an API key environment variable configured
and present in the shell environment. `config inspect` redacts secret-looking
values, so it can show which key name is configured without exposing the value.

## Unexpected Permissions

Inspect `[permissions]`, `[[permissions.rules]]`, and
`[permissions.shell_sandbox]` in `squeezy config inspect`. Permission policy
decides whether an operation may start; shell sandboxing is an additional local
execution boundary for approved shell commands.

If a shell command fails only under sandboxing, compare the command's file and
network needs with configured `read_roots`, `write_roots`, and
`network = "deny_by_default"`.

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
