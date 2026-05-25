# Shell Sandboxing

Squeezy's shell tool runs local commands behind three separate controls:

- Permission policy decides whether the command may run.
- Command analysis classifies the command's risk before approval.
- OS sandboxing limits what an approved command can do after it starts.

Sandboxing is enabled by default and is configured under
`[permissions.shell_sandbox]`. **The permission policy is the strong gate;
the OS sandbox is best-effort defense in depth.** See "Limits" below.

## What It Does

The shell sandbox hardens approved local commands. It is not a replacement for
permission prompts; it is a second boundary after a command has already passed
policy.

The current implementation:

- Fails closed when a required sandbox backend is unavailable: macOS denies if
  `sandbox-exec` isn't on disk or if the kernel refuses to apply the profile;
  Linux denies if `unprivileged_userns_clone` is `0` or `/proc/self/ns/user`
  doesn't exist.
- Uses `tree-sitter-bash` to classify shell commands before approval.
- Recursively unwraps shell wrappers (`sh -c "X"`, `bash -lc "Y"`,
  `env BAR=v cmd`, `nohup cmd`, `nice -n N cmd`, `timeout N cmd`,
  `xargs ... cmd`, `sudo cmd`, etc.) so destructive/network/compiler
  classification fires on the inner argv, not just the wrapper.
- Treats parse errors, command substitutions, shell expansions, heredocs, and
  other dynamic shell constructs conservatively (capability `Shell`,
  `dynamic = true`).
- Runs shell commands in their own process group and terminates the whole group
  on timeout or cancellation (`SIGTERM`, then `SIGKILL` after `kill_grace_ms`).
- Applies an allowlisted environment via `env_clear` + per-name preservation,
  and never returns environment values in approvals or tool results.
- Protects metadata directories (`.git`, `.squeezy`, and `.agents` by
  default) before spawn. Shell command text that writes one of those
  directories is denied even if the workspace root is otherwise writable.
- Blocks command strings that reference configured sensitive path patterns
  such as `.ssh/**`, `.aws/**`, `.netrc`, `.kube/**`, `.npmrc`, and `.env*`.
  The matcher tokenizes the command, expands `~/` and `$HOME`, and matches
  on path segments so `cat .environment` is not falsely flagged as `.env`.
- Emits redacted JSONL audit records to `.squeezy/audit/shell.jsonl` under a
  process-wide mutex with rotation at 8 MiB and up to four archived files.
- Adds `policy`, `sandbox`, `sandbox_network`, and `env` metadata to shell
  tool results.

On **macOS**, Squeezy launches shell commands through `/usr/bin/sandbox-exec`
with a `(deny default)` SBPL profile. The profile then re-allows the minimum
needed for normal builds and tests: reads under `/usr`, `/bin`, `/sbin`,
`/System`, `/Library`, `/opt`, `/private/etc`, `/dev/{null,zero,random,urandom}`,
`$CARGO_HOME`, `$RUSTUP_HOME`, `$HOME/.cargo`, and `$HOME/.rustup`; reads and
writes under the workspace root, `/tmp`, `/private/tmp`, `/private/var/folders`,
`$TMPDIR`, `$CARGO_HOME`, `$RUSTUP_HOME`, `$HOME/.cargo`, and `$HOME/.rustup`.
Sensitive paths are denied on top of the default deny, and network is denied
unless the command is classified as network and the user sets
`network = "allow_when_approved"`.

On **Linux**, Squeezy uses a direct syscall backend. The pre-spawn probe checks
`/proc/sys/kernel/unprivileged_userns_clone` and `/proc/self/ns/user`. When
namespacing is available, the spawned shell calls
`unshare(CLONE_NEWUSER | CLONE_NEWNS [| CLONE_NEWNET])`, writes
`/proc/self/{setgroups,uid_map,gid_map}` so the inner uid maps to the parent
uid, and then `execve`s the shell. On older kernels or containers without
user-namespace support, the backend reports unavailable; in `mode = "required"`
the tool call is denied pre-spawn rather than running unsandboxed, and in
`mode = "best_effort"` the command runs with the remaining shell policy
controls (env allowlist, timeout, output cap, audit) but no OS isolation.

## When To Use It

