use std::{env, fmt::Write as _, fs, path::PathBuf, time::Duration};

use clap::Args;
use serde_json::json;
use squeezy_core::{
    AppConfig, McpServerConfig, McpTransport, ProviderConfig, ProviderSettings, Result,
    SettingsFile, default_settings_path,
};
use squeezy_llm::{
    KeySource, fallback_env_var, github_copilot_auth_file_path, resolve_api_key_with_inline,
};
use squeezy_store::{
    SessionStore, SqueezyStore, cache_diagnostics, ensure_repo_profile, prune_cache_backups,
};
use squeezy_tools::{McpClientRegistry, McpServerStatus, McpStaleOutcome};
use tokio_util::sync::CancellationToken;

use crate::update::{self, UpdateStatus};

const STATE_CACHE_WARN_BYTES: u64 = 128 * 1024 * 1024;
const GRAPH_CACHE_WARN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
    /// Probe live connectivity: issue a tiny request to the configured
    /// provider (confirming auth + base_url) and run the MCP `initialize`
    /// handshake against every enabled MCP server (confirming each one starts
    /// and advertises tools). Opt-in because it touches the network, may
    /// consume a handful of provider tokens, and spawns the configured stdio
    /// MCP `command`s as child processes.
    #[arg(long)]
    pub probe: bool,
    /// Remove rotated redb schema backups after reporting cache health.
    #[arg(long)]
    pub prune_cache: bool,
    /// Skip the repo-profile load. Useful for post-install smoke tests and
    /// Linux package CI that do not have a full repository to scan, or for
    /// fast binary-health checks where repo latency is undesirable.
    #[arg(long)]
    pub no_repo_profile: bool,
    /// Skip the update-availability check. Useful in network-isolated CI
    /// environments or when only local health checks are needed.
    #[arg(long)]
    pub no_update_check: bool,
    /// Windows only: provision the elevated shell-sandbox tier (one-time, UAC
    /// prompt). Creates the hidden local sandbox users and installs the WFP
    /// network egress-block filters, enabling `windows_sandbox_level =
    /// "elevated"`. Performs the action and exits without running other checks.
    #[arg(long)]
    pub sandbox_setup: bool,
    /// Windows only: remove all elevated shell-sandbox machine state (sandbox
    /// users, WFP filters, registry entries, secrets). Performs the action and
    /// exits without running other checks.
    #[arg(long)]
    pub sandbox_teardown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug)]
struct Check {
    name: String,
    status: Status,
    detail: String,
    /// Optional structured metadata included in `--json` output. Used by the
    /// sandbox row to expose machine-readable platform fields (`backend`,
    /// `userns`, `landlock`, `required_mode_supported`) without scraping prose.
    extra: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub exit_code: i32,
    checks: Vec<Check>,
    version: &'static str,
    target: &'static str,
    json: bool,
}

