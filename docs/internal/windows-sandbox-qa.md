# Windows shell-sandbox QA checklist

The Windows shell sandbox cannot be validated by macOS/Linux CI, and its Win32
code is cross-compiled (not runtime-tested) during development. This checklist
is the runtime acceptance gate. Run it on a real Windows host before claiming
Windows sandbox parity. The **restricted-token** tier needs no admin; the
**elevated** tier needs an Administrator account.

Referenced from `crates/squeezy-skills/external-docs/SHELL_SANDBOXING.md`.

## Packaging prerequisite

The elevated tier launches a helper binary `squeezy-sandbox-setup.exe`, which
`helper_materialization` resolves next to the running `squeezy.exe`. **The
release/dist packaging must build and ship `squeezy-sandbox-setup.exe` in the
same directory as `squeezy.exe`.** (`cargo build -p squeezy-win-sandbox
--bin squeezy-sandbox-setup` produces it; the dist job must include it.) Without
it, `squeezy doctor --sandbox-setup` fails with a "helper not found" error.

## A. Restricted-token tier (default, no admin — also covered by CI)

Config (`.squeezy/settings.toml`): `[permissions.shell_sandbox]` with
`windows_sandbox_level = "restricted_token"` (default), `mode = "required"`.

1. **Write inside workspace allowed** — a shell command that writes a file under
   the workspace root succeeds and the file exists.
2. **Write outside workspace denied** — a command writing to e.g.
   `C:\Users\Public\squeezy-escape.txt` fails; the file is NOT created.
3. **Reads unrestricted** — `type C:\Windows\win.ini` (or `dir C:\Windows`)
   succeeds (the restricted tier does not gate reads).
4. **Metadata protected** — writing under `.git` / `.squeezy` / `.agents` in the
   workspace is denied.
5. **World-writable escape closed** — create a world-writable dir
   (`icacls <dir> /grant Everyone:(F)`) outside the workspace; a sandboxed
   command cannot write into it.
6. **Posture honesty** — the `.squeezy/audit/shell.jsonl` row shows
   `backend = "windows-restricted-token"`, `filesystem = "enforced_writes_only"`,
   `network = "not_enforced"`.
7. **`mode = "required"` runs** — required mode does NOT deny pre-spawn (the
   restricted tier is an available backend).
8. **`squeezy doctor`** — the `sandbox` row reports the restricted-token backend
   as available and mentions the opt-in elevated tier.

## B. Elevated tier (opt-in, Administrator host)

1. **Setup** — `squeezy doctor --sandbox-setup` shows a UAC prompt; on accept it
   reports success. Verify:
   - `net user` lists `SqueezySandboxOffline` and `SqueezySandboxOnline`.
   - Both are hidden from the login screen (registry
     `HKLM\…\Winlogon\SpecialAccounts\UserList` has DWORD `0` entries).
   - WFP filters exist: `netsh wfp show filters` (or `show state`) lists the
     Squeezy provider/sublayer with 12 block filters scoped to the offline SID.
2. Set `windows_sandbox_level = "elevated"`.
3. **Network blocked when denied** — with network NOT approved, a command like
   `curl https://example.com` / `Invoke-WebRequest` fails to connect (runs as the
   offline user; WFP blocks DNS/egress). Audit row: `network = "enforced"`.
4. **Network allowed when approved** — approve a network command; it now reaches
   the network (runs as the online user, no WFP filters).
5. **Full read isolation** — reading the real user's `~/.ssh/id_*`, `~/.aws/…`,
   or other profile files is denied (the sandbox user has no access).
6. **Write isolation** — writes outside the granted roots are denied; writes in
   the workspace succeed. Audit row: `filesystem = "enforced"`.
7. **`mode = "required"` before setup** — with `windows_sandbox_level =
   "elevated"` but setup NOT run, `mode = "required"` denies pre-spawn with a
   message pointing to `--sandbox-setup`; `best_effort` falls back to the
   restricted-token tier.
8. **Teardown** — `squeezy doctor --sandbox-teardown` removes everything: `net
   user` no longer lists the sandbox users, `netsh wfp show filters` no longer
   shows the Squeezy provider, and the registry hide entries are gone.

## C. Multi-user / reboot notes

- Elevated state (users, WFP filters) is machine-global; the marker tracking it
  lives in the invoking user's `%LOCALAPPDATA%\squeezy\win-sandbox`. On a shared
  machine, run setup/teardown as the same OS user. WFP filters and users persist
  across reboot until `--sandbox-teardown`.