Use shell sandboxing for normal agent-driven shell execution:

- Running build and test commands such as `cargo test`, `cargo check`, or
  `cargo fmt`.
- Running project-local scripts that should only write inside the workspace.
- Letting the agent inspect local tool output without exposing shell access to
  credential files.
- Keeping accidental network calls blocked unless the command is explicitly
  classified and approved as a network command.

Keep the default `mode = "best_effort"` for everyday development. Squeezy uses
the OS sandbox when the host can apply it, and falls back to the
permission-gated direct runner when macOS or Linux refuses nested sandboxing.

Use `mode = "required"` when strict isolation is more important than command
execution. A missing or unavailable sandbox backend becomes an explicit denial.

Use `mode = "external"` when Squeezy itself is already running inside a
trusted outer sandbox. Squeezy does not apply a nested OS backend in this mode,
but it still keeps the permission policy, environment allowlist, audit trail,
timeouts, output caps, sensitive-path checks, metadata-directory checks, and
approval metadata.

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
`tree-sitter-bash`. The classifier:

- Extracts command segments and preserves quoted operators.
- Recursively unwraps known shell wrappers and analyses the inner argv so
  `sh -c "rm -rf target"`, `nohup rm -rf target`, and
  `env BAR=v rm -rf target` all classify as `destructive`.
- Detects destructive output redirects (`>`, `>>`, `>|`, `&>`, `&>>`, `<>`)
  with a quote-aware scanner that ignores file-descriptor duplications such
  as `2>&1` and `>&-`.
- Marks dynamic constructs (`$( )`, `${ }`, backticks, process substitution,
  parse errors) as high risk and forces capability `Shell`.
- Maps common command families to capabilities `compiler`, `git`, `network`,
  `destructive`, or `shell`.

Squeezy then validates local execution policy:

- The command must not be empty.
- `workdir` must canonicalize inside the workspace.
- `timeout_ms` and `output_byte_cap` must be positive and remain within global
  caps.
- Environment variables are cleared and rebuilt from the configured allowlist.
- Sensitive path patterns are checked before spawn.

If the command is allowed, Squeezy prepares a sandbox plan:

- `required`: deny if the backend cannot be used. On macOS this catches a
  missing or refused `sandbox-exec`; on Linux it catches missing
  `unprivileged_userns_clone` / `/proc/self/ns/user` or unavailable Landlock
  filesystem enforcement.
- `best_effort`: use the backend when possible and otherwise run with the
  remaining shell policy controls (env allowlist, timeout/output cap, audit).
- `off`: run directly with no OS sandbox.
- `external`: run directly because an outer sandbox is responsible for
  isolation; Squeezy still records the sandbox posture as `external`.

Filesystem roots are opt-in. The default writable set is the workspace, temp
directories, and Rust toolchain caches. Add shared project roots in committed
`squeezy.toml`, and add personal absolute paths in
`~/.squeezy/projects/<repo-id>/settings.toml`. `read_roots` are read-only;
`write_roots` allow read/write. Both lists must contain existing absolute
directories. Sensitive path patterns still deny before spawn, and macOS adds
explicit deny rules for sensitive paths inside allowed roots.
`protected_metadata_names` defaults to `.git`, `.squeezy`, and `.agents`.
Write-capable shell commands that target these names are denied at
command-analysis time and, on macOS, via explicit `require-not` write guards
under every writable root.

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
- cwd as workspace-relative when inside the workspace, otherwise the configured
  absolute shell root path,
- classification capability, target, risk, network/destructive flags, and
  parser metadata,
- sandbox backend, mode, network posture, filesystem posture, configured extra
  roots, and required flag,
- allowlisted environment variable names (values are never recorded),
- timeout and output caps,
- outcome, denial reason, and exit code,
- stdout/stderr byte counts and SHA-256 hashes.

Audit records do not include raw stdout, raw stderr, or environment values.

## Configuration

Default settings:

