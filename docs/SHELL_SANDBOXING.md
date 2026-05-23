# Shell Sandboxing

Squeezy's shell tool runs local commands behind three separate controls:

- Permission policy decides whether the command may run.
- Command analysis classifies the command's risk before approval.
- OS sandboxing limits what an approved command can do after it starts.

Sandboxing is enabled by default and is configured under
`[permissions.shell_sandbox]`.

## What It Does

The shell sandbox hardens approved local commands. It is not a replacement for
permission prompts; it is a second boundary after a command has already passed
policy.

The current implementation:

- Fails closed by default when a required sandbox backend is unavailable.
- Uses `tree-sitter-bash` to classify shell commands before approval.
- Treats parse errors, command substitutions, shell expansions, heredocs, and
  other dynamic shell constructs conservatively.
- Runs shell commands in their own process group and terminates the whole group
  on timeout or cancellation.
- Applies an allowlisted environment and never returns environment values in
  approvals or tool results.
- Blocks command strings that directly reference configured sensitive path
  patterns such as `.ssh/**`, `.aws/**`, `.netrc`, `.kube/**`, `.npmrc`, and
  `.env*`.
- Emits redacted JSONL audit records to `.squeezy/audit/shell.jsonl`.
- Adds `policy`, `sandbox`, and `env` metadata to shell tool results.

On macOS, Squeezy launches shell commands through `/usr/bin/sandbox-exec`.
The generated profile denies network access for non-network commands, denies
writes outside the workspace and temp directories, and denies reads/writes to
configured sensitive paths.

On Linux, Squeezy uses a direct syscall backend. It creates a process group and
uses namespace syscalls for mount/network isolation where the kernel permits
them. If the required backend cannot be applied, `mode = "required"` denies the
tool call instead of silently running unsandboxed.

## When To Use It

Use shell sandboxing for normal agent-driven shell execution:

- Running build and test commands such as `cargo test`, `cargo check`, or
  `cargo fmt`.
- Running project-local scripts that should only write inside the workspace.
- Letting the agent inspect local tool output without exposing shell access to
  credential files.
- Keeping accidental network calls blocked unless the command is explicitly
  classified and approved as a network command.

Keep `mode = "required"` for everyday use. It is the safest default because a
missing or unavailable sandbox backend becomes an explicit denial.

Use `mode = "best_effort"` only when command execution is more important than
strict isolation, such as a development environment where an older OS or
container cannot apply the sandbox.

Use `mode = "off"` only for controlled tests, local debugging of the sandbox
itself, or environments that provide an equivalent outer sandbox. Turning it off
does not bypass Squeezy's permission policy, timeout caps, output caps, or
environment allowlist, but it removes the OS isolation boundary.

## When It Is Needed

Sandboxing matters most when the command is approved but still has risk:

- The command invokes a broad tool such as `sh`, `bash`, `make`, `npm`, or a
  project script.
- The command may execute repository-controlled code.
- The workspace is untrusted or recently fetched.
- Secrets exist in standard user locations, including SSH keys, cloud provider
  config, package-manager credentials, kube config, or `.env` files.
- Network access should be a deliberate permission event, not an incidental
  side effect of a build script.

Permission rules answer "should this command be allowed to start?" Sandboxing
answers "what can this allowed command touch once it runs?"

## How It Works

Before spawning the command, Squeezy parses the shell text with
`tree-sitter-bash`. The classifier extracts command segments, preserves quoted
operators, marks dynamic constructs as high risk, and maps common command
families to capabilities such as `compiler`, `git`, `network`, `shell`, or
`destructive`.

Squeezy then validates local execution policy:

- The command must not be empty.
- `workdir` must resolve inside the workspace.
- `timeout_ms` and `output_byte_cap` must be positive and remain within global
  caps.
- Environment variables are cleared and rebuilt from the configured allowlist.
- Sensitive path patterns are checked before spawn.

If the command is allowed, Squeezy prepares a sandbox plan:

- `required`: deny if the backend cannot be used.
- `best_effort`: use the backend when possible and otherwise run with the
  remaining shell policy controls.
- `off`: run directly with no OS sandbox.

For process cleanup, Squeezy creates a process group for the shell command. On
timeout or cancellation it sends `SIGTERM`, waits for `kill_grace_ms`, then
sends `SIGKILL` to the process group. This prevents a shell wrapper from leaving
grandchildren running after the tool call ends.

## Audit Records

When `audit = true`, each shell attempt appends one JSON object to
`.squeezy/audit/shell.jsonl`.

The audit record includes:

- timestamp (`ts_unix_ms`), call id, and tool name,
- redacted (then truncated) command string and optional redacted description,
- workspace-relative cwd (no redaction applied; cwd is a workspace path),
- classification capability, target, risk, network/destructive flags, and
  parser metadata,
- sandbox backend, mode, network posture, and required flag,
- allowlisted environment variable names (values are never recorded),
- timeout and output caps,
- outcome, denial reason, and exit code,
- stdout/stderr byte counts and SHA-256 hashes.

Audit records do not include raw stdout, raw stderr, or environment values.

## Configuration

Default settings:

```toml
[permissions.shell_sandbox]
mode = "required"
network = "deny_by_default"
audit = true
kill_grace_ms = 250
env_allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME", "RUSTFLAGS", "RUST_BACKTRACE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NIX_SSL_CERT_FILE", "LC_*"]
sensitive_path_patterns = [".ssh/**", ".aws/**", ".config/gh/**", ".netrc", ".gnupg/**", ".kube/**", ".docker/config.json", ".cargo/credentials*", ".npmrc", ".pypirc", ".env*"]
```

`network = "deny_by_default"` keeps network blocked for ordinary shell commands.
Network-looking commands still route through the `network` permission
capability before execution.

`network = "allow_when_approved"` is intended for workflows where a network
classified shell command should be able to use the network after it passes the
permission gate.

## Limits

The sandbox is intentionally local and deterministic. It does not make
untrusted code safe in the same way as a disposable VM or container with a
separate user, filesystem, and network stack.

Known limits:

- macOS `sandbox-exec` is deprecated by Apple, but remains the available native
  command-line sandbox backend on supported macOS systems.
- Some host sandboxes can prevent `sandbox-exec` from applying a nested profile;
  in `required` mode Squeezy treats that as a denial.
- Linux namespace setup depends on kernel and user-namespace policy.
- Sensitive path matching is conservative and string/path-pattern based before
  spawn; keep the default deny list and add project-specific paths when needed.
- `mode = "off"` removes OS isolation and should not be used for routine agent
  shell execution.
