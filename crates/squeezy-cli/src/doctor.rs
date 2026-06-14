use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, ValueEnum};
use serde_json::json;
use squeezy_core::{
    AppConfig, McpServerConfig, McpTransport, OllamaConfig, ProviderConfig, ProviderSettings,
    Result, SettingsFile, SqueezyError, default_prompt_history_path, default_settings_path,
};
use squeezy_llm::{
    KeySource, fallback_env_var, github_copilot_auth_file_path, resolve_api_key_with_inline,
};
use squeezy_parse::smoke_all_languages;
use squeezy_store::{
    GRAPH_SCHEMA_VERSION, GraphStore, STALE_RUNNING_SESSION_THRESHOLD_MS, SessionQuery,
    SessionStatus, SessionStore, SqueezyStore, StoragePathReport,
    cache_diagnostics_with_session_dir, ensure_repo_profile, graph_path, prune_cache_backups,
    user_squeezy_dir_detail,
};
use squeezy_tools::{McpClientRegistry, McpServerStatus, McpStaleOutcome};
use squeezy_workspace::{WorkspaceRootKind, WorkspaceRootProfile};
use tokio_util::sync::CancellationToken;

use crate::update::{self, UpdateStatus};

const STATE_CACHE_WARN_BYTES: u64 = 128 * 1024 * 1024;
const GRAPH_CACHE_WARN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Args, Default)]
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
    #[arg(long, conflicts_with = "sandbox_teardown")]
    pub sandbox_setup: bool,
    /// Windows only: remove all elevated shell-sandbox machine state (sandbox
    /// users, WFP filters, registry entries, secrets). Performs the action and
    /// exits without running other checks.
    #[arg(long, conflicts_with = "sandbox_setup")]
    pub sandbox_teardown: bool,
    /// Report detailed Linux shell-sandbox posture (user namespace support,
    /// Landlock support, seccomp support, required-mode viability) and exit.
    /// On non-Linux platforms, reports the active backend with a note that the
    /// Linux detail only applies on Linux.
    #[arg(long)]
    pub linux_sandbox: bool,
    /// Run only matching check names. Accepts exact names like
    /// `session_store`, prefixed families like `provider`, and probe names
    /// like `probe:mcp:docs`. Repeat for multiple checks.
    #[arg(long = "only", value_name = "CHECK")]
    pub only: Vec<String>,
    /// Print only checks with this status. Repeat for multiple statuses.
    #[arg(long = "status", value_enum)]
    pub status: Vec<DoctorStatusFilter>,
    /// Skip the release update check. Useful for offline diagnostics and CI.
    #[arg(long = "skip-update")]
    pub skip_update: bool,
    /// Include cache/session storage paths, mount types, and backup age
    /// details in the cache diagnostic row. Output is richest on Linux
    /// (mount classification via `/proc/self/mountinfo`); macOS and
    /// Windows show paths and probe state with `class=unknown`.
    #[arg(long)]
    pub storage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum DoctorStatusFilter {
    Ok,
    Warn,
    Fail,
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

    /// Severity rank for the human-readable ordering: failures first, then
    /// warnings, then ok rows. The `--json` body keeps source order.
    fn severity_rank(self) -> u8 {
        match self {
            Status::Fail => 0,
            Status::Warn => 1,
            Status::Ok => 2,
        }
    }

    fn matches_filter(self, filter: DoctorStatusFilter) -> bool {
        matches!(
            (self, filter),
            (Status::Ok, DoctorStatusFilter::Ok)
                | (Status::Warn, DoctorStatusFilter::Warn)
                | (Status::Fail, DoctorStatusFilter::Fail)
        )
    }
}

#[derive(Debug, Clone)]
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
    warnings: usize,
    failures: usize,
    checks: Vec<Check>,
    version: &'static str,
    target: &'static str,
    json: bool,
    /// Resolved state paths included in the `--json` output so support tooling
    /// can distinguish `%APPDATA%`, `%LOCALAPPDATA%`, `%USERPROFILE%`, and
    /// fallback `.squeezy` state locations on Windows.
    paths: DoctorPaths,
}

#[derive(Debug, Default)]
struct DoctorPaths {
    settings_path: Option<PathBuf>,
    session_root: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    prompt_history_path: Option<PathBuf>,
}

impl DoctorReport {
    pub fn print(&self) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&self.json_body()).unwrap_or_default()
            );
            return;
        }
        let header = if self.exit_code != 0 {
            "squeezy: fail"
        } else if self.warnings > 0 {
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
        // Surface failures first, then warnings, then ok rows, so a skim of a
        // mostly-green run can't miss a failure buried mid-list. `sort_by_key`
        // is stable, so category push-order is preserved within each group.
        // The `--json` body intentionally keeps source order for stable parsing.
        let mut ordered: Vec<&Check> = self.checks.iter().collect();
        ordered.sort_by_key(|check| check.status.severity_rank());
        for check in ordered {
            println!(
                "  [{}] {:<name_width$}  {}",
                check.status.as_str(),
                check.name,
                check.detail,
                name_width = name_width,
            );
        }
        if self.exit_code == 0 && self.warnings == 0 {
            println!("Ready. Run `squeezy` to start.");
        } else {
            println!("See TROUBLESHOOTING.md (crates/squeezy-skills/external-docs/) for fixes.");
        }
    }

    fn json_body(&self) -> serde_json::Value {
        let path_str = |p: &Option<PathBuf>| p.as_ref().map(|v| v.display().to_string());
        json!({
            "version": self.version,
            "target": self.target,
            "ok": self.failures == 0,
            "warnings": self.warnings,
            "failures": self.failures,
            "paths": {
                "settings_path": path_str(&self.paths.settings_path),
                "session_root": path_str(&self.paths.session_root),
                "cache_path": path_str(&self.paths.cache_path),
                "prompt_history_path": path_str(&self.paths.prompt_history_path),
            },
            "checks": self.checks.iter().map(|c| {
                let mut obj = json!({
                    "name": c.name,
                    "status": c.status.as_str(),
                    "detail": c.detail,
                });
                if let (Some(extra), Some(map)) = (c.extra.as_ref(), obj.as_object_mut())
                    && let Some(extra_map) = extra.as_object()
                {
                    // Extra metadata can never overwrite the base row fields.
                    for (k, v) in extra_map {
                        if !matches!(k.as_str(), "name" | "status" | "detail") {
                            map.insert(k.clone(), v.clone());
                        }
                    }
                }
                obj
            }).collect::<Vec<_>>(),
        })
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
            warnings: usize::from(matches!(check.status, Status::Warn)),
            failures: usize::from(matches!(check.status, Status::Fail)),
            checks: vec![check],
            version,
            target,
            json: args.json,
            paths: DoctorPaths::default(),
        });
    }

    // `--linux-sandbox` is a focused diagnostic: report sandbox posture and exit.
    if args.linux_sandbox {
        let check = linux_sandbox_detail_check();
        let exit_code = if matches!(check.status, Status::Fail) {
            1
        } else {
            0
        };
        return Ok(DoctorReport {
            exit_code,
            warnings: usize::from(matches!(check.status, Status::Warn)),
            failures: usize::from(matches!(check.status, Status::Fail)),
            checks: vec![check],
            version,
            target,
            json: args.json,
            paths: DoctorPaths::default(),
        });
    }

    let mut checks = Vec::new();

    let config = if needs_config(args) {
        match AppConfig::from_env_and_settings() {
            Ok(config) => {
                if should_include_check(args, "config") {
                    let labels = config.config_source_labels();
                    checks.push(Check {
                        name: "config".to_string(),
                        status: Status::Ok,
                        detail: format!("sources: {}", labels.join(", ")),
                        extra: None,
                    });
                }
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
        }
    } else {
        None
    };

    if let Some(config) = config.as_ref() {
        if should_include_check(args, "repo_profile") {
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
        }
        if should_include_check(args, "workspace_paths") {
            checks.push(workspace_paths_check(config));
        }

        let (provider_name, provider_check) = provider_credential_check(&config.provider);
        let provider_check_name = format!("provider:{provider_name}");
        if should_include_check(args, &provider_check_name) {
            checks.push(Check {
                name: provider_check_name,
                status: provider_check.0,
                detail: provider_check.1,
                extra: None,
            });
        }

        if should_include_check(args, "providers") {
            checks.push(providers_check(&load_user_settings()));
        }

        let provider_probe_name = format!("probe:{provider_name}");
        if args.probe && should_include_check(args, &provider_probe_name) {
            let (status, detail) = probe_provider(&config.provider).await;
            checks.push(Check {
                name: provider_probe_name,
                status,
                detail,
                extra: None,
            });
        }

        if should_include_check(args, "mcp") {
            checks.push(mcp_check(&config.mcp_servers));
        }
        if args.probe && config.mcp_servers.values().any(|server| server.enabled) {
            checks.extend(probe_mcp_servers(&config.mcp_servers, args).await);
        }
        let skill_catalog =
            squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);
        if should_include_check(args, "skills") {
            checks.push(skills_check(config, &skill_catalog));
        }
        if should_include_check(args, "hooks")
            && let Some(hooks) = hooks_check(config, &skill_catalog)
        {
            checks.push(hooks);
        }
        if should_include_check(args, "skills_roots") {
            checks.push(skills_roots_check(config));
        }
        if should_include_check(args, "hooks:shell") {
            checks.push(hook_shell_check(config));
        }
        if should_include_check(args, "session_store") {
            checks.push(session_store_check(config));
        }
        checks.extend(
            session_paths_checks(config)
                .into_iter()
                .filter(|check| should_include_check(args, &check.name)),
        );
        if should_include_check(args, "user_global_storage") {
            checks.push(user_global_storage_check(config));
        }
        if should_include_check(args, "state_store") {
            checks.push(state_store_check(config));
        }
        if should_include_check(args, "graph_store") {
            checks.push(graph_store_check(config));
        }
        if should_include_check(args, "cache") {
            checks.push(cache_check(config, args.prune_cache, args.storage));
        }
        if should_include_check(args, "settings_location") {
            checks.push(settings_location_check(config));
        }
    }

    if should_include_check(args, "terminal") {
        checks.push(terminal_capability_check());
    }
    if should_include_check(args, "parser_health") {
        checks.push(parser_health_check());
    }
    if should_include_check(args, "sandbox") {
        checks.push(sandbox_check(config.as_ref()));
    }
    if should_include_check(args, "update") {
        if args.no_update_check {
            checks.push(Check {
                name: "update".to_string(),
                status: Status::Ok,
                detail: "skipped (--no-update-check)".to_string(),
                extra: None,
            });
        } else if !args.skip_update {
            checks.push(update_check(update::check_for_update().await));
        }
    }

    let config_failed = config.is_none() && needs_config(args);
    let selector_failures = unmatched_selector_checks(args, &checks, config_failed);
    checks.extend(selector_failures.iter().cloned());

    // Warnings (e.g. missing optional API keys, missing sandbox tool) print as
    // such but do not fail the command: smoke tests in CI / brew test run in
    // environments where keys are absent and still need the binary to come up
    // green. Only hard failures (config load broken, session store unwritable)
    // produce a non-zero exit, matching the `--health` compatibility alias.
    let (warnings, failures) = check_counts(&checks);
    let exit_code = exit_code_for_checks(&checks);
    let mut checks = filter_checks(args, checks);
    for failure in selector_failures {
        if !checks.iter().any(|check| check.name == failure.name) {
            checks.push(failure);
        }
    }

    // Collect resolved state paths for the --json output so support tooling
    // can disambiguate %APPDATA%, %LOCALAPPDATA%, %USERPROFILE%, and fallback
    // .squeezy state locations on Windows without having to reconstruct them
    // from the check detail strings.
    let settings_path = default_settings_path();
    let session_root = config
        .as_ref()
        .map(|c| SessionStore::open(c).root().to_path_buf());
    let cache_path = crate::update::cache_path();
    let prompt_history_path = default_prompt_history_path();

    Ok(DoctorReport {
        exit_code,
        warnings,
        failures,
        checks,
        version,
        target,
        json: args.json,
        paths: DoctorPaths {
            settings_path: Some(settings_path),
            session_root,
            cache_path,
            prompt_history_path: Some(prompt_history_path),
        },
    })
}