impl DoctorReport {
    pub fn print(&self) {
        let (warnings, failures) = check_counts(&self.checks);
        if self.json {
            let body = json!({
                "version": self.version,
                "target": self.target,
                "ok": failures == 0,
                "warnings": warnings,
                "failures": failures,
                "checks": self.checks.iter().map(|c| {
                    let mut obj = json!({
                        "name": c.name,
                        "status": c.status.as_str(),
                        "detail": c.detail,
                    });
                    if let (Some(extra), Some(map)) = (c.extra.as_ref(), obj.as_object_mut())
                        && let Some(extra_map) = extra.as_object()
                    {
                        // Skip keys already present in the base object so that
                        // extra metadata can never silently overwrite "name",
                        // "status", or "detail".
                        for (k, v) in extra_map {
                            if !matches!(k.as_str(), "name" | "status" | "detail") {
                                map.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    obj
                }).collect::<Vec<_>>(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            return;
        }
        let header = if self.exit_code != 0 {
            "squeezy: fail"
        } else if warnings > 0 {
            "squeezy: ok (warnings)"
        } else {
            "squeezy: ok"
        };
        println!("{header}");
        println!("version={} target={}", self.version, self.target);
        let name_width = self
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(0)
            .max(4);
        for check in &self.checks {
            println!(
                "  [{}] {:<name_width$}  {}",
                check.status.as_str(),
                check.name,
                check.detail,
                name_width = name_width,
            );
        }
    }
}

fn check_counts(checks: &[Check]) -> (usize, usize) {
    checks
        .iter()
        .fold((0, 0), |(warnings, failures), check| match check.status {
            Status::Warn => (warnings + 1, failures),
            Status::Fail => (warnings, failures + 1),
            Status::Ok => (warnings, failures),
        })
}

pub async fn run(args: &DoctorArgs) -> Result<DoctorReport> {
    let version = env!("CARGO_PKG_VERSION");
    let target = env!("SQUEEZY_TARGET_TRIPLE");

    // `--sandbox-setup` / `--sandbox-teardown` are actions, not diagnostics:
    // perform the one requested and report just its result.
    if args.sandbox_setup || args.sandbox_teardown {
        let check = if args.sandbox_teardown {
            sandbox_teardown_action()
        } else {
            sandbox_setup_action(AppConfig::from_env_and_settings().ok().as_ref())
        };
        let exit_code = if matches!(check.status, Status::Fail) {
            1
        } else {
            0
        };
        return Ok(DoctorReport {
            exit_code,
            checks: vec![check],
            version,
            target,
            json: args.json,
        });
    }

    let mut checks = Vec::new();

    let config = match AppConfig::from_env_and_settings() {
        Ok(config) => {
            let labels = config.config_source_labels();
            checks.push(Check {
                name: "config".to_string(),
                status: Status::Ok,
                detail: format!("sources: {}", labels.join(", ")),
                extra: None,
            });
            Some(config)
        }
        Err(error) => {
            checks.push(Check {
                name: "config".to_string(),
                status: Status::Fail,
                detail: format!("{error}"),
                extra: None,
            });
            None
        }
    };

    if let Some(config) = config.as_ref() {
        if args.no_repo_profile {
            checks.push(Check {
                name: "repo_profile".to_string(),
                status: Status::Ok,
                detail: "skipped (--no-repo-profile)".to_string(),
                extra: None,
            });
        } else {
            match ensure_repo_profile(&config.workspace_root, &config.graph) {
                Ok(loaded) => checks.push(Check {
                    name: "repo_profile".to_string(),
                    status: Status::Ok,
                    detail: format!(
                        "status={} languages={}",
                        loaded.status.as_str(),
                        loaded.profile.languages.len()
                    ),
                    extra: None,
                }),
                Err(error) => checks.push(Check {
                    name: "repo_profile".to_string(),
                    status: Status::Warn,
                    detail: format!("{error}"),
                    extra: None,
                }),
            }
        }

        let (provider_name, provider_check) = provider_credential_check(&config.provider);
        checks.push(Check {
            name: format!("provider:{provider_name}"),
            status: provider_check.0,
            detail: provider_check.1,
            extra: None,
        });

        checks.push(providers_check(&load_user_settings()));

        if args.probe {
            let (status, detail) = probe_provider(&config.provider).await;
            checks.push(Check {
                name: format!("probe:{provider_name}"),
                status,
                detail,
                extra: None,
            });
        }

        checks.push(mcp_check(&config.mcp_servers));
        if args.probe && config.mcp_servers.values().any(|server| server.enabled) {
            checks.extend(probe_mcp_servers(&config.mcp_servers).await);
        }
        checks.push(skills_check(config));
        checks.push(session_store_check(config));
        checks.push(state_store_check(config));
        checks.push(cache_check(config, args.prune_cache));
    }

    checks.push(sandbox_check());
    if args.no_update_check {
        checks.push(Check {
            name: "update".to_string(),
            status: Status::Ok,
            detail: "skipped (--no-update-check)".to_string(),
            extra: None,
        });
    } else {
        checks.push(update_check(update::check_for_update().await));
    }

    // Warnings (e.g. missing optional API keys, missing sandbox tool) print as
    // such but do not fail the command: smoke tests in CI / brew test run in
    // environments where keys are absent and still need the binary to come up
    // green. Only hard failures (config load broken, session store unwritable)
    // produce a non-zero exit, matching the old `--health` contract.
    let (_, failures) = check_counts(&checks);
    let exit_code = if failures > 0 { 1 } else { 0 };

    Ok(DoctorReport {
        exit_code,
        checks,
        version,
        target,
        json: args.json,
    })
}

fn provider_credential_check(provider: &ProviderConfig) -> (&'static str, (Status, String)) {
    match provider {
        ProviderConfig::OpenAi(c) => (
            "openai",
            credential_check(c.api_key.as_deref(), &c.api_key_env),
        ),
        ProviderConfig::Anthropic(c) => (
            "anthropic",
            credential_check(c.api_key.as_deref(), &c.api_key_env),
        ),
        ProviderConfig::Google(c) => (
            "google",
            credential_check(c.api_key.as_deref(), &c.api_key_env),
        ),
        ProviderConfig::AzureOpenAi(c) => (
            "azure_openai",
            credential_check(c.api_key.as_deref(), &c.api_key_env),
        ),
        ProviderConfig::Bedrock(c) => (
            "bedrock",
            (
                Status::Ok,
                format!("region={} (uses AWS credential chain)", c.region),
            ),
        ),
        ProviderConfig::Ollama(c) => (
            "ollama",
            (
                Status::Ok,
                format!("base_url={} (no API key required)", c.base_url),
            ),
        ),
        ProviderConfig::OpenAiCodex(_) => ("openai_codex", openai_codex_auth_check()),
        ProviderConfig::GitHubCopilot(_) => ("github_copilot", github_copilot_auth_check()),
        ProviderConfig::OpenAiCompatible(c) => (
            c.preset.as_str(),
            credential_check(c.api_key.as_deref(), &c.api_key_env),
        ),
        ProviderConfig::Faux(_) => (
            "faux",
            (
                Status::Ok,
                "in-process scripted provider (no credential required)".to_string(),
            ),
        ),
    }
}

/// Report whether the OAuth token file for the ChatGPT Codex provider
/// exists. Doctor does not load or decode the token here — it only
/// notes presence so the user knows whether to run `squeezy auth
/// openai-codex login`.
fn openai_codex_auth_check() -> (Status, String) {
    let Some(home) = dirs::home_dir() else {
        return (
            Status::Warn,
            "could not determine home directory; \
             run `squeezy auth openai-codex login` to authenticate"
                .to_string(),
        );
    };
    let path = home.join(".squeezy").join("auth").join("openai-codex.json");
    if path.exists() {
        (Status::Ok, format!("token present at {}", path.display()))
    } else {
        (
            Status::Warn,
            format!(
                "no token at {}; run `squeezy auth openai-codex login` to authenticate",
                path.display()
            ),
        )
    }
}

fn github_copilot_auth_check() -> (Status, String) {
    let Some(path) = github_copilot_auth_file_path() else {
        return (
            Status::Warn,
            "could not determine auth file path; \
             run `squeezy auth github-copilot login` to authenticate"
                .to_string(),
        );
    };
    if path.exists() {
        (Status::Ok, format!("token present at {}", path.display()))
    } else {
        (
            Status::Warn,
            format!(
                "no token at {}; run `squeezy auth github-copilot login` to authenticate",
                path.display()
            ),
        )
    }
}

/// Resolve the active provider's credential through the same chain the
/// runtime uses (`resolve_api_key_with_inline`: inline TOML key,
/// `credentials.json`, `api_key_env`, the conventional fallback env var,
/// then `SQUEEZY_CREDENTIALS_JSON`) so doctor agrees with what an actual
/// session would find. Reporting only `env::var(api_key_env)` warned on
/// perfectly working inline-key, `credentials.json`, and fallback-env
/// (e.g. `OPENAI_API_KEY`) setups.
fn credential_check(inline: Option<&str>, env_name: &str) -> (Status, String) {
    match resolve_api_key_with_inline(inline, env_name) {
        Ok(resolved) => (
            Status::Ok,
            format!(
                "resolved via {}",
                key_source_label(resolved.source, env_name)
            ),
        ),
        Err(_) => (
            Status::Warn,
            format!(
                "{env_name} not set; export it, set [providers.<name>] api_key = \"…\" in \
                 ~/.squeezy/settings.toml, or run `squeezy auth set <provider>`"
            ),
        ),
    }
}

/// Human-readable name for where a resolved key came from, mirroring the
/// resolution chain so the user knows which source doctor honored.
fn key_source_label(source: KeySource, env_name: &str) -> String {
    match source {
        KeySource::Inline => "inline [providers.<name>] api_key".to_string(),
        KeySource::File => "credentials.json".to_string(),
        KeySource::Env => format!("{env_name} env var"),
        KeySource::FallbackEnv => fallback_env_var(env_name)
            .map(|name| format!("{name} env var"))
            .unwrap_or_else(|| "fallback env var".to_string()),
        KeySource::JsonEnv => "SQUEEZY_CREDENTIALS_JSON".to_string(),
    }
}

fn session_store_check(config: &AppConfig) -> Check {
    let store = SessionStore::open(config);
    let root = store.root().to_path_buf();
    match probe_writable(&root) {
        Ok(()) => Check {
            name: "session_store".to_string(),
            status: Status::Ok,
            detail: format!("writable: {}", root.display()),
            extra: None,
        },
        Err(error) => Check {
            name: "session_store".to_string(),
            status: Status::Fail,
            detail: format!("{}: {error}", root.display()),
            extra: None,
        },
    }
}

fn probe_writable(root: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(root)?;
    let probe = root.join(".squeezy-doctor-probe");
    fs::write(&probe, b"ok")?;
    fs::remove_file(&probe)
}

/// Open the redb-backed `state.redb` store and report whether it loads cleanly.
/// Fail is reserved for hard errors (corrupt file, permission denied); a
/// successful open after `SqueezyStore` migrated from an older schema is still
/// reported as `ok` because the store handles that internally.
fn state_store_check(config: &AppConfig) -> Check {
    match SqueezyStore::open(&config.workspace_root, config.cache.root.as_deref()) {
        Ok(store) => {
            let path = store.path().display().to_string();
            Check {
                name: "state_store".to_string(),
                status: Status::Ok,
                detail: format!("opened: {path}"),
                extra: None,
            }
        }
        Err(error) => Check {
            name: "state_store".to_string(),
            status: Status::Fail,
            detail: format!("{error}"),
            extra: None,
        },
    }
}

fn cache_check(config: &AppConfig, prune: bool) -> Check {
    let diagnostics = match cache_diagnostics(&config.workspace_root, config.cache.root.as_deref())
    {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            return Check {
                name: "cache".to_string(),
                status: Status::Fail,
                detail: format!("{error}"),
                extra: None,
            };
        }
    };
    let mut status = Status::Ok;
    let mut detail = format!(
        "state={} graph={} backups={} ({}) at {}",
        format_bytes(diagnostics.state.size_bytes),
        format_bytes(diagnostics.graph.size_bytes),
        diagnostics.backups.len(),
        format_bytes(diagnostics.backup_total_bytes),
        diagnostics.cache_dir.display(),
    );
    if diagnostics.state.size_bytes > STATE_CACHE_WARN_BYTES {
        status = Status::Warn;
        detail.push_str("; state.redb is unusually large");
    }
    if diagnostics.graph.size_bytes > GRAPH_CACHE_WARN_BYTES {
        status = Status::Warn;
        detail.push_str("; graph.redb is large but lazy-loaded");
    }
    if diagnostics.backup_total_bytes > 0 && !prune {
        status = Status::Warn;
        detail.push_str("; run `squeezy doctor --prune-cache` to remove backups");
    }
    if prune {
        match prune_cache_backups(&config.workspace_root, config.cache.root.as_deref()) {
            Ok(report) => {
                detail.push_str(&format!(
                    "; pruned {} backups ({})",
                    report.removed_files.len(),
                    format_bytes(report.removed_bytes)
                ));
                if diagnostics.state.size_bytes <= STATE_CACHE_WARN_BYTES
                    && diagnostics.graph.size_bytes <= GRAPH_CACHE_WARN_BYTES
                {
                    status = Status::Ok;
                }
            }
            Err(error) => {
                status = Status::Fail;
                detail.push_str(&format!("; prune failed: {error}"));
            }
        }
    }
    Check {
        name: "cache".to_string(),
        status,
        detail,
        extra: None,
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Best-effort load of the user's `settings.toml`. Doctor still works when the
/// file is absent (returns an empty `SettingsFile`); parse errors collapse to
/// the same empty value because the existing `config` row already surfaced the
/// real diagnostic.
fn load_user_settings() -> SettingsFile {
    SettingsFile::load_optional(&default_settings_path()).unwrap_or_default()
}

/// Summarize `[providers.*]` blocks in the user's settings: for each section,
/// say whether it looks usable (`configured`) or is missing its API key
/// (`missing api_key`). Providers that don't take a key (`bedrock`, `ollama`)
/// are flagged `keyless`. Empty `[providers]` is reported as `ok` with a note;
/// the active provider already gets its own `provider:<name>` row.
fn providers_check(settings: &SettingsFile) -> Check {
    let Some(providers) = settings.providers.as_ref().filter(|map| !map.is_empty()) else {
        return Check {
            name: "providers".to_string(),
            status: Status::Ok,
            detail: "no [providers.*] sections in settings.toml".to_string(),
            extra: None,
        };
    };
    let mut detail = String::new();
    let mut missing = 0usize;
    for (name, settings) in providers {
        let state = provider_settings_state(name, settings);
        if state.starts_with("missing") {
            missing += 1;
        }
        if !detail.is_empty() {
            detail.push_str(", ");
        }
        let _ = write!(detail, "{name}={state}");
    }
    let status = if missing > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    Check {
        name: "providers".to_string(),
        status,
        detail,
        extra: None,
    }
}

fn provider_settings_state(name: &str, settings: &ProviderSettings) -> &'static str {
    if matches!(name, "bedrock" | "ollama") {
        return "keyless";
    }
    if settings
        .api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return "configured";
    }
    if let Some(env_name) = settings.api_key_env.as_deref()
        && env::var(env_name).is_ok_and(|value| !value.trim().is_empty())
    {
        return "configured";
    }
    "missing api_key"
}

/// Summarize configured MCP servers without touching the network: count
/// enabled/disabled servers and verify that each enabled entry has the field
/// its transport needs (`command` for stdio, `url` for http/sse). For stdio
/// servers the first token of `command` is also resolved against `PATH` to
/// catch common Linux packaging failures (binary absent from PATH, missing
/// executable bit) before the user reaches `--probe`. Missing fields or an
/// unresolvable command downgrade the row to `warn`.
fn mcp_check(servers: &std::collections::BTreeMap<String, McpServerConfig>) -> Check {
    if servers.is_empty() {
        return Check {
            name: "mcp".to_string(),
            status: Status::Ok,
            detail: "no MCP servers configured".to_string(),
            extra: None,
        };
    }
    let mut enabled = 0usize;
    let mut disabled = 0usize;
    let mut issues = String::new();
    for (name, server) in servers {
        if !server.enabled {
            disabled += 1;
            continue;
        }
        enabled += 1;
        match server.transport {
            McpTransport::Stdio => {
                let cmd = server.command.as_deref().map(str::trim).unwrap_or("");
                if cmd.is_empty() {
                    if !issues.is_empty() {
                        issues.push_str(", ");
                    }
                    let _ = write!(issues, "{name}: stdio transport without command");
                } else if let Some(issue) = mcp_stdio_command_issue(cmd) {
                    if !issues.is_empty() {
                        issues.push_str(", ");
                    }
                    let _ = write!(issues, "{name}: {issue}");
                }
            }
            McpTransport::Http | McpTransport::Sse => {
                if server
                    .url
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    if !issues.is_empty() {
                        issues.push_str(", ");
                    }
                    let _ = write!(
                        issues,
                        "{name}: {} transport without url",
                        server.transport.as_str()
                    );
                }
            }
        }
    }
    let summary = format!("enabled={enabled} disabled={disabled}");
    if issues.is_empty() {
        Check {
            name: "mcp".to_string(),
            status: Status::Ok,
            detail: summary,
            extra: None,
        }
    } else {
        Check {
            name: "mcp".to_string(),
            status: Status::Warn,
            detail: format!("{summary}; {issues}"),
            extra: None,
        }
    }
}

/// Offline check for a stdio MCP server command: resolve the first token
/// against `PATH` and verify the file exists and has the execute bit set.
/// Returns `None` when the command looks reachable, or a short warning
/// string when a common packaging failure is detected.
fn mcp_stdio_command_issue(command: &str) -> Option<String> {
    // Extract the binary name/path (first whitespace-delimited token).
    let binary = command.split_whitespace().next()?;
    let path = std::path::Path::new(binary);

    // If the user specified an absolute or relative path, check it directly.
    if path.is_absolute() || binary.contains(std::path::MAIN_SEPARATOR) {
        return mcp_stdio_path_issue(path);
    }

    // Otherwise walk PATH looking for the binary.
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return mcp_stdio_path_issue(&candidate);
        }
    }
    Some(format!(
        "stdio command '{binary}' not found on PATH; \
         install the package or check PATH"
    ))
}

/// Given a resolved path, check whether it has the execute bit set (Unix) or
/// simply exists (Windows). Returns a warning string on problems, `None` on ok.
fn mcp_stdio_path_issue(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) if meta.permissions().mode() & 0o111 != 0 => None,
            Ok(_) => Some(format!(
                "stdio command '{}' exists but is not executable (missing execute bit)",
                path.display()
            )),
            Err(err) => Some(format!(
                "stdio command '{}' cannot be stat'd: {err}",
                path.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        if path.exists() {
            None
        } else {
            Some(format!("stdio command '{}' does not exist", path.display()))
        }
    }
}

/// Live MCP reachability probe (opt-in via `--probe`; complements the
/// offline `mcp_check` config row). Builds a throwaway client registry from
/// the configured servers and drives the same timeout-bounded `initialize` +
/// tool-discovery handshake a real session uses (`refresh_tools`), so a
/// configured-but-broken server is caught here instead of at first tool call.
/// Emits one `probe:mcp:<name>` row per enabled server: `Ready` → ok (with the
/// advertised tool count), `Failed` → fail (with the handshake error), and a
/// cancelled/incomplete handshake → warn. Disabled servers are skipped. The
/// registry is shut down afterward so any stdio child processes spawned for the
/// handshake are terminated.
async fn probe_mcp_servers(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> Vec<Check> {
    let registry = McpClientRegistry::new(servers.clone());
    let outcome = registry.refresh_tools(CancellationToken::new()).await;
    registry.shutdown().await;
    outcome
        .status
        .per_server
        .iter()
        .map(|(name, server_status)| {
            let (status, detail) = match server_status {
                McpServerStatus::Ready { tools_count, .. } => (
                    Status::Ok,
                    format!("handshake ok; {tools_count} tools advertised"),
                ),
                McpServerStatus::Stale {
                    tools_count,
                    outcome,
                } => (
                    Status::Warn,
                    format!(
                        "handshake stale; serving {tools_count} cached tools after {}",
                        mcp_stale_outcome_detail(outcome)
                    ),
                ),
                McpServerStatus::Failed { error } => {
                    (Status::Fail, format!("handshake failed: {error}"))
                }
                McpServerStatus::Cancelled => (
                    Status::Warn,
                    "handshake timed out or was cancelled".to_string(),
                ),
                McpServerStatus::Starting => {
                    (Status::Warn, "handshake did not complete".to_string())
                }
            };
            Check {
                name: format!("probe:mcp:{name}"),
                status,
                detail,
                extra: None,
            }
        })
        .collect()
}

fn mcp_stale_outcome_detail(outcome: &McpStaleOutcome) -> String {
    match outcome {
        McpStaleOutcome::Failed { error } => format!("discovery failed: {error}"),
        McpStaleOutcome::Cancelled => "discovery was cancelled".to_string(),
    }
}

/// Summarize the discovered skill catalog without doing any network or
/// long-running work: walks the configured roots, counts total /
/// enabled / disabled skills, and downgrades to `warn` when a
/// same-precedence name collision flips trigger activation into
/// ambiguous mode. Pure stat work so the row stays fast and matches
/// the rest of `doctor`'s offline-CI contract.
fn skills_check(config: &AppConfig) -> Check {
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);
    let summaries = catalog.summaries();
    if summaries.is_empty() {
        return Check {
            name: "skills".to_string(),
            status: Status::Ok,
            detail: "no skills discovered".to_string(),
            extra: None,
        };
    }
    let disabled = summaries.iter().filter(|s| s.disabled).count();
    let enabled = summaries.len() - disabled;
    let ambiguous = catalog.ambiguous_names().len();
    let mut detail = format!("enabled={enabled} disabled={disabled}");
    if ambiguous > 0 {
        let names = catalog
            .ambiguous_names()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        detail.push_str(&format!("; ambiguous={ambiguous} ({names})"));
        return Check {
            name: "skills".to_string(),
            status: Status::Warn,
            detail,
            extra: None,
        };
    }
    if config.skills.hooks_enabled {
        detail.push_str("; hooks_enabled");
    }
    Check {
        name: "skills".to_string(),
        status: Status::Ok,
        detail,
        extra: None,
    }
}

/// Pull the result of `update::check_for_update()` into a doctor row. Newer
/// releases warn (so the user actually sees the nudge in CI smoke runs);
/// up-to-date and offline / disabled checks stay `ok` because we don't want a
/// network-isolated CI to mark the doctor red on principle.
fn update_check(status: UpdateStatus) -> Check {
    let row_status = if status.is_warning() {
        Status::Warn
    } else {
        Status::Ok
    };
    Check {
        name: "update".to_string(),
        status: row_status,
        detail: status.doctor_detail(),
        extra: None,
    }
}

/// Report the active shell-sandbox backend. Delegates to
/// `squeezy_tools::shell_sandbox_doctor`, the single source of truth shared
/// with the runtime — so this row reflects the backend the sandbox actually
/// uses (e.g. Linux `linux-direct-syscalls`, not the long-stale `bwrap` proxy),
/// and the Windows restricted-token / elevated tiers.
///
/// In `--json` mode the row includes structured fields (`backend`, `userns`,
/// `landlock`, `required_mode_supported`) so distro/package smoke tests can
/// gate on Linux readiness without scraping prose.
fn sandbox_check() -> Check {
    let report = squeezy_tools::shell_sandbox_doctor();
    let extra = {
        let mut map = serde_json::Map::new();
        map.insert(
            "backend".to_string(),
            serde_json::Value::String(report.backend.to_string()),
        );
        map.insert(
            "required_mode_supported".to_string(),
            serde_json::Value::Bool(report.available),
        );
        if let Some(userns) = report.userns {
            map.insert("userns".to_string(), serde_json::Value::Bool(userns));
        }
        if let Some(landlock) = report.landlock {
            map.insert("landlock".to_string(), serde_json::Value::Bool(landlock));
        }
        if let Some(reason) = report.fallback_reason {
            map.insert(
                "fallback_reason".to_string(),
                serde_json::Value::String(reason),
            );
        }
        serde_json::Value::Object(map)
    };
    Check {
        name: "sandbox".to_string(),
        status: if report.available {
            Status::Ok
        } else {
            Status::Warn
        },
        detail: format!("backend {}: {}", report.backend, report.detail),
        extra: Some(extra),
    }
}

/// `doctor --sandbox-setup`: provision the Windows elevated shell-sandbox tier.
fn sandbox_setup_action(config: Option<&AppConfig>) -> Check {
    let Some(config) = config else {
        return Check {
            name: "sandbox-setup".to_string(),
            status: Status::Fail,
            detail: "could not load configuration; cannot provision the sandbox".to_string(),
            extra: None,
        };
    };
    match squeezy_tools::windows_sandbox_setup(
        &config.permissions.shell_sandbox,
        &config.workspace_root,
    ) {
        Ok(detail) => Check {
            name: "sandbox-setup".to_string(),
            status: Status::Ok,
            detail,
            extra: None,
        },
        Err(detail) => Check {
            name: "sandbox-setup".to_string(),
            status: Status::Fail,
            detail,
            extra: None,
        },
    }
}

/// `doctor --sandbox-teardown`: remove the Windows elevated-tier machine state.
fn sandbox_teardown_action() -> Check {
    match squeezy_tools::windows_sandbox_teardown() {
        Ok(detail) => Check {
            name: "sandbox-teardown".to_string(),
            status: Status::Ok,
            detail,
            extra: None,
        },
        Err(detail) => Check {
            name: "sandbox-teardown".to_string(),
            status: Status::Fail,
            detail,
            extra: None,
        },
    }
}

/// Live probe of the configured provider. Returns `(Status, detail)` so the
/// caller can shove the result into a doctor `Check` row. The probe is
/// intentionally cheap — most providers expose a `GET /models` listing that
/// doesn't count against token budgets. Bedrock has no equivalent in the
/// runtime crate we depend on, so it reports a warn rather than fail.
pub(crate) async fn probe_provider(provider: &ProviderConfig) -> (Status, String) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(err) => return (Status::Warn, format!("could not build http client: {err}")),
    };
    match provider {
        ProviderConfig::OpenAi(c) => {
            let key = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env);
            probe_openai_compatible(&client, &c.base_url, key, None).await
        }
        ProviderConfig::Anthropic(c) => {
            // Anthropic added `GET /v1/models` in 2024; reuse the same shape
            // as OpenAI-compatible, but with the `x-api-key` header.
            let key = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env);
            probe_anthropic(&client, &c.base_url, key).await
        }
        ProviderConfig::Google(c) => {
            let key = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env);
            probe_google(&client, &c.base_url, key).await
        }
        ProviderConfig::AzureOpenAi(c) => {
            let key = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env);
            probe_azure_openai(&client, &c.base_url, &c.api_version, key).await
        }
        ProviderConfig::Bedrock(_) => (
            Status::Warn,
            "probe not implemented for Bedrock (no list-models endpoint in the runtime SDK)"
                .to_string(),
        ),
        ProviderConfig::Ollama(c) => probe_ollama(&client, &c.base_url).await,
        ProviderConfig::OpenAiCodex(_) => (
            Status::Warn,
            "probe not implemented for ChatGPT Codex \
             (the backend does not expose a list-models endpoint)"
                .to_string(),
        ),
        ProviderConfig::GitHubCopilot(_) => (
            Status::Warn,
            "probe not implemented for GitHub Copilot \
             (the chat backend does not expose a stable list-models endpoint)"
                .to_string(),
        ),
        ProviderConfig::Faux(_) => (
            Status::Ok,
            "faux provider is in-process; no remote endpoint to probe".to_string(),
        ),
        ProviderConfig::OpenAiCompatible(c) => {
            let mut extra = Vec::new();
            for (key, value) in &c.extra_headers {
                extra.push((key.as_str(), value.as_str()));
            }
            let key = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env);
            probe_openai_compatible(&client, &c.base_url, key, Some(extra)).await
        }
    }
}