```toml
[permissions.shell_sandbox]
mode = "best_effort"
network = "deny_by_default"
audit = true
kill_grace_ms = 250
env_allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME", "RUSTFLAGS", "RUST_BACKTRACE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NIX_SSL_CERT_FILE", "LC_*"]
read_roots = []
write_roots = []
protected_metadata_names = [".git", ".squeezy", ".agents"]
sensitive_path_patterns = [".ssh/**", ".aws/**", ".config/gh/**", ".netrc", ".gnupg/**", ".kube/**", ".docker/config.json", ".cargo/credentials*", ".npmrc", ".pypirc", ".env*"]
# replace_sensitive_path_patterns = false  # default; user list EXTENDS the floor above.
```

`network = "deny_by_default"` keeps the network namespace closed for every
shell command, including those classified as `network`. The permission policy
can still ask the user whether to RUN the command (e.g. `curl ...`); if
approved, the command runs but cannot reach the network. The audit record
shows `sandbox.network = "denied_classified"` for that case.

`network = "allow_when_approved"` opens the network namespace **only** when
the command is classified as `network` and the permission policy allowed it.
All other commands still run with network denied. The audit record shows
`sandbox.network = "allowed_approved"` when network is opened and
`"denied"` for everything else.

`env_allowlist` patterns support exact names (e.g. `PATH`) and single
trailing wildcards (e.g. `LC_*`). Other glob shapes are rejected at config
load time.

`read_roots` and `write_roots` are empty by default. They are merged across
user, project, and per-repo user settings, canonicalized, and rejected when a
path is missing, relative, a file, duplicated across read/write roots, or
inside a sensitive path base. `write_roots` imply read access.

`protected_metadata_names` entries must be single path segments. Setting the
list to an empty array disables metadata directory protection and emits a
configuration warning.

`sensitive_path_patterns` patterns must include a literal prefix before any
wildcard. By default a user-supplied list **extends** the built-in floor —
project-specific entries cannot accidentally disable the `.ssh/**`, `.aws/**`,
`.netrc`, etc. denials. To opt out of the floor, set
`replace_sensitive_path_patterns = true` and provide the full list.

`kill_grace_ms` accepts values in the range `10..=60_000`. Out-of-range values
fail loudly at config load.

## Limits

The sandbox is intentionally local and deterministic. **It is not a substitute
for a disposable VM or container** with a separate user, filesystem, and
network stack. The permission policy (capability + target rule matching) is
the strong gate; the sandbox is best-effort defense in depth.

Known limits:

- CI covers backend selection, required-mode unavailable behavior, runtime
  unavailable detection, and platform-gated smoke execution. It does not prove
  full OS-boundary denial semantics such as blocking reads from real credential
  files, blocking writes outside the workspace, or blocking routed network
  traffic; those checks require controlled self-hosted machines. The smoke
  tests also skip themselves when the host kills the sandboxed child before
  it produces any output (signal-terminated, empty stdout/stderr, no exit
  code), since that is indistinguishable post-hoc from a third-party EDR or
  shell-intercept toolchain refusing to run under `sandbox-exec` / `unshare`.
- macOS `sandbox-exec` is deprecated by Apple, but remains the available native
  command-line sandbox backend on supported macOS systems.
- Some host sandboxes (CI runners, third-party VPN/EDR products) can prevent
  `sandbox-exec` from applying a nested profile. In `required` mode Squeezy
  treats that as a denial; in `best_effort` it falls through.
- Linux namespace setup depends on kernel build flags and the
  `unprivileged_userns_clone` sysctl. Common environments where this is
  disabled: Docker containers with the default seccomp profile, locked-down
  enterprise Linux distributions, WSL1. In `required` mode Squeezy denies
  pre-spawn; in `best_effort` the command runs without OS isolation.
- The classifier is parser-backed but conservative. Truly dynamic constructs
  (`$(...)`, `${...}`, backticks, process substitution, parse errors) always
  classify as `Shell` with risk `High`, even if the inner command would look
  safe; this is deliberate.
- Sensitive-path matching is path-segment based at the **command text** layer
  before spawn. It catches `$HOME/.ssh/id_rsa`, `~/.aws/config`, and
  `cat ./.env.production`; it does NOT inspect what the spawned process
  actually opens. For OS-level path enforcement, rely on the macOS deny rules
  or future namespace-based controls.
- `mode = "off"` removes OS isolation and should not be used for routine agent
  shell execution. Permission policy, env allowlist, timeout/output caps, and
  audit still apply.