fn workspace_paths_check(config: &AppConfig) -> Check {
    let profile = WorkspaceRootProfile::from_path(&config.workspace_root);
    let case = match probe_case_sensitivity(&config.workspace_root) {
        Some(true) => "case_sensitive",
        Some(false) => "case_insensitive",
        None => "case_unknown",
    };
    let status = if matches!(
        profile.kind,
        WorkspaceRootKind::WindowsDriveRoot
            | WorkspaceRootKind::WindowsSystemRoot
            | WorkspaceRootKind::WindowsProfileRoot
            | WorkspaceRootKind::WindowsCloudRoot
            | WorkspaceRootKind::WindowsUncShareRoot
            | WorkspaceRootKind::WindowsVerbatimRoot
            | WorkspaceRootKind::UnixRoot
            | WorkspaceRootKind::UnixProtectedRoot
    ) {
        Status::Warn
    } else {
        Status::Ok
    };
    Check {
        name: "workspace_paths".to_string(),
        status,
        detail: format!(
            "root={} canonical={} normalized={} kind={} volume={}",
            profile.original,
            profile.canonical.as_deref().unwrap_or("unavailable"),
            profile.normalized,
            profile.kind.as_str(),
            case
        ),
        extra: None,
    }
}

fn probe_case_sensitivity(root: &std::path::Path) -> Option<bool> {
    let lower = root.join(format!(
        ".squeezy-doctor-case-probe-{}-a",
        std::process::id()
    ));
    let upper = root.join(format!(
        ".SQUEEZY-DOCTOR-CASE-PROBE-{}-A",
        std::process::id()
    ));
    fs::write(&lower, b"case").ok()?;
    let case_sensitive = !upper.exists();
    let _ = fs::remove_file(&lower);
    Some(case_sensitive)
}

fn needs_config(args: &DoctorArgs) -> bool {
    if args.only.is_empty() {
        return true;
    }
    args.only.iter().any(|selector| {
        !matches!(
            selector.as_str(),
            "parser_health" | "sandbox" | "terminal" | "update"
        )
    })
}

fn exit_code_for_checks(checks: &[Check]) -> i32 {
    let (_, failures) = check_counts(checks);
    if failures > 0 { 1 } else { 0 }
}

fn should_include_check(args: &DoctorArgs, name: &str) -> bool {
    args.only.iter().all(|selector| selector.trim().is_empty())
        || args
            .only
            .iter()
            .any(|selector| check_name_matches(selector, name))
}

fn check_name_matches(selector: &str, name: &str) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    name == selector
        || (name.len() > selector.len()
            && name.as_bytes().get(selector.len()) == Some(&b':')
            && name.starts_with(selector))
}

fn filter_checks(args: &DoctorArgs, checks: Vec<Check>) -> Vec<Check> {
    if args.status.is_empty() {
        return checks;
    }
    checks
        .into_iter()
        .filter(|check| {
            args.status
                .iter()
                .any(|filter| check.status.matches_filter(*filter))
        })
        .collect()
}

fn unmatched_selector_checks(
    args: &DoctorArgs,
    checks: &[Check],
    config_failed: bool,
) -> Vec<Check> {
    // Split the unmatched selectors into two buckets:
    //   * `unknown` -- typos / unrecognised selector names. These are
    //     hard failures and tell the user to fix the command line.
    //   * `skipped_due_to_config` -- selectors that *would* have been
    //     evaluated, but config failed to load so the underlying check
    //     never ran (e.g. `--only providers` against a corrupt
    //     `settings.toml`). These are not user errors; they are a
    //     downstream consequence of the `config` row already failing,
    //     so we surface them as a single `selector` warn note instead
    //     of silently dropping them.
    let mut unknown = Vec::new();
    let mut skipped_due_to_config = Vec::new();
    for selector in &args.only {
        let selector = selector.trim();
        if selector.is_empty()
            || checks
                .iter()
                .any(|check| check_name_matches(selector, &check.name))
            || (selector == "update" && (args.skip_update || args.no_update_check))
        {
            continue;
        }
        if config_failed
            && !matches!(
                selector,
                "parser_health" | "sandbox" | "terminal" | "update"
            )
        {
            skipped_due_to_config.push(selector.to_string());
        } else {
            unknown.push(selector.to_string());
        }
    }
    let mut out = Vec::new();
    if !unknown.is_empty() {
        out.push(Check {
            name: "selector".to_string(),
            status: Status::Fail,
            detail: format!("unknown doctor --only selector(s): {}", unknown.join(", ")),
            extra: None,
        });
    }
    if !skipped_due_to_config.is_empty() {
        // Distinct name so the "always re-include selector failures
        // after status filtering" loop in `run()` does not treat this
        // warn row and the `selector` fail row above as the same entry.
        out.push(Check {
            name: "selector:skipped".to_string(),
            status: Status::Warn,
            detail: format!(
                "unable to evaluate {} because the `config` row failed to load; \
                 fix the configuration to re-enable {}",
                skipped_due_to_config.join(", "),
                if skipped_due_to_config.len() == 1 {
                    "this check"
                } else {
                    "these checks"
                }
            ),
            extra: None,
        });
    }
    out
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
        ProviderConfig::Ollama(c) => ("ollama", ollama_credential_check(c)),
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
        Err(_) => (Status::Warn, missing_key_remediation(env_name)),
    }
}

