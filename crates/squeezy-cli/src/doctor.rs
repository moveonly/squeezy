use std::{env, fmt::Write as _, fs, path::PathBuf, time::Duration};

use clap::Args;
use serde_json::json;
use squeezy_core::{
    AppConfig, McpServerConfig, McpTransport, ProviderConfig, ProviderSettings, Result,
    SettingsFile, default_settings_path,
};
use squeezy_llm::{KeySource, fallback_env_var, resolve_api_key_with_inline};
use squeezy_store::{SessionStore, SqueezyStore, ensure_repo_profile};

use crate::update::{self, UpdateStatus};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
    /// Probe the configured provider by issuing a tiny request to confirm
    /// the auth + base_url work. Opt-in because it touches the network
    /// (and, for first-party Anthropic, may consume a handful of tokens).
    #[arg(long)]
    pub probe: bool,
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
                "checks": self.checks.iter().map(|c| json!({
                    "name": c.name,
                    "status": c.status.as_str(),
                    "detail": c.detail,
                })).collect::<Vec<_>>(),
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
    let mut checks = Vec::new();

    let config = match AppConfig::from_env_and_settings() {
        Ok(config) => {
            let labels = config.config_source_labels();
            checks.push(Check {
                name: "config".to_string(),
                status: Status::Ok,
                detail: format!("sources: {}", labels.join(", ")),
            });
            Some(config)
        }
        Err(error) => {
            checks.push(Check {
                name: "config".to_string(),
                status: Status::Fail,
                detail: format!("{error}"),
            });
            None
        }
    };

    if let Some(config) = config.as_ref() {
        match ensure_repo_profile(&config.workspace_root, &config.graph) {
            Ok(loaded) => checks.push(Check {
                name: "repo_profile".to_string(),
                status: Status::Ok,
                detail: format!(
                    "status={} languages={}",
                    loaded.status.as_str(),
                    loaded.profile.languages.len()
                ),
            }),
            Err(error) => checks.push(Check {
                name: "repo_profile".to_string(),
                status: Status::Warn,
                detail: format!("{error}"),
            }),
        }

        let (provider_name, provider_check) = provider_credential_check(&config.provider);
        checks.push(Check {
            name: format!("provider:{provider_name}"),
            status: provider_check.0,
            detail: provider_check.1,
        });

        checks.push(providers_check(&load_user_settings()));

        if args.probe {
            let (status, detail) = probe_provider(&config.provider).await;
            checks.push(Check {
                name: format!("probe:{provider_name}"),
                status,
                detail,
            });
        }

        checks.push(mcp_check(&config.mcp_servers));
        checks.push(session_store_check(config));
        checks.push(state_store_check(config));
    }

    checks.push(sandbox_check());
    checks.push(update_check(update::check_for_update().await));

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
        },
        Err(error) => Check {
            name: "session_store".to_string(),
            status: Status::Fail,
            detail: format!("{}: {error}", root.display()),
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
            }
        }
        Err(error) => Check {
            name: "state_store".to_string(),
            status: Status::Fail,
            detail: format!("{error}"),
        },
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
/// its transport needs (`command` for stdio, `url` for http/sse). Missing
/// fields downgrade the row to `warn` — the server will fail to launch at
/// session start but doctor stays runnable in CI without keys.
fn mcp_check(servers: &std::collections::BTreeMap<String, McpServerConfig>) -> Check {
    if servers.is_empty() {
        return Check {
            name: "mcp".to_string(),
            status: Status::Ok,
            detail: "no MCP servers configured".to_string(),
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
                if server
                    .command
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    if !issues.is_empty() {
                        issues.push_str(", ");
                    }
                    let _ = write!(issues, "{name}: stdio transport without command");
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
        }
    } else {
        Check {
            name: "mcp".to_string(),
            status: Status::Warn,
            detail: format!("{summary}; {issues}"),
        }
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
    }
}

#[cfg(target_os = "macos")]
fn sandbox_check() -> Check {
    if which("sandbox-exec").is_some() {
        Check {
            name: "sandbox".to_string(),
            status: Status::Ok,
            detail: "sandbox-exec is on PATH".to_string(),
        }
    } else {
        Check {
            name: "sandbox".to_string(),
            status: Status::Warn,
            detail: "sandbox-exec not found; shell sandboxing will be limited".to_string(),
        }
    }
}

#[cfg(target_os = "linux")]
fn sandbox_check() -> Check {
    if which("bwrap").is_some() {
        Check {
            name: "sandbox".to_string(),
            status: Status::Ok,
            detail: "bwrap is on PATH".to_string(),
        }
    } else {
        Check {
            name: "sandbox".to_string(),
            status: Status::Warn,
            detail: "bwrap not found; install bubblewrap for shell sandboxing".to_string(),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandbox_check() -> Check {
    Check {
        name: "sandbox".to_string(),
        status: Status::Warn,
        detail: "no sandbox backend known for this OS".to_string(),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