/// Resolve the credential for a live probe through the runtime chain so
/// inline-key, `credentials.json`, and fallback-env (e.g. `OPENAI_API_KEY`)
/// setups actually get probed instead of being skipped as "API key env var
/// is unset". `None` keeps the existing skip path when nothing resolves.
fn resolve_probe_key(inline: Option<&str>, env_name: &str) -> Option<String> {
    resolve_api_key_with_inline(inline, env_name)
        .ok()
        .map(|resolved| resolved.value)
}

async fn probe_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<String>,
    extra_headers: Option<Vec<(&str, &str)>>,
) -> (Status, String) {
    let Some(api_key) = api_key.filter(|v| !v.trim().is_empty()) else {
        return (
            Status::Warn,
            "skipping probe: API key env var is unset".to_string(),
        );
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = client.get(&url).bearer_auth(api_key);
    if let Some(headers) = extra_headers {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (Status::Ok, format!("GET {url} returned {status}"))
            } else {
                let body = response.text().await.unwrap_or_default();
                let snippet = body.chars().take(160).collect::<String>();
                (
                    Status::Fail,
                    format!("GET {url} returned {status}: {snippet}"),
                )
            }
        }
        Err(err) => (Status::Fail, format!("GET {url} failed: {err}")),
    }
}

async fn probe_anthropic(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<String>,
) -> (Status, String) {
    let Some(api_key) = api_key.filter(|v| !v.trim().is_empty()) else {
        return (
            Status::Warn,
            "skipping probe: API key env var is unset".to_string(),
        );
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    match client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (Status::Ok, format!("GET {url} returned {status}"))
            } else {
                let body = response.text().await.unwrap_or_default();
                let snippet = body.chars().take(160).collect::<String>();
                (
                    Status::Fail,
                    format!("GET {url} returned {status}: {snippet}"),
                )
            }
        }
        Err(err) => (Status::Fail, format!("GET {url} failed: {err}")),
    }
}