/// Actionable remediation shown when no credential resolves for a
/// provider: names the canonical env var and the three ways to supply a
/// key. Shared so the credential row and the live-probe skip row speak
/// with one voice instead of the probe path's bare "env var is unset".
fn missing_key_remediation(env_name: &str) -> String {
    format!(
        "{env_name} not set; export it, set [providers.<name>] api_key = \"…\" in \
         ~/.squeezy/settings.toml, or run `squeezy auth set <provider>`"
    )
}

const DEFAULT_OLLAMA_API_KEY_ENV: &str = "OLLAMA_API_KEY";

/// Credential check for the Ollama provider. Ollama operates without auth by
/// default, so an absent `OLLAMA_API_KEY` is not a failure. However, if a
/// user has configured a *custom* `api_key_env` (e.g. pointing at an Ollama
/// Cloud token) and that env var is unset, we surface a `Warn` so the
/// misconfiguration is caught early rather than producing a silent 401.
fn ollama_credential_check(c: &OllamaConfig) -> (Status, String) {
    let has_inline = c.api_key.as_deref().is_some_and(|v| !v.trim().is_empty());
    if has_inline || !c.api_key_env.is_empty() {
        match resolve_api_key_with_inline(c.api_key.as_deref(), &c.api_key_env) {
            Ok(resolved) => (
                Status::Ok,
                format!(
                    "base_url={}, bearer token resolved via {}",
                    c.base_url,
                    key_source_label(resolved.source, &c.api_key_env)
                ),
            ),
            Err(_) => {
                // Only warn when the user explicitly configured a non-default
                // key env — that signals they expect auth to be required.
                // For the default OLLAMA_API_KEY case, absence is expected.
                if c.api_key_env != DEFAULT_OLLAMA_API_KEY_ENV {
                    (
                        Status::Warn,
                        format!(
                            "base_url={} — {} is not set; set it or remove api_key_env to use \
                             the default no-auth local Ollama path",
                            c.base_url, c.api_key_env
                        ),
                    )
                } else {
                    (
                        Status::Ok,
                        format!(
                            "base_url={} (OLLAMA_API_KEY unset; no-auth local deployment assumed)",
                            c.base_url
                        ),
                    )
                }
            }
        }
    } else {
        (
            Status::Ok,
            format!("base_url={} (no API key required)", c.base_url),
        )
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

/// Report resolved session-related paths and whether XDG variables are honored.
/// Shows the workspace session root, global index path, static memory path, and
/// warns when `HOME` is unset, XDG is invalid, or the active index directory is
/// read-only.
fn session_paths_checks(config: &AppConfig) -> Vec<Check> {
    let mut checks = Vec::new();
    let store = SessionStore::open(config);
    let session_root = store.root().display().to_string();

    let index_path = SessionStore::global_index_path();
    let memory_path = SessionStore::memory_path();
    let xdg_state_home = env::var_os("XDG_STATE_HOME");
    let xdg_honored = xdg_state_home
        .as_ref()
        .is_some_and(|xdg| PathBuf::from(xdg).is_absolute());
    let xdg_invalid = xdg_state_home.as_ref().is_some_and(|xdg| {
        let path = PathBuf::from(xdg);
        !path.is_absolute()
    });

    // Warn when HOME is unset; an absolute XDG_STATE_HOME can still carry the
    // global index, but the memory file remains HOME-based.
    if env::var_os("HOME").is_none() {
        let detail = if index_path.is_some() {
            "HOME is not set; memory file is unavailable; global index uses XDG_STATE_HOME"
                .to_string()
        } else {
            "HOME is not set; global session index and memory file are unavailable".to_string()
        };
        checks.push(Check {
            name: "session_home".to_string(),
            status: Status::Warn,
            detail,
            extra: None,
        });
    }

    if xdg_invalid {
        let raw = xdg_state_home
            .as_ref()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        checks.push(Check {
            name: "session_xdg_state_home".to_string(),
            status: Status::Warn,
            detail: format!("XDG_STATE_HOME={raw} is not absolute; falling back to HOME"),
            extra: None,
        });
    }

    // Check that the directory that will hold the global index is writable. This
    // uses the resolved index path (XDG or legacy) rather than HOME so the check
    // is accurate when XDG_STATE_HOME redirects to a different location.
    let index_dir_readonly = index_path
        .as_ref()
        .and_then(|p| p.parent())
        .is_some_and(|dir| {
            fs::metadata(dir)
                .map(|m| m.permissions().readonly())
                .unwrap_or(false)
        });
    if index_dir_readonly {
        let dir = index_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        checks.push(Check {
            name: "session_home".to_string(),
            status: Status::Warn,
            detail: format!(
                "global index directory {dir} appears read-only; index writes will fail"
            ),
            extra: None,
        });
    }

    let index_detail = match &index_path {
        Some(p) => {
            if xdg_honored {
                format!("global_index={} (XDG_STATE_HOME honored)", p.display())
            } else {
                format!("global_index={}", p.display())
            }
        }
        None => "global_index=unavailable (HOME unset)".to_string(),
    };
    let memory_detail = match &memory_path {
        Some(p) => format!("memory={}", p.display()),
        None => "memory=unavailable (HOME unset)".to_string(),
    };

    // Warn if the legacy path still exists after an XDG migration — it continues
    // to be merged on every startup until it is manually removed.
    let legacy_present = xdg_honored
        && SessionStore::legacy_global_index_path()
            .is_some_and(|lp| lp.exists() && index_path.as_ref().is_some_and(|ip| *ip != lp));
    if legacy_present {
        let lp = SessionStore::legacy_global_index_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        checks.push(Check {
            name: "session_legacy_index".to_string(),
            status: Status::Warn,
            detail: format!(
                "legacy global index still present at {lp}; \
                 it will continue to be merged on every startup until removed"
            ),
            extra: None,
        });
    }

    let stale_running = if !index_dir_readonly {
        let stale_count = count_stale_running_sessions(&store);
        if stale_count > 0 {
            Some(stale_count)
        } else {
            None
        }
    } else {
        None
    };

    let mut detail = format!("session_root={session_root}  {index_detail}  {memory_detail}");
    let mut status = Status::Ok;
    if let Some(count) = stale_running {
        detail.push_str(&format!(
            "; {count} stale running session(s) (started >{}h ago); \
             run `squeezy sessions list` to review",
            STALE_RUNNING_SESSION_THRESHOLD_MS / (3600 * 1000)
        ));
        status = Status::Warn;
    }

    checks.push(Check {
        name: "session_paths".to_string(),
        status,
        detail,
        extra: None,
    });
    checks
}

/// Count sessions whose status is `Running` but whose last-event timestamp
/// is older than [`STALE_RUNNING_SESSION_THRESHOLD_MS`]. Returns 0 if the
/// session list cannot be read.
fn count_stale_running_sessions(store: &SessionStore) -> usize {
    let Ok(sessions) = store.list(&SessionQuery::default()) else {
        return 0;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    sessions
        .iter()
        .filter(|s| {
            s.status == SessionStatus::Running
                && now.saturating_sub(s.started_at_ms) > STALE_RUNNING_SESSION_THRESHOLD_MS
        })
        .count()
}

fn session_store_check(config: &AppConfig) -> Check {
    let store = SessionStore::open(config);
    let root = store.root().to_path_buf();
    match probe_writable(&root) {
        Ok(()) => {
            let index = store.session_index_diagnostics();
            let index_detail = match index.error {
                Some(error) => format!("metadata_index_error={error}"),
                None if index.exists => format!(
                    "metadata_index={} entries={} schema={}",
                    index.path.display(),
                    index.indexed_sessions,
                    index
                        .schema_version
                        .map(|version| version.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ),
                None => format!("metadata_index={} missing", index.path.display()),
            };
            Check {
                name: "session_store".to_string(),
                status: Status::Ok,
                detail: format!(
                    "writable JSON/JSONL logs: {}; {index_detail}",
                    root.display()
                ),
                extra: None,
            }
        }
        Err(error) => Check {
            name: "session_store".to_string(),
            status: Status::Fail,
            detail: format!("{}: {error}", root.display()),
            extra: None,
        },
    }
}

fn probe_writable(root: &std::path::Path) -> std::io::Result<()> {
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
            detail: format!("{} ({})", error, storage_error_hint(&error.to_string())),
            extra: None,
        },
    }
}

fn graph_store_check(config: &AppConfig) -> Check {
    let path = graph_path(&config.workspace_root, config.cache.root.as_deref());
    if !path.exists() {
        return Check {
            name: "graph_store".to_string(),
            status: Status::Ok,
            detail: format!("absent: {} (graph cache not created yet)", path.display()),
            extra: None,
        };
    }
    match GraphStore::probe_path_read_only(&path) {
        Ok(probe) if probe.schema_version == Some(GRAPH_SCHEMA_VERSION) => Check {
            name: "graph_store".to_string(),
            status: Status::Ok,
            detail: format!("readable: {}", probe.path.display()),
            extra: None,
        },
        Ok(probe) => Check {
            name: "graph_store".to_string(),
            status: Status::Warn,
            detail: format!(
                "readable with schema {:?}, expected {}; graph persistence will reinitialize on next write: {}",
                probe.schema_version,
                GRAPH_SCHEMA_VERSION,
                probe.path.display()
            ),
            extra: None,
        },
        Err(error) => {
            // On Windows, another agent process holding graph.redb open will
            // fail this probe with ERROR_SHARING_VIOLATION (32) or
            // ERROR_LOCK_VIOLATION (33); occasionally ERROR_ACCESS_DENIED (5).
            // Distinguish that from a real open failure so the user isn't told
            // persistence is broken when it's actually just another live
            // Squeezy session. The literal Win32 messages for codes 32/33 do
            // *not* contain the phrase "sharing violation", so we match by
            // `raw_os_error()` first (when we can recover the underlying
            // `io::Error`), and fall back to substring markers that *do*
            // appear in those messages ("being used by another process",
            // "another process has locked").
            let text = error.to_string();
            let is_locked = match &error {
                SqueezyError::Io(io_err) => matches!(io_err.raw_os_error(), Some(5 | 32 | 33)),
                _ => false,
            } || {
                let lower = text.to_ascii_lowercase();
                lower.contains("being used by another process")
                    || lower.contains("another process has locked")
                    || lower.contains("sharing violation")
                    || lower.contains("access is denied")
            };
            if is_locked {
                Check {
                    name: "graph_store".to_string(),
                    status: Status::Ok,
                    detail: format!(
                        "locked by another process (graph persistence is active): {}",
                        path.display()
                    ),
                    extra: None,
                }
            } else {
                Check {
                    name: "graph_store".to_string(),
                    status: Status::Warn,
                    detail: format!(
                        "{text}; graph persistence will be disabled until graph.redb can be opened: {}",
                        path.display()
                    ),
                    extra: None,
                }
            }
        }
    }
}

fn user_global_storage_check(config: &AppConfig) -> Check {
    let mut detail = user_squeezy_dir_detail();
    if cfg!(windows) && env::var_os("HOME").is_none() {
        detail.push_str(
            "; HOME is unset, using native Windows profile/app-data fallback if available",
        );
    }
    if workspace_looks_synced(&config.workspace_root) && config.cache.root.is_none() {
        detail.push_str(
            "; workspace appears to be under a synced folder, consider a short local [cache].root",
        );
        return Check {
            name: "user_global_storage".to_string(),
            status: Status::Warn,
            detail,
            extra: None,
        };
    }
    Check {
        name: "user_global_storage".to_string(),
        status: Status::Ok,
        detail,
        extra: None,
    }
}

/// Substrings (lowercased) that mark a path component as belonging to a
/// known consumer cloud-sync client root. Matches the spellings that
/// real-world Windows / macOS / Linux installers create; the heuristic is
/// intentionally over-eager — false positives cost a single doctor warn,
/// while a missed sync mount can cost the user repeated cache corruption
/// when the agent races the sync engine.
const SYNCED_FOLDER_MARKERS: &[&str] = &[
    "onedrive",
    "dropbox",
    "googledrive",
    "google drive",
    "drive file stream",
    "drivefs",
    "icloud drive",
    "icloud-drive",
    "icloud_drive",
    "icloud~drive",
    "syncthing",
    "pcloud",
    "sync.com",
    "nextcloud",
    "owncloud",
    "yandex.disk",
    "yandexdisk",
];

fn workspace_looks_synced(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        SYNCED_FOLDER_MARKERS
            .iter()
            .any(|marker| name.contains(marker))
    })
}