async fn probe_google(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<String>,
) -> (Status, String) {
    let Some(api_key) = api_key.filter(|v| !v.trim().is_empty()) else {
        return (
            Status::Warn,
            "skipping probe: API key env var is unset".to_string(),
        );
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    match client
        .get(&url)
        .header("x-goog-api-key", api_key)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (Status::Ok, format!("GET {url} returned {status}"))
            } else {
                let body = response.text().await.unwrap_or_default();
                let snippet = body.chars().take(160).collect::<String>();
                (
                    Status::Fail,
                    format!("GET {url} returned {status}: {snippet}"),
                )
            }
        }
        Err(err) => (Status::Fail, format!("GET {url} failed: {err}")),
    }
}

async fn probe_azure_openai(
    client: &reqwest::Client,
    base_url: &str,
    api_version: &str,
    api_key: Option<String>,
) -> (Status, String) {
    let Some(api_key) = api_key.filter(|v| !v.trim().is_empty()) else {
        return (
            Status::Warn,
            "skipping probe: API key env var is unset".to_string(),
        );
    };
    let url = format!(
        "{}/models?api-version={api_version}",
        base_url.trim_end_matches('/')
    );
    match client.get(&url).header("api-key", api_key).send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (Status::Ok, format!("GET {url} returned {status}"))
            } else {
                let body = response.text().await.unwrap_or_default();
                let snippet = body.chars().take(160).collect::<String>();
                (
                    Status::Fail,
                    format!("GET {url} returned {status}: {snippet}"),
                )
            }
        }
        Err(err) => (Status::Fail, format!("GET {url} failed: {err}")),
    }
}

async fn probe_ollama(client: &reqwest::Client, base_url: &str) -> (Status, String) {
    let url = format!("{}/tags", base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (Status::Ok, format!("GET {url} returned {status}"))
            } else {
                (
                    Status::Fail,
                    format!("GET {url} returned {status} (Ollama running?)"),
                )
            }
        }
        Err(err) => (
            Status::Fail,
            format!("GET {url} failed: {err} (is the Ollama daemon running?)"),
        ),
    }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