fn storage_error_hint(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("lock") || lower.contains("busy") || lower.contains("would block") {
        "likely lock contention"
    } else if lower.contains("permission denied") || lower.contains("access denied") {
        "likely permission problem"
    } else if lower.contains("no space") || lower.contains("enospc") {
        "likely disk full"
    } else if lower.contains("corrupt")
        || lower.contains("checksum")
        || lower.contains("invalid database")
        || lower.contains("invalid magic")
    {
        "possible redb corruption"
    } else if lower.contains("unsupported")
        || lower.contains("operation not supported")
        || lower.contains("not supported")
    {
        "possible unsupported filesystem behavior"
    } else {
        "storage open failed"
    }
}

fn cache_check(config: &AppConfig, prune: bool, storage: bool) -> Check {
    let diagnostics = match cache_diagnostics_with_session_dir(
        &config.workspace_root,
        config.cache.root.as_deref(),
        config.session_logs.log_dir.as_deref(),
    ) {
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
    if let Some(stats) = diagnostics.state_stats.as_ref() {
        if let Some(error) = &stats.error {
            detail.push_str(&format!("; state_stats_error={error}"));
        } else {
            detail.push_str(&format!(
                "; state schema={} receipts={} reads={} mcp_tools={} observations={} checkpoints={}",
                stats
                    .schema_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                stats.tool_receipts,
                stats.read_snapshots,
                stats.mcp_tool_cache_entries,
                stats.observations,
                stats.compaction_checkpoints,
            ));
        }
    }
    if let Some(stats) = diagnostics.graph_stats.as_ref() {
        if let Some(error) = &stats.error {
            detail.push_str(&format!("; graph_stats_error={error}"));
        } else {
            detail.push_str(&format!(
                "; graph schema={} partitions={} resolver_entries={} import_graph={}",
                stats
                    .schema_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                stats.graph_partitions,
                stats.resolver_entries,
                stats.import_graph_present,
            ));
        }
    }
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
    let storage_warnings: Vec<&StoragePathReport> = diagnostics
        .storage
        .iter()
        .filter(|report| report.warning.is_some())
        .collect();
    if !storage_warnings.is_empty() {
        status = Status::Warn;
        detail.push_str("; storage warning: ");
        detail.push_str(
            &storage_warnings
                .iter()
                .map(|report| {
                    format!(
                        "{}={}({})",
                        report.label,
                        report.filesystem_type.as_deref().unwrap_or("unknown"),
                        report.classification.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
        // Use the per-label relocation hint so a warning on `sessions`
        // points at `[session].log_dir` instead of misleading the user
        // to move `[cache].root` (which only governs cache/state/graph
        // paths). Dedupe across warnings so two cache-flavoured labels
        // collapse to one suggestion.
        let mut hints: Vec<&'static str> = storage_warnings
            .iter()
            .map(|report| squeezy_store::storage_relocation_hint(&report.label))
            .collect();
        hints.sort_unstable();
        hints.dedup();
        detail.push_str("; move ");
        detail.push_str(&hints.join(" and "));
        detail.push_str(" to a local SSD path");
    }
    if storage {
        detail.push_str("; storage: ");
        detail.push_str(&format_storage_reports(&diagnostics.storage));
        detail.push_str("; probes: ");
        detail.push_str(&format_storage_probes(config));
        if !diagnostics.backups.is_empty() {
            detail.push_str("; backups: ");
            detail.push_str(&format_backup_details(&diagnostics.backups));
            detail.push_str("; prune command: squeezy doctor --prune-cache");
        }
    }
    if prune {
        match prune_cache_backups(&config.workspace_root, config.cache.root.as_deref()) {
            Ok(report) => {
                detail.push_str(&format!(
                    "; pruned {} backups ({})",
                    report.removed_files.len(),
                    format_bytes(report.removed_bytes)
                ));
                if !report.failed_files.is_empty() {
                    status = Status::Warn;
                    let failed = report
                        .failed_files
                        .iter()
                        .map(|(path, error)| format!("{} ({error})", path.display()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    detail.push_str(&format!("; failed to prune: {failed}"));
                }
                if diagnostics.state.size_bytes <= STATE_CACHE_WARN_BYTES
                    && diagnostics.graph.size_bytes <= GRAPH_CACHE_WARN_BYTES
                    && report.failed_files.is_empty()
                    && storage_warnings.is_empty()
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

fn format_storage_probes(config: &AppConfig) -> String {
    let state = match SqueezyStore::open(&config.workspace_root, config.cache.root.as_deref()) {
        Ok(store) => format!("state.redb=open({})", store.path().display()),
        Err(error) => format!(
            "state.redb=fail:{}({})",
            storage_error_hint(&error.to_string()),
            error
        ),
    };
    let graph = match GraphStore::open(&config.workspace_root, config.cache.root.as_deref()) {
        Ok(store) => format!("graph.redb=open({})", store.path().display()),
        Err(error) => format!(
            "graph.redb=fail:{}({})",
            storage_error_hint(&error.to_string()),
            error
        ),
    };
    format!("{state}, {graph}")
}

fn format_storage_reports(reports: &[StoragePathReport]) -> String {
    reports
        .iter()
        .map(|report| {
            // Substitute "n/a" rather than "unknown" so a row that simply
            // didn't match a mountinfo entry (macOS/Windows, or a path
            // outside `/proc/self/mountinfo`) reads as missing detail
            // rather than as a defect-classified mount.
            let fs_type = report.filesystem_type.as_deref().unwrap_or("n/a");
            let source = report.mount_source.as_deref().unwrap_or("n/a");
            format!(
                "{}={} fs={} source={} class={}",
                report.label,
                report.path.display(),
                fs_type,
                source,
                report.classification.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn format_backup_details(backups: &[squeezy_store::CacheFileReport]) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis());
    backups
        .iter()
        .map(|backup| {
            let age = match (now_ms, backup.modified_unix_ms) {
                (Some(now), Some(modified)) if now >= modified => {
                    format_age_ms(now.saturating_sub(modified))
                }
                _ => "age unknown".to_string(),
            };
            format!(
                "{} {} {}",
                backup.path.display(),
                format_bytes(backup.size_bytes),
                age
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_age_ms(age_ms: u128) -> String {
    const MINUTE: u128 = 60_000;
    const HOUR: u128 = 60 * MINUTE;
    const DAY: u128 = 24 * HOUR;
    if age_ms >= DAY {
        format!("{}d old", age_ms / DAY)
    } else if age_ms >= HOUR {
        format!("{}h old", age_ms / HOUR)
    } else if age_ms >= MINUTE {
        format!("{}m old", age_ms / MINUTE)
    } else {
        "less than 1m old".to_string()
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

/// Summarize the `[providers.*]` blocks in the user's settings file alone:
/// for each section, report whether settings carry an inline `api_key` or a
/// configured `api_key_env` that is set (`configured`), or neither
/// (`no key in settings`). Providers that don't take a key (`bedrock`,
/// `ollama`) are flagged `keyless`. Empty `[providers]` is reported as `ok`
/// with a note.
///
/// This row inspects user settings only; it deliberately does NOT walk the
/// full runtime credential chain (`credentials.json`, the conventional
/// vendor fallback env var, `SQUEEZY_CREDENTIALS_JSON`). The active
/// provider's `provider:<name>` row resolves through that chain, so a
/// provider keyed solely by `OPENAI_API_KEY` / `credentials.json` shows
/// `no key in settings` here while resolving Ok there — the labels keep the
/// two rows from contradicting each other.
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
    for (name, settings) in providers {
        let state = provider_settings_state(name, settings);
        if !detail.is_empty() {
            detail.push_str(", ");
        }
        let _ = write!(detail, "{name}={state}");
    }
    // Settings-only inventory: a section with no inline/env key here may
    // still resolve via the credential chain that the active provider's
    // `provider:<name>` row checks, so this row stays informational
    // rather than warning and contradicting that authoritative row.
    Check {
        name: "providers".to_string(),
        status: Status::Ok,
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
    "no key in settings"
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
                } else {
                    // Warn about relative command paths — they depend on the
                    // working-directory and $PATH at startup time and may
                    // resolve to a different binary than intended.
                    // Only checked on Unix where absolute paths start with `/`;
                    // on Windows the test fixtures use Unix-style paths that
                    // `Path::is_absolute` considers relative.
                    if cfg!(unix) && !std::path::Path::new(cmd).is_absolute() {
                        if !issues.is_empty() {
                            issues.push_str(", ");
                        }
                        let _ = write!(
                            issues,
                            "{name}: stdio command is a relative path ({cmd:?}); \
                             use an absolute path for reproducible resolution"
                        );
                    }
                    if let Some(issue) = mcp_stdio_command_issue(cmd) {
                        if !issues.is_empty() {
                            issues.push_str(", ");
                        }
                        let _ = write!(issues, "{name}: {issue}");
                    }
                    // Warn about env overrides that can redirect dynamic
                    // linker or interpreter search paths — common vectors for
                    // unintentional binary substitution on Linux.
                    const RISKY_ENV_VARS: &[&str] = &[
                        "PATH",
                        "LD_PRELOAD",
                        "LD_LIBRARY_PATH",
                        "PYTHONPATH",
                        "NODE_OPTIONS",
                    ];
                    for var in RISKY_ENV_VARS {
                        if server.env.contains_key(*var) {
                            if !issues.is_empty() {
                                issues.push_str(", ");
                            }
                            let _ = write!(
                                issues,
                                "{name}: env overrides {var} for stdio MCP server (security risk)"
                            );
                        }
                    }
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
    // Accept both the platform-native separator and forward slash, so a config
    // written with `/` on Windows (e.g. `bin/server.exe`) still routes through
    // the direct-path branch instead of being treated as a bare PATH lookup.
    if path.is_absolute() || binary.contains(std::path::MAIN_SEPARATOR) || binary.contains('/') {
        return mcp_stdio_path_issue(path);
    }

    // Otherwise walk PATH looking for the binary. If PATH is unset or
    // unparseable we cannot resolve the command, so surface that explicitly
    // rather than silently passing the check.
    let Some(path_var) = std::env::var_os("PATH") else {
        return Some(format!(
            "PATH is unset; cannot resolve stdio command '{binary}'"
        ));
    };
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

/// Given a resolved path, check whether it names an executable file (Unix) or
/// an existing file (Windows). Returns a warning string on problems, `None` on ok.
fn mcp_stdio_path_issue(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) if !meta.is_file() => Some(format!(
                "stdio command '{}' exists but is not a file",
                path.display()
            )),
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
        if path.is_file() {
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
///
/// For stdio servers that fail or produce no tools, the last few stderr lines
/// captured during startup are appended to the detail to help diagnose the
/// failure without having to re-run the server manually.
async fn probe_mcp_servers(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
    args: &DoctorArgs,
) -> Vec<Check> {
    let servers = servers
        .iter()
        .filter(|(name, server)| {
            server.enabled && should_include_check(args, &format!("probe:mcp:{name}"))
        })
        .map(|(name, server)| (name.clone(), server.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if servers.is_empty() {
        return Vec::new();
    }
    let registry = McpClientRegistry::new(servers.clone());
    let outcome = registry.refresh_tools(CancellationToken::new()).await;

    // Collect stderr tails before shutdown so the ring buffer is still live.
    let mut stderr_tails: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, server) in &servers {
        if server.enabled && matches!(server.transport, McpTransport::Stdio) {
            let tail = registry.stderr_tail(name).await;
            if !tail.is_empty() {
                stderr_tails.insert(name.clone(), tail);
            }
        }
    }

    registry.shutdown().await;
    outcome
        .status
        .per_server
        .iter()
        .map(|(name, server_status)| {
            let (status, mut detail) = match server_status {
                McpServerStatus::Ready { tools_count, .. } => {
                    let server = servers.get(name);
                    let extra = if let Some(s) = server {
                        stdio_probe_detail(name, s)
                    } else {
                        String::new()
                    };
                    let base = format!("handshake ok; {tools_count} tools advertised");
                    (
                        Status::Ok,
                        if extra.is_empty() {
                            base
                        } else {
                            format!("{base}; {extra}")
                        },
                    )
                }
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
            // Append captured stderr for failed/warn stdio servers to give
            // actionable startup diagnostics without requiring a manual re-run.
            if !matches!(status, Status::Ok)
                && let Some(lines) = stderr_tails.get(name)
            {
                let excerpt: String = lines.iter().map(|l| format!("\n  stderr: {l}")).collect();
                detail.push_str(&excerpt);
            }
            Check {
                name: format!("probe:mcp:{name}"),
                status,
                detail,
                extra: None,
            }
        })
        .collect()
}

/// Return a compact detail string describing the stdio MCP server configuration
/// for successful probe rows (resolved command path, env key count, platform).
fn stdio_probe_detail(name: &str, server: &McpServerConfig) -> String {
    let _ = name;
    let Some(cmd) = server.command.as_deref() else {
        return String::new();
    };
    let mut parts = Vec::new();
    // Resolved binary path via PATH walk (best-effort; empty on failure).
    if std::path::Path::new(cmd).is_absolute() {
        parts.push(format!("cmd={cmd}"));
    } else if let Some(resolved) = which_in_path(cmd) {
        parts.push(format!("cmd={resolved}"));
    } else {
        parts.push(format!("cmd={cmd} (not found in PATH)"));
    }
    if !server.env.is_empty() {
        parts.push(format!("env_keys={}", server.env.len()));
    }
    #[cfg(unix)]
    parts.push("process_group=new".to_string());
    parts.join(" ")
}

/// Walk `$PATH` to find the first executable named `name`. Returns the absolute
/// path as a `String`, or `None` if no match is found.
///
/// On Unix the candidate must have at least one execute bit set; a file that
/// is readable but not executable would fail at startup with "Permission denied"
/// and is more useful to surface as "not found" in the doctor detail string.
fn which_in_path(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return candidate.into_os_string().into_string().ok();
        }
    }
    None
}

/// Return `true` if `path` is a regular file that the current process can
/// attempt to execute. On Unix this checks `is_file()` *and* at least one
/// execute bit in the mode bits; on other platforms it falls back to
/// `is_file()` only (no permission API available).
fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn mcp_stale_outcome_detail(outcome: &McpStaleOutcome) -> String {
    match outcome {
        McpStaleOutcome::Failed { error } => format!("discovery failed: {error}"),
        McpStaleOutcome::Cancelled => "discovery was cancelled".to_string(),
    }
}

/// Probe whether a program exists on `PATH` by walking the entries.
///
/// Used instead of spawning a process so the check stays offline-safe
/// and fast. On Windows, also checks `<name>.exe` as a fallback.
#[cfg(windows)]
fn program_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.join(name).exists() {
            return true;
        }
        #[cfg(windows)]
        if dir.join(format!("{name}.exe")).exists() {
            return true;
        }
    }
    false
}

/// When `[skills] hooks_enabled = true`, verify that the hook shell is
/// reachable so Windows users get an explicit diagnostic instead of
/// discovering the problem at hook fire time (where failures are
/// silently fail-open).
///
/// - On Unix, checks for `/bin/sh`.
/// - On Windows, checks for `pwsh`, then `powershell`, then `cmd`
///   (matching the resolution order in the skill hook runner).
fn hook_shell_check(config: &AppConfig) -> Check {
    if !config.skills.hooks_enabled {
        return Check {
            name: "hooks:shell".to_string(),
            status: Status::Ok,
            detail: "hooks_enabled=false; shell check skipped".to_string(),
            extra: None,
        };
    }

    #[cfg(not(windows))]
    {
        let shell = std::path::Path::new("/bin/sh");
        if shell.exists() {
            Check {
                name: "hooks:shell".to_string(),
                status: Status::Ok,
                detail: "hook shell available: /bin/sh".to_string(),
                extra: None,
            }
        } else {
            Check {
                name: "hooks:shell".to_string(),
                status: Status::Warn,
                detail: "hooks_enabled=true but /bin/sh was not found; skill hooks will fail-open when spawning".to_string(),
                extra: None,
            }
        }
    }

    #[cfg(windows)]
    {
        let candidates: &[&str] = &["pwsh", "powershell", "cmd"];
        for &shell in candidates {
            if program_on_path(shell) {
                return Check {
                    name: "hooks:shell".to_string(),
                    status: Status::Ok,
                    detail: format!("hook shell available: {shell}"),
                    extra: None,
                };
            }
        }
        let tried = candidates.join(", ");
        Check {
            name: "hooks:shell".to_string(),
            status: Status::Warn,
            detail: format!(
                "hooks_enabled=true but no hook shell ({tried}) found on PATH; \
                 skill hooks will fail-open when spawning"
            ),
            extra: None,
        }
    }
}

/// Summarize the discovered skill catalog without doing any network or
/// long-running work: walks the configured roots, counts total /
/// enabled / disabled skills, and downgrades to `warn` when a
/// same-precedence name collision flips trigger activation into
/// ambiguous mode. When `hooks_enabled`, also reports hook handler
/// counts and which skills declare hooks. Pure stat work so the row
/// stays fast and matches the rest of `doctor`'s offline-CI contract.
fn skills_check(config: &AppConfig, catalog: &squeezy_skills::SkillCatalog) -> Check {
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
        // Summarise which enabled skills declare hooks and how many
        // handler specs they contain, so users can confirm what will
        // fire without inspecting individual SKILL.md files.
        let mut total_specs = 0usize;
        let mut hook_skills: Vec<String> = Vec::new();
        for summary in summaries.iter().filter(|s| !s.disabled) {
            if let Ok(loaded) = catalog.load(&summary.name)
                && !loaded.hooks.is_empty()
            {
                let count: usize = loaded
                    .hooks
                    .values()
                    .flat_map(|matchers| matchers.iter())
                    .flat_map(|m| m.hooks.iter())
                    .filter(|s| s.kind_valid)
                    .count();
                if count > 0 {
                    total_specs += count;
                    hook_skills.push(summary.name.clone());
                }
            }
        }
        detail.push_str(&format!("; hooks_enabled handlers={total_specs}"));
        if !hook_skills.is_empty() {
            detail.push_str(&format!(" ({})", hook_skills.join(", ")));
        }
        // Hooks run as sh -c with Squeezy's full process privileges, so
        // warn in doctor whenever this high-trust mode is active so
        // operators notice it in CI smoke runs.
        detail.push_str(" (hooks run with Squeezy process privileges)");
        // On Windows, skill hooks run through `sh -c`. If `sh` is not in
        // PATH (no Git Bash / MSYS), hooks will fail to spawn; warn early
        // before the first dispatch.
        #[cfg(windows)]
        if which_sh_missing() {
            detail.push_str("; `sh` not found in PATH (add sh via Git for Windows or MSYS2, or add `failure_policy: deny` to policy hooks)");
        }
        return Check {
            name: "skills".to_string(),
            status: Status::Warn,
            detail,
            extra: None,
        };
    }
    Check {
        name: "skills".to_string(),
        status: Status::Ok,
        detail,
        extra: None,
    }
}

/// Validate hook declarations for every non-disabled skill when
/// `[skills].hooks_enabled = true`. Reports missing scripts, missing
/// executable bits, missing shebang lines, and inline shell snippets.
/// Returns `None` when hooks are disabled so the check row is omitted
/// entirely rather than cluttering the output.
fn hooks_check(config: &AppConfig, catalog: &squeezy_skills::SkillCatalog) -> Option<Check> {
    if !config.skills.hooks_enabled {
        return None;
    }
    let issues = squeezy_skills::catalog_hook_issues(catalog);

    // Count declared handlers for the detail line.
    let total_handlers: usize = catalog
        .summaries()
        .iter()
        .filter(|s| !s.disabled)
        .filter_map(|s| catalog.load(&s.name).ok())
        .map(|loaded| {
            loaded
                .hooks
                .values()
                .map(|matchers| matchers.iter().map(|m| m.hooks.len()).sum::<usize>())
                .sum::<usize>()
        })
        .sum();
    // Note: catalog.load() results are cached, so the loads above and those
    // inside catalog_hook_issues() hit the skill-catalog cache.

    let errors = issues.iter().filter(|i| i.is_error).count();
    let notes = issues.iter().filter(|i| !i.is_error).count();
    let mut detail = format!("handlers={total_handlers}");
    if errors > 0 || notes > 0 {
        detail.push_str(&format!("; errors={errors} notes={notes}"));
        for issue in &issues {
            let kind = if issue.is_error { "error" } else { "note" };
            detail.push_str(&format!("; {kind}:{} {}", issue.skill, issue.message));
        }
    }
    let status = if errors > 0 {
        Status::Fail
    } else if notes > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    Some(Check {
        name: "hooks".to_string(),
        status,
        detail,
        extra: None,
    })
}

/// Returns `true` when `sh.exe` or `sh` is not found in any directory on
/// the current `PATH`. Uses filesystem `try_exists` probes so no child
/// process is spawned, keeping `skills_check` stat-only.
#[cfg(windows)]
fn which_sh_missing() -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        if dir.join("sh.exe").try_exists().unwrap_or(false)
            || dir.join("sh").try_exists().unwrap_or(false)
        {
            return false;
        }
    }
    true
}

/// Report resolved skill discovery roots with their configuration sources so
/// Linux users can understand which directories will be scanned.  Warns per
/// root when `HOME` is absent and that specific root resolved to a relative
/// path (rather than blanket-warning whenever `HOME` is missing, which can be
/// misleading when `XDG_DATA_HOME` provides absolute roots).
fn skills_roots_check(config: &AppConfig) -> Check {
    let s = &config.skills;
    let mut parts: Vec<String> = Vec::new();
    let mut any_relative = false;

    let push_root =
        |label: &str, path: &std::path::Path, parts: &mut Vec<String>, rel: &mut bool| {
            if path.is_relative() {
                *rel = true;
                parts.push(format!("{label}={} (relative!)", path.display()));
            } else {
                parts.push(format!("{label}={}", path.display()));
            }
        };

    push_root(
        "compat_user",
        &s.compat_user_dir,
        &mut parts,
        &mut any_relative,
    );
    push_root("user", &s.user_dir, &mut parts, &mut any_relative);

    if let Some(xdg) = &s.xdg_user_dir {
        push_root("xdg_user", xdg, &mut parts, &mut any_relative);
    }

    for extra in &s.extra_roots {
        push_root("extra_root", extra, &mut parts, &mut any_relative);
    }

    // Project-local roots (workspace-relative, always shown).
    parts.push(format!(
        "project_compat={}",
        config.workspace_root.join(".agents/skills").display()
    ));
    parts.push(format!(
        "project={}",
        config.workspace_root.join(".squeezy/skills").display()
    ));

    let detail = parts.join(" | ");

    if any_relative {
        return Check {
            name: "skills_roots".to_string(),
            status: Status::Warn,
            detail: format!(
                "One or more skill roots resolved to relative paths (HOME likely unset). \
                 Set HOME or override with SQUEEZY_SKILLS_USER_DIR / \
                 SQUEEZY_SKILLS_COMPAT_USER_DIR. Roots: {detail}"
            ),
            extra: None,
        };
    }

    Check {
        name: "skills_roots".to_string(),
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

/// Smoke-initialize every registered tree-sitter grammar and report the
/// results as a single `parser_health` doctor row.  Fails when any grammar
/// cannot be loaded (e.g. a musl/static-link regression on Linux).
///
/// Reported fields:
///   `target`   — compile-time target triple
///   `backends` — number of registered language backends
///   `ok`/`fail` — counts of grammars that parsed their fixture / failed
fn parser_health_check() -> Check {
    let target = env!("SQUEEZY_TARGET_TRIPLE");
    let results = smoke_all_languages();
    let total = results.len();
    let failures: Vec<_> = results.iter().filter(|r| !r.ok).collect();
    let fail_count = failures.len();
    let ok_count = total - fail_count;
    let backend_count = squeezy_core::LanguageFamily::all().len();

    if fail_count == 0 {
        Check {
            name: "parser_health".to_string(),
            status: Status::Ok,
            detail: format!(
                "target={target} backends={backend_count} grammars={ok_count}/{total} ok"
            ),
            extra: None,
        }
    } else {
        let names: Vec<String> = failures
            .iter()
            .map(|r| format!("{:?}", r.language))
            .collect();
        Check {
            name: "parser_health".to_string(),
            status: Status::Fail,
            detail: format!(
                "target={target} backends={backend_count} grammars={ok_count}/{total} ok; \
                 failed: {}",
                names.join(", ")
            ),
            extra: None,
        }
    }
}

/// Warn when the resolved settings path lives inside the workspace root.
///
/// On Windows this is a common first-run pitfall: when neither
/// `SQUEEZY_SETTINGS_PATH`, `%APPDATA%`, nor `$HOME` resolve, the code falls
/// back to `.squeezy/settings.toml` relative to the working directory.  That
/// can put API keys and other secrets inside a working tree that may be shared
/// or version-controlled.  The check is cross-platform so the same warning
/// fires on any OS where the fallback path is hit.
fn settings_location_check(config: &AppConfig) -> Check {
    let settings = default_settings_path();
    let workspace = &config.workspace_root;
    let is_repo_local = settings_path_is_repo_local(&settings, workspace);
    if is_repo_local {
        return Check {
            name: "settings_location".to_string(),
            status: Status::Warn,
            detail: format!(
                "settings resolved to a repo-local path ({}); \
                 user secrets may be inside a working tree. \
                 Set SQUEEZY_SETTINGS_PATH or ensure {} is writable.",
                settings.display(),
                if cfg!(target_os = "windows") {
                    "%APPDATA%"
                } else {
                    "$HOME"
                },
            ),
            extra: None,
        };
    }
    Check {
        name: "settings_location".to_string(),
        status: Status::Ok,
        detail: settings.display().to_string(),
        extra: None,
    }
}

fn settings_path_is_repo_local(settings: &Path, workspace: &Path) -> bool {
    let first_meaningful_component = settings
        .components()
        .find(|component| !matches!(component, std::path::Component::CurDir));
    if first_meaningful_component
        .map(|component| component.as_os_str() == ".squeezy")
        .unwrap_or(false)
    {
        return true;
    }

    let settings_norm = normalize_path_for_boundary_compare(settings);
    let workspace_norm = normalize_path_for_boundary_compare(workspace);

    settings_norm == workspace_norm
        || settings_norm
            .strip_prefix(&workspace_norm)
            .is_some_and(|rest| workspace_norm == "/" || rest.starts_with('/'))
}

fn normalize_path_for_boundary_compare(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    while value.ends_with('/') && value.len() > 1 {
        value.pop();
    }
    if cfg!(target_os = "windows") {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

/// Report terminal capabilities.
///
/// On Windows this is particularly useful because the ANSI virtual-terminal
/// processing mode must be explicitly enabled and legacy console hosts do not
/// support it.  On all platforms it confirms that stdout/stderr are connected
/// to a terminal and shows what terminal emulator/multiplexer is in use.
fn terminal_capability_check() -> Check {
    use std::io::IsTerminal as _;
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stderr_is_tty = std::io::stderr().is_terminal();

    let mut hints: Vec<String> = Vec::new();
    hints.push(format!(
        "stdout={} stderr={}",
        if stdout_is_tty { "tty" } else { "pipe" },
        if stderr_is_tty { "tty" } else { "pipe" },
    ));

    // Detect common terminal environments via environment variables.
    let term_program = env::var("TERM_PROGRAM").ok();
    let wt_session = env::var("WT_SESSION").is_ok(); // Windows Terminal
    let term = env::var("TERM").ok();
    let colorterm = env::var("COLORTERM").ok();

    if wt_session {
        hints.push("Windows Terminal (ConPTY)".to_string());
    } else if let Some(ref prog) = term_program {
        hints.push(format!("TERM_PROGRAM={prog}"));
    } else if let Some(ref t) = term {
        hints.push(format!("TERM={t}"));
    }

    if let Some(ref ct) = colorterm {
        hints.push(format!("COLORTERM={ct}"));
    }

    // On Windows, add a note about ANSI virtual-terminal processing availability.
    #[cfg(target_os = "windows")]
    {
        // We check whether the ENABLE_VIRTUAL_TERMINAL_PROCESSING console mode
        // bit appears likely by looking for known signals.  We cannot call
        // GetConsoleMode here without winapi, so we infer from environment:
        // WT_SESSION (Windows Terminal) always supports VTP; ConEmu/Cmder set
        // ConEmuANSI; legacy cmd.exe with no VTP enablement leaves these unset.
        let ansi_likely = wt_session
            || env::var("ConEmuANSI").is_ok()
            || env::var("ANSICON").is_ok()
            || term_program.is_some()
            || colorterm.is_some();
        hints.push(format!(
            "ansi_vt={}",
            if ansi_likely { "likely" } else { "unknown" }
        ));

        // Detect WSL vs native Windows vs Git Bash / MSYS2.
        if env::var("WSL_DISTRO_NAME").is_ok() || env::var("WSLENV").is_ok() {
            hints.push("WSL".to_string());
        } else if env::var("MSYSTEM").is_ok() {
            hints.push("Git Bash/MSYS2".to_string());
        }
    }

    Check {
        name: "terminal".to_string(),
        status: Status::Ok,
        detail: hints.join("; "),
        extra: None,
    }
}

/// Report the active shell-sandbox backend and configured mode/network. Delegates
/// to `squeezy_tools::shell_sandbox_doctor`, the single source of truth shared
/// with the runtime, and exposes structured JSON fields for Linux package smoke
/// tests without scraping prose.
fn sandbox_check(config: Option<&AppConfig>) -> Check {
    let report = squeezy_tools::shell_sandbox_doctor();
    let mut detail = format!("backend {}: {}", report.backend, report.detail);
    let mut extra = serde_json::Map::new();
    extra.insert(
        "backend".to_string(),
        serde_json::Value::String(report.backend.to_string()),
    );
    extra.insert(
        "required_mode_supported".to_string(),
        serde_json::Value::Bool(report.available),
    );

    if let Some(cfg) = config {
        let sb = &cfg.permissions.shell_sandbox;
        detail.push_str(&format!(
            "; configured mode={} network={}",
            sb.mode.as_str(),
            sb.network.as_str()
        ));
        extra.insert(
            "mode".to_string(),
            serde_json::Value::String(sb.mode.as_str().to_string()),
        );
        extra.insert(
            "network".to_string(),
            serde_json::Value::String(sb.network.as_str().to_string()),
        );
        // On Windows, surface the configured sandbox level so operators can
        // see when it differs from the doctor-reported backend. Notably,
        // `windows_sandbox_level=disabled` selects the job-object-only backend
        // at runtime, which is more limited than the restricted-token default.
        #[cfg(target_os = "windows")]
        {
            detail.push_str(&format!(
                " windows_sandbox_level={}",
                sb.windows_sandbox_level.as_str()
            ));
            extra.insert(
                "windows_sandbox_level".to_string(),
                serde_json::Value::String(sb.windows_sandbox_level.as_str().to_string()),
            );
        }
        // Explain squeezy ask socket availability under Linux direct-syscalls:
        // the seccomp filter denies AF_UNIX, so in-shell approval escalation
        // is unavailable. Show a note when the backend is active.
        if report.backend == "linux-direct-syscalls" && report.available {
            detail.push_str(
                "; note: squeezy ask (in-shell approval) is unavailable - seccomp denies AF_UNIX sockets under linux-direct-syscalls",
            );
        }
    }

    // Surface Linux-specific sandbox health fields for diagnostics.
    if let Some(userns) = report.linux_user_namespaces {
        detail.push_str(if userns {
            "; user-namespaces: available"
        } else {
            "; user-namespaces: unavailable"
        });
        extra.insert("userns".to_string(), serde_json::Value::Bool(userns));
    } else if let Some(userns) = report.userns {
        extra.insert("userns".to_string(), serde_json::Value::Bool(userns));
    }
    if let Some(landlock) = report.landlock {
        extra.insert("landlock".to_string(), serde_json::Value::Bool(landlock));
    }
    if let Some(abi) = report.linux_landlock_abi {
        if abi > 0 {
            detail.push_str(&format!("; landlock-abi: {abi}"));
        } else {
            detail.push_str("; landlock-abi: unavailable");
        }
        extra.insert("landlock_abi".to_string(), json!(abi));
    }
    if let Some(seccomp) = report.linux_seccomp_available {
        detail.push_str(if seccomp {
            "; seccomp: available"
        } else {
            "; seccomp: unavailable"
        });
        extra.insert("seccomp".to_string(), serde_json::Value::Bool(seccomp));
    }
    if let Some(blocked) = report.linux_ask_socket_blocked {
        if blocked {
            detail.push_str("; squeezy-ask-in-child: blocked (AF_UNIX denied by seccomp)");
        }
        extra.insert(
            "ask_socket_blocked".to_string(),
            serde_json::Value::Bool(blocked),
        );
    }
    if let Some(reason) = report.fallback_reason {
        extra.insert(
            "fallback_reason".to_string(),
            serde_json::Value::String(reason),
        );
    }

    Check {
        name: "sandbox".to_string(),
        status: if report.available {
            Status::Ok
        } else {
            Status::Warn
        },
        detail,
        extra: Some(serde_json::Value::Object(extra)),
    }
}

/// Detailed Linux sandbox posture check for `doctor --linux-sandbox`.
/// On Linux, reports user namespace support, Landlock support, seccomp
/// support, and required-mode viability. On other platforms, reports the
/// active backend with a note that Linux detail only applies on Linux.
fn linux_sandbox_detail_check() -> Check {
    let report = squeezy_tools::shell_sandbox_doctor();
    linux_sandbox_check_from_report(report)
}

#[cfg(target_os = "linux")]
fn linux_sandbox_check_from_report(report: squeezy_tools::ShellSandboxDoctor) -> Check {
    let detail = format!(
        "backend={} available={} - {}",
        report.backend, report.available, report.detail
    );
    // Use Fail (not Warn) so `--linux-sandbox` exits 1 on hosts where required
    // mode would fail - CI gates written as `squeezy doctor --linux-sandbox`
    // can rely on the exit code.
    Check {
        name: "linux-sandbox".to_string(),
        status: if report.available {
            Status::Ok
        } else {
            Status::Fail
        },
        detail,
        extra: None,
    }
}

#[cfg(not(target_os = "linux"))]
fn linux_sandbox_check_from_report(report: squeezy_tools::ShellSandboxDoctor) -> Check {
    Check {
        name: "linux-sandbox".to_string(),
        status: Status::Warn,
        detail: format!(
            "linux-sandbox detail is only available on Linux \
             (current platform: {}); active backend={}: {}",
            std::env::consts::OS,
            report.backend,
            report.detail
        ),
        extra: None,
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
            let Some(key) = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env) else {
                return (Status::Warn, probe_skip_missing_key(&c.api_key_env));
            };
            probe_openai_compatible(&client, &c.base_url, Some(key), None).await
        }
        ProviderConfig::Anthropic(c) => {
            // Anthropic added `GET /v1/models` in 2024; reuse the same shape
            // as OpenAI-compatible, but with the `x-api-key` header.
            let Some(key) = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env) else {
                return (Status::Warn, probe_skip_missing_key(&c.api_key_env));
            };
            probe_anthropic(&client, &c.base_url, Some(key)).await
        }
        ProviderConfig::Google(c) => {
            let Some(key) = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env) else {
                return (Status::Warn, probe_skip_missing_key(&c.api_key_env));
            };
            probe_google(&client, &c.base_url, Some(key)).await
        }
        ProviderConfig::AzureOpenAi(c) => {
            let Some(key) = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env) else {
                return (Status::Warn, probe_skip_missing_key(&c.api_key_env));
            };
            probe_azure_openai(&client, &c.base_url, &c.api_version, Some(key)).await
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
            let Some(key) = resolve_probe_key(c.api_key.as_deref(), &c.api_key_env) else {
                return (Status::Warn, probe_skip_missing_key(&c.api_key_env));
            };
            probe_openai_compatible(&client, &c.base_url, Some(key), Some(extra)).await
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

/// Detail for the live-probe skip row when no credential resolves.
/// Reuses [`missing_key_remediation`] so a scoped `--only probe:<provider>`
/// run still hands the user the same actionable fix the credential row
/// shows, instead of a dead-end "API key env var is unset".
fn probe_skip_missing_key(env_name: &str) -> String {
    format!("skipping probe: {}", missing_key_remediation(env_name))
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

// TODO: `squeezy cache move` — relocate state.redb, graph.redb, and session
// cache roots safely, with lock checks before moving and rollback on failure.
// See Category10_linux.md "Linux new features" / squeezy-cache-move.

// TODO: `squeezy cache verify` — open both redb stores, validate schema
// versions, count graph/resolver rows, and report whether backups are
// restorable or prune-only.
// See Category10_linux.md "Linux new features" / squeezy-cache-verify.

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
