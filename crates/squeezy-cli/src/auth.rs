use std::collections::BTreeMap;
use std::env;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use squeezy_core::{
    SeparatedSources, SqueezyError, load_separated_settings_sources,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits},
};
use squeezy_llm::{
    AnthropicLoginConfig, AnthropicOAuthSource, DEFAULT_POLICY_MODELS,
    GitHubCopilotDeviceCodeResponse, GitHubCopilotLoginHooks, GitHubCopilotLoginOutcome,
    OpenAiCodexLoginOutcome, PersistedTokens, codex_auth_file_path, exchange_authorization_code,
    generate_pkce, github_copilot_auth_file_path, github_copilot_read_tokens,
    login_github_copilot_interactive, login_openai_codex_interactive, normalize_github_domain,
    parse_authorization_input,
};
use tokio_util::sync::CancellationToken;

/// Every `[providers.<section>]` name that can carry an inline `api_key`,
/// paired with the CLI alias used in error messages. Order is the
/// canonical listing for `auth list` / `auth status` without a provider.
const KNOWN_PROVIDERS: &[KnownProvider] = &[
    KnownProvider {
        section: "openai",
        cli: "openai",
        env: "SQUEEZY_OPENAI_KEY",
        fallback_env: Some("OPENAI_API_KEY"),
    },
    KnownProvider {
        section: "anthropic",
        cli: "anthropic",
        env: "SQUEEZY_ANTHROPIC_KEY",
        fallback_env: Some("ANTHROPIC_API_KEY"),
    },
    KnownProvider {
        section: "google",
        cli: "google",
        env: "SQUEEZY_GOOGLE_KEY",
        fallback_env: Some("GOOGLE_API_KEY"),
    },
    KnownProvider {
        section: "azure_openai",
        cli: "azure",
        env: "SQUEEZY_AZURE_OPENAI_KEY",
        fallback_env: Some("AZURE_OPENAI_API_KEY"),
    },
    KnownProvider {
        section: "openrouter",
        cli: "openrouter",
        env: "SQUEEZY_OPENROUTER_KEY",
        fallback_env: Some("OPENROUTER_API_KEY"),
    },
    KnownProvider {
        section: "vercel",
        cli: "vercel",
        env: "SQUEEZY_VERCEL_KEY",
        fallback_env: Some("AI_GATEWAY_API_KEY"),
    },
    KnownProvider {
        section: "portkey",
        cli: "portkey",
        env: "SQUEEZY_PORTKEY_KEY",
        fallback_env: Some("PORTKEY_API_KEY"),
    },
    KnownProvider {
        section: "groq",
        cli: "groq",
        env: "SQUEEZY_GROQ_KEY",
        fallback_env: Some("GROQ_API_KEY"),
    },
    KnownProvider {
        section: "xai",
        cli: "xai",
        env: "SQUEEZY_XAI_KEY",
        fallback_env: Some("XAI_API_KEY"),
    },
    KnownProvider {
        section: "deepseek",
        cli: "deepseek",
        env: "SQUEEZY_DEEPSEEK_KEY",
        fallback_env: Some("DEEPSEEK_API_KEY"),
    },
    KnownProvider {
        section: "vertex",
        cli: "vertex",
        env: "SQUEEZY_VERTEX_KEY",
        fallback_env: Some("VERTEX_ACCESS_TOKEN"),
    },
    KnownProvider {
        section: "mistral",
        cli: "mistral",
        env: "SQUEEZY_MISTRAL_KEY",
        fallback_env: Some("MISTRAL_API_KEY"),
    },
    KnownProvider {
        section: "together",
        cli: "together",
        env: "SQUEEZY_TOGETHER_KEY",
        fallback_env: Some("TOGETHER_API_KEY"),
    },
    KnownProvider {
        section: "fireworks",
        cli: "fireworks",
        env: "SQUEEZY_FIREWORKS_KEY",
        fallback_env: Some("FIREWORKS_API_KEY"),
    },
    KnownProvider {
        section: "cerebras",
        cli: "cerebras",
        env: "SQUEEZY_CEREBRAS_KEY",
        fallback_env: Some("CEREBRAS_API_KEY"),
    },
    // Local self-hosted OpenAI-compatible servers. They typically run without
    // authentication on a loopback port; the inline-key slot exists so users
    // can stand up a reverse proxy that requires a bearer token.
    KnownProvider {
        section: "lmstudio",
        cli: "lmstudio",
        env: "SQUEEZY_LMSTUDIO_KEY",
        fallback_env: Some("LMSTUDIO_API_KEY"),
    },
    KnownProvider {
        section: "vllm",
        cli: "vllm",
        env: "SQUEEZY_VLLM_KEY",
        fallback_env: Some("VLLM_API_KEY"),
    },
    KnownProvider {
        section: "llamacpp",
        cli: "llamacpp",
        env: "SQUEEZY_LLAMACPP_KEY",
        fallback_env: Some("LLAMACPP_API_KEY"),
    },
    KnownProvider {
        section: "cloudflare_workers_ai",
        cli: "cloudflare_workers_ai",
        env: "SQUEEZY_CLOUDFLARE_WORKERS_AI_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    KnownProvider {
        section: "cloudflare_ai_gateway",
        cli: "cloudflare_ai_gateway",
        env: "SQUEEZY_CLOUDFLARE_AI_GATEWAY_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    KnownProvider {
        section: "openai_compatible",
        cli: "openai_compatible",
        env: "SQUEEZY_OPENAI_COMPATIBLE_KEY",
        fallback_env: None,
    },
];

#[derive(Debug, Clone, Copy)]
struct KnownProvider {
    section: &'static str,
    cli: &'static str,
    env: &'static str,
    fallback_env: Option<&'static str>,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Store a provider API key as inline `api_key` in the project-local TOML")]
    Set(AuthSetArgs),
    #[command(
        about = "List providers with a stored inline `api_key` across user and project TOMLs"
    )]
    List(AuthListArgs),
    #[command(about = "Remove the inline `api_key` for a provider from the project-local TOML")]
    Remove(AuthRemoveArgs),
    #[command(about = "Report which providers have a key (inline or env) and where it resolves")]
    Status(AuthStatusArgs),
    #[command(
        about = "Anthropic Claude Pro/Max OAuth login (subscription quota instead of an API key)"
    )]
    Anthropic {
        #[command(subcommand)]
        command: AnthropicOauthCommand,
    },
    #[command(
        name = "openai-codex",
        about = "Manage the ChatGPT Plus/Pro subscription (OpenAI Codex OAuth) token"
    )]
    OpenAiCodex {
        #[command(subcommand)]
        command: OpenAiCodexCommand,
    },
    #[command(
        name = "github-copilot",
        about = "GitHub Copilot subscription OAuth (device code) and per-model policy"
    )]
    GitHubCopilot {
        #[command(subcommand)]
        command: GitHubCopilotCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AnthropicOauthCommand {
    #[command(
        about = "Open the Claude.ai consent screen and persist the OAuth tokens for this user"
    )]
    Login(AnthropicLoginArgs),
    #[command(about = "Remove the persisted Anthropic OAuth tokens")]
    Logout,
    #[command(
        about = "Show whether Anthropic OAuth tokens are persisted and (roughly) when they expire"
    )]
    Status,
}

#[derive(Debug, Subcommand)]
pub enum OpenAiCodexCommand {
    #[command(about = "Run the OAuth login flow and persist the access/refresh tokens")]
    Login(OpenAiCodexLoginArgs),
    #[command(about = "Remove the persisted Codex token (sign out)")]
    Logout,
    #[command(about = "Show whether a Codex token is currently persisted")]
    Status,
}

#[derive(Debug, Subcommand)]
pub enum GitHubCopilotCommand {
    #[command(about = "Run the device-code OAuth flow and persist the Copilot Chat API tokens")]
    Login(GitHubCopilotLoginArgs),
    #[command(about = "Remove the persisted Copilot tokens (sign out)")]
    Logout,
    #[command(about = "Show whether Copilot tokens are persisted and how long they're valid")]
    Status,
}

#[derive(Debug, Args, Default)]
pub struct GitHubCopilotLoginArgs {
    /// Optional GitHub Enterprise hostname (e.g. `acme.ghe.com`). Leave
    /// empty for the standard `github.com` account.
    #[arg(long, help = "GitHub Enterprise hostname; leave unset for github.com")]
    pub enterprise_domain: Option<String>,
    /// Skip the best-effort browser launch and only print the
    /// verification URI. Useful in headless or SSH sessions.
    #[arg(long, help = "Do not try to launch a browser; just print the URL")]
    pub no_browser: bool,
    /// Skip the per-model policy-enablement step entirely. Off by
    /// default so a fresh login also flips the per-user "enabled"
    /// gates GitHub requires for the curated model list.
    #[arg(long, help = "Skip the post-login per-model policy POSTs")]
    pub skip_policy: bool,
    /// Override the model list whose policy gates get flipped after
    /// login. Comma-separated; defaults to [`DEFAULT_POLICY_MODELS`].
    #[arg(
        long,
        value_name = "MODELS",
        help = "Comma-separated model ids to enable (default: curated bundle)"
    )]
    pub models: Option<String>,
}

#[derive(Debug, Args, Default)]
pub struct AnthropicLoginArgs {
    /// Skip the best-effort `open`/`xdg-open` browser launch and only
    /// print the authorize URL. Useful in headless or SSH sessions.
    #[arg(long, help = "Do not try to launch a browser; just print the URL")]
    pub no_browser: bool,
}

#[derive(Debug, Args, Default)]
pub struct OpenAiCodexLoginArgs {
    /// Originator tag stamped in the OAuth authorize URL and on every
    /// Codex request. Defaults to `squeezy` so OpenAI can attribute
    /// traffic; override only if you know what you're doing.
    #[arg(long, help = "OAuth originator tag (default: squeezy)")]
    pub originator: Option<String>,
    /// Skip the automatic browser launch. The CLI prints the URL and
    /// waits for the callback — useful on headless hosts.
    #[arg(long, help = "Do not invoke a browser; print the URL only")]
    pub no_browser: bool,
}

#[derive(Debug, Args)]
pub struct AuthSetArgs {
    /// Provider id (openai, anthropic, google, azure, …, openrouter, portkey).
    pub provider: String,
    /// API key value. If omitted, read from stdin so it isn't captured in
    /// shell history.
    #[arg(long, help = "Inline API key value (otherwise read from stdin)")]
    pub value: Option<String>,
    /// Write to `~/.squeezy/settings.toml` instead of the project-local
    /// `~/.squeezy/projects/<slug>/settings.toml`. The committed repo
    /// `./squeezy.toml` is never a valid target — keys do not belong in
    /// version control.
    #[arg(
        long,
        help = "Save to the user-level settings TOML instead of project-local"
    )]
    pub user: bool,
}

#[derive(Debug, Args, Default)]
pub struct AuthListArgs {
    /// Emit the list as JSON for scripting.
    #[arg(long, help = "Emit the list as JSON")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AuthRemoveArgs {
    /// Provider id (openai, anthropic, google, azure, …, openrouter, portkey).
    pub provider: String,
    /// Remove from `~/.squeezy/settings.toml` instead of the project-local
    /// `~/.squeezy/projects/<slug>/settings.toml`.
    #[arg(
        long,
        help = "Edit the user-level settings TOML instead of project-local"
    )]
    pub user: bool,
}

#[derive(Debug, Args)]
pub struct AuthStatusArgs {
    /// Optional provider id. When omitted, every known provider is listed.
    pub provider: Option<String>,
    /// Emit the status as JSON for scripting.
    #[arg(long, help = "Emit the status as JSON")]
    pub json: bool,
}

pub async fn handle_auth_command(command: &AuthCommand) -> squeezy_core::Result<()> {
    match command {
        AuthCommand::Set(args) => handle_auth_set(args, read_api_key_from_stdin),
        AuthCommand::List(args) => handle_auth_list(args),
        AuthCommand::Remove(args) => handle_auth_remove(args),
        AuthCommand::Status(args) => handle_auth_status(args),
        AuthCommand::Anthropic { command } => handle_anthropic_oauth(command).await,
        AuthCommand::OpenAiCodex { command } => handle_openai_codex_command(command),
        AuthCommand::GitHubCopilot { command } => handle_github_copilot_command(command),
    }
}

fn handle_openai_codex_command(command: &OpenAiCodexCommand) -> squeezy_core::Result<()> {
    match command {
        OpenAiCodexCommand::Login(args) => handle_openai_codex_login(args),
        OpenAiCodexCommand::Logout => handle_openai_codex_logout(),
        OpenAiCodexCommand::Status => handle_openai_codex_status(),
    }
}

fn handle_openai_codex_login(args: &OpenAiCodexLoginArgs) -> squeezy_core::Result<()> {
    let auth_path = codex_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_OPENAI_CODEX_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    let originator = args
        .originator
        .clone()
        .unwrap_or_else(|| "squeezy".to_string());
    let no_browser = args.no_browser;

    // Construct a runtime explicitly so this stays runnable from the
    // non-async `handle_auth_command` entry point without changing
    // the trait signature.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| SqueezyError::Config(format!("tokio runtime build failed: {err}")))?;

    let auth_path_for_login = auth_path.clone();
    let outcome: OpenAiCodexLoginOutcome = runtime.block_on(async move {
        login_openai_codex_interactive(&originator, &auth_path_for_login, |url| {
            eprintln!("Open this URL in your browser to sign in to ChatGPT Plus/Pro:");
            eprintln!();
            eprintln!("    {url}");
            eprintln!();
            if no_browser {
                eprintln!("(--no-browser: not launching a browser; waiting for callback…)");
                return Ok(());
            }
            match open_browser(url) {
                Ok(()) => {
                    eprintln!("Browser launched. Waiting for callback…");
                }
                Err(err) => {
                    eprintln!("Could not launch browser ({err}). Open the URL manually.");
                }
            }
            Ok(())
        })
        .await
    })?;

    let expires_in = expires_in_human(outcome.expires_at_unix_ms);
    println!(
        "signed in to ChatGPT (account {}); token saved to {}{}",
        outcome.account_id,
        outcome.auth_file.display(),
        expires_in
            .map(|s| format!("; access token valid for ~{s}"))
            .unwrap_or_default()
    );
    Ok(())
}

fn handle_openai_codex_logout() -> squeezy_core::Result<()> {
    let auth_path = codex_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_OPENAI_CODEX_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    match std::fs::remove_file(&auth_path) {
        Ok(()) => {
            println!("removed codex token at {}", auth_path.display());
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("no codex token at {}", auth_path.display());
            Ok(())
        }
        Err(err) => Err(SqueezyError::Config(format!(
            "could not remove {}: {err}",
            auth_path.display()
        ))),
    }
}

fn handle_openai_codex_status() -> squeezy_core::Result<()> {
    let auth_path = codex_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_OPENAI_CODEX_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    if !auth_path.exists() {
        println!(
            "no codex token at {} — run `squeezy auth openai-codex login`",
            auth_path.display()
        );
        return Ok(());
    }
    match squeezy_llm::load_codex_token(&auth_path)? {
        Some(token) => {
            let expires_in = expires_in_human(token.expires_at_unix_ms);
            println!(
                "codex token present at {} for account {}{}",
                auth_path.display(),
                token.account_id,
                expires_in
                    .map(|s| format!(" (access token valid for ~{s})"))
                    .unwrap_or_else(
                        || " (access token already expired; will refresh on next use)".to_string()
                    )
            );
            Ok(())
        }
        None => {
            println!(
                "no codex token at {} — run `squeezy auth openai-codex login`",
                auth_path.display()
            );
            Ok(())
        }
    }
}

fn handle_github_copilot_command(command: &GitHubCopilotCommand) -> squeezy_core::Result<()> {
    match command {
        GitHubCopilotCommand::Login(args) => handle_github_copilot_login(args),
        GitHubCopilotCommand::Logout => handle_github_copilot_logout(),
        GitHubCopilotCommand::Status => handle_github_copilot_status(),
    }
}

fn handle_github_copilot_login(args: &GitHubCopilotLoginArgs) -> squeezy_core::Result<()> {
    let auth_path = github_copilot_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_GITHUB_COPILOT_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    let enterprise_domain = match args.enterprise_domain.as_deref() {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(normalize_github_domain(trimmed).ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "invalid GitHub Enterprise URL/domain `{trimmed}`"
                    ))
                })?)
            }
        }
        None => None,
    };

    // Owned policy-model strings, materialized outside the async block
    // so the `&[&str]` we hand to the login orchestrator borrows from a
    // value with a known lifetime.
    let policy_models_owned: Vec<String> = match args.models.as_deref() {
        Some(raw) => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => DEFAULT_POLICY_MODELS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };

    let no_browser = args.no_browser;
    let skip_policy = args.skip_policy;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| SqueezyError::Config(format!("tokio runtime build failed: {err}")))?;

    let on_device_code = move |device: &GitHubCopilotDeviceCodeResponse| {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "Open this URL to authorize squeezy on GitHub:");
        let _ = writeln!(stderr);
        let _ = writeln!(stderr, "    {}", device.verification_uri);
        let _ = writeln!(stderr);
        let _ = writeln!(stderr, "Enter the device code: {}", device.user_code);
        let _ = writeln!(stderr);
        let interval = device.interval.unwrap_or(5);
        let _ = writeln!(
            stderr,
            "(polling every ~{interval}s; the prompt expires in {expires}s)",
            expires = device.expires_in
        );
    };
    let on_browser_open = move |url: &str| {
        if no_browser {
            eprintln!("--no-browser: not launching a browser; waiting for authorization…");
            return;
        }
        match open_browser(url) {
            Ok(()) => eprintln!("Browser launched. Waiting for authorization…"),
            Err(err) => eprintln!(
                "Could not launch browser ({err}). Open the URL above manually and authorize the device code."
            ),
        }
    };
    let on_progress = move |message: &str| {
        eprintln!("{message}");
    };

    let outcome: GitHubCopilotLoginOutcome = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let model_refs: Vec<&str> = policy_models_owned.iter().map(String::as_str).collect();
        let hooks = GitHubCopilotLoginHooks {
            on_device_code: &on_device_code,
            on_browser_open: &on_browser_open,
            on_progress: &on_progress,
        };
        login_github_copilot_interactive(
            enterprise_domain.as_deref(),
            &auth_path,
            &hooks,
            &cancel,
            skip_policy,
            &model_refs,
        )
        .await
    })?;

    let expires_in = expires_in_human(outcome.expires_at_unix_ms);
    let domain_label = outcome.enterprise_domain.as_deref().unwrap_or("github.com");
    println!(
        "signed in to GitHub Copilot ({}); token saved to {}{}",
        domain_label,
        outcome.auth_file.display(),
        expires_in
            .map(|s| format!("; copilot token valid for ~{s}"))
            .unwrap_or_default()
    );
    if !outcome.policy_outcomes.is_empty() {
        let total = outcome.policy_outcomes.len();
        let enabled = outcome.policy_outcomes.iter().filter(|o| o.success).count();
        println!("models with policy enabled: {enabled}/{total}");
        for entry in &outcome.policy_outcomes {
            let label = if entry.success { "ok" } else { "skipped" };
            println!("  [{label}] {}", entry.model_id);
        }
    } else if skip_policy {
        println!("(--skip-policy: did not flip per-model policy gates)");
    }
    Ok(())
}

fn handle_github_copilot_logout() -> squeezy_core::Result<()> {
    let auth_path = github_copilot_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_GITHUB_COPILOT_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    match std::fs::remove_file(&auth_path) {
        Ok(()) => {
            println!("removed github-copilot tokens at {}", auth_path.display());
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("no github-copilot tokens at {}", auth_path.display());
            Ok(())
        }
        Err(err) => Err(SqueezyError::Config(format!(
            "could not remove {}: {err}",
            auth_path.display()
        ))),
    }
}

fn handle_github_copilot_status() -> squeezy_core::Result<()> {
    let auth_path = github_copilot_auth_file_path().ok_or_else(|| {
        SqueezyError::Config(
            "could not determine ~/.squeezy auth directory; \
             set SQUEEZY_GITHUB_COPILOT_AUTH_FILE or HOME"
                .to_string(),
        )
    })?;
    match github_copilot_read_tokens(&auth_path)? {
        Some(tokens) => {
            let domain = tokens.enterprise_domain.as_deref().unwrap_or("github.com");
            let expires_in = expires_in_human(tokens.expires_at_unix_ms);
            println!(
                "github-copilot token present at {} for {}{}",
                auth_path.display(),
                domain,
                expires_in
                    .map(|s| format!(" (copilot token valid for ~{s})"))
                    .unwrap_or_else(|| {
                        " (copilot token expired; will refresh on next request)".to_string()
                    })
            );
            println!(
                "  github_token: {}",
                redact_oauth_token(&tokens.github_token)
            );
            println!(
                "  copilot_token: {}",
                redact_oauth_token(&tokens.copilot_token)
            );
            Ok(())
        }
        None => {
            println!(
                "no github-copilot tokens at {} — run `squeezy auth github-copilot login`",
                auth_path.display()
            );
            Ok(())
        }
    }
}

fn expires_in_human(expires_at_unix_ms: u64) -> Option<String> {
    let expiry = UNIX_EPOCH.checked_add(Duration::from_millis(expires_at_unix_ms))?;
    let now = SystemTime::now();
    let remaining = expiry.duration_since(now).ok()?;
    let secs = remaining.as_secs();
    if secs < 60 {
        Some(format!("{secs}s"))
    } else if secs < 3600 {
        Some(format!("{}m", secs / 60))
    } else {
        Some(format!("{}h {}m", secs / 3600, (secs % 3600) / 60))
    }
}

/// Best-effort browser launcher. Each platform has its own CLI:
/// `open` on macOS, `xdg-open` on Linux, `cmd /C start` on Windows.
/// Falls back to an explicit error so the caller can print the URL
/// for the user to open manually.
fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).status()?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).status()?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no browser launcher for this platform",
        ))
    }
}

fn read_api_key_from_stdin() -> squeezy_core::Result<String> {
    if io::stdin().is_terminal() {
        eprint!("API key: ");
        let _ = io::stderr().flush();
    }
    let mut reader = BufReader::new(io::stdin());
    let mut buffer = String::new();
    reader
        .read_line(&mut buffer)
        .map_err(|err| SqueezyError::Config(format!("failed to read API key from stdin: {err}")))?;
    Ok(buffer.trim().to_string())
}

/// Map a CLI-supplied provider id to its `[providers.<section>]` TOML
/// section name. Bedrock and Ollama have no single inline key — we surface
/// that as an actionable error instead of writing nothing.
pub(crate) fn provider_section_for(provider: &str) -> squeezy_core::Result<&'static str> {
    match provider {
        "openai" => Ok("openai"),
        "anthropic" | "claude" => Ok("anthropic"),
        "google" | "gemini" => Ok("google"),
        "azure" | "azure-openai" | "azure_openai" => Ok("azure_openai"),
        "bedrock" => Err(SqueezyError::Config(
            "bedrock uses the AWS default credential chain; configure credentials with aws configure"
                .to_string(),
        )),
        "ollama" | "local" => Err(SqueezyError::Config(
            "ollama runs locally and does not require an API key".to_string(),
        )),
        // OpenAI-compatible presets reuse the same name as both the CLI
        // provider id and the TOML section, so pass them through.
        other => Ok(static_section_name(other)),
    }
}

fn static_section_name(provider: &str) -> &'static str {
    // The set of OpenAI-compatible preset names is closed; resolve each to
    // a literal &'static str instead of leaking the heap string.
    match provider {
        "openrouter" => "openrouter",
        "vercel" => "vercel",
        "portkey" => "portkey",
        "groq" => "groq",
        "xai" => "xai",
        "deepseek" => "deepseek",
        "vertex" => "vertex",
        "mistral" => "mistral",
        "together" => "together",
        "fireworks" => "fireworks",
        "cerebras" => "cerebras",
        "lmstudio" => "lmstudio",
        "vllm" => "vllm",
        "llamacpp" => "llamacpp",
        "openai_compatible" => "openai_compatible",
        // Last-resort: fail closed if the caller passed something we
        // can't statically map. A future provider should be wired into
        // this match rather than silently leaking memory.
        _ => "",
    }
}

pub(crate) fn handle_auth_set(
    args: &AuthSetArgs,
    read_stdin: impl Fn() -> squeezy_core::Result<String>,
) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let target_path = if args.user {
        sources.user_path_default
    } else {
        sources.repo_path_default
    };
    handle_auth_set_at_path(args, target_path, args.user, read_stdin)
}

/// Test-friendly variant: the path resolution is hoisted to the caller so
/// unit tests can point at a tempdir.
pub(crate) fn handle_auth_set_at_path(
    args: &AuthSetArgs,
    target_path: PathBuf,
    user_scope: bool,
    read_stdin: impl Fn() -> squeezy_core::Result<String>,
) -> squeezy_core::Result<()> {
    let section = provider_section_for(&args.provider)?;
    if section.is_empty() {
        return Err(SqueezyError::Config(format!(
            "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
            args.provider
        )));
    }
    let value = match &args.value {
        Some(value) => value.clone(),
        None => read_stdin()?,
    };
    if value.trim().is_empty() {
        return Err(SqueezyError::Config(
            "API key must not be empty".to_string(),
        ));
    }
    let scope_target = if user_scope {
        SettingsScope::user(target_path.clone())
    } else {
        SettingsScope::repo(target_path.clone())
    };
    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: section.to_string(),
            fields: vec![("api_key", EditOp::SetString(value.trim().to_string()))],
        },
    };
    apply_edits(&scope_target, &[edit]).map_err(|err| {
        SqueezyError::Config(format!("failed to write {}: {err}", target_path.display()))
    })?;
    println!(
        "saved api key for {} to {}",
        args.provider,
        target_path.display()
    );
    Ok(())
}

fn handle_auth_list(args: &AuthListArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let entries = collect_inline_keys(&sources);
    if args.json {
        let json = serde_json::to_string_pretty(&entries.to_json())
            .map_err(|err| SqueezyError::Config(format!("failed to serialize auth list: {err}")))?;
        println!("{json}");
    } else {
        print_inline_keys_table(&entries);
    }
    Ok(())
}

fn handle_auth_remove(args: &AuthRemoveArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let target_path = if args.user {
        sources.user_path_default
    } else {
        sources.repo_path_default
    };
    handle_auth_remove_at_path(args, target_path, args.user)
}

pub(crate) fn handle_auth_remove_at_path(
    args: &AuthRemoveArgs,
    target_path: PathBuf,
    user_scope: bool,
) -> squeezy_core::Result<()> {
    let section = provider_section_for(&args.provider)?;
    if section.is_empty() {
        return Err(SqueezyError::Config(format!(
            "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
            args.provider
        )));
    }
    if !tier_has_inline_key(&target_path, section) {
        return Err(SqueezyError::Config(format!(
            "no inline api_key for {} found in {}",
            args.provider,
            target_path.display()
        )));
    }
    let scope_target = if user_scope {
        SettingsScope::user(target_path.clone())
    } else {
        SettingsScope::repo(target_path.clone())
    };
    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: section.to_string(),
            fields: vec![("api_key", EditOp::Unset)],
        },
    };
    apply_edits(&scope_target, &[edit]).map_err(|err| {
        SqueezyError::Config(format!("failed to write {}: {err}", target_path.display()))
    })?;
    println!(
        "removed api key for {} from {}",
        args.provider,
        target_path.display()
    );
    Ok(())
}

fn handle_auth_status(args: &AuthStatusArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    handle_auth_status_with_env(args, &sources, &|name| env::var(name).ok())
}

pub(crate) fn handle_auth_status_with_env(
    args: &AuthStatusArgs,
    sources: &SeparatedSources,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> squeezy_core::Result<()> {
    let rows: Vec<StatusRow> = match &args.provider {
        Some(provider) => {
            let section = provider_section_for(provider)?;
            if section.is_empty() {
                return Err(SqueezyError::Config(format!(
                    "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
                    provider
                )));
            }
            let known = KNOWN_PROVIDERS
                .iter()
                .find(|p| p.section == section)
                .copied()
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "no status view for provider {}; unknown section {}",
                        provider, section
                    ))
                })?;
            vec![compute_status_row(known, sources, env_lookup)]
        }
        None => KNOWN_PROVIDERS
            .iter()
            .copied()
            .map(|p| compute_status_row(p, sources, env_lookup))
            .collect(),
    };
    if args.json {
        let json =
            serde_json::to_string_pretty(&rows.iter().map(StatusRow::to_json).collect::<Vec<_>>())
                .map_err(|err| {
                    SqueezyError::Config(format!("failed to serialize status: {err}"))
                })?;
        println!("{json}");
    } else {
        print_status_table(&rows);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct InlineKeyEntry {
    provider: String,
    tier: TierLabel,
    path: PathBuf,
    redacted: String,
}

#[derive(Debug, Clone, Copy)]
enum TierLabel {
    User,
    Project,
    Repo,
}

impl TierLabel {
    fn as_str(self) -> &'static str {
        match self {
            TierLabel::User => "user",
            TierLabel::Project => "project",
            TierLabel::Repo => "local",
        }
    }
}

#[derive(Debug, Default)]
struct InlineKeyList {
    entries: Vec<InlineKeyEntry>,
}

impl InlineKeyList {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "provider": entry.provider,
                        "tier": entry.tier.as_str(),
                        "path": entry.path.display().to_string(),
                        "redacted": entry.redacted,
                    })
                })
                .collect(),
        )
    }
}

fn collect_inline_keys(sources: &SeparatedSources) -> InlineKeyList {
    let mut entries: Vec<InlineKeyEntry> = Vec::new();
    let tiers: [(Option<&squeezy_core::TierSource>, TierLabel); 3] = [
        (sources.user.as_ref(), TierLabel::User),
        (sources.project.as_ref(), TierLabel::Project),
        (sources.repo.as_ref(), TierLabel::Repo),
    ];
    for (tier, label) in tiers {
        let Some(tier) = tier else { continue };
        let inline = extract_inline_keys_from_doc(&tier.doc);
        for (section, value) in inline {
            entries.push(InlineKeyEntry {
                provider: section,
                tier: label,
                path: tier.path.clone(),
                redacted: redact_secret(&value),
            });
        }
    }
    InlineKeyList { entries }
}

fn extract_inline_keys_from_doc(doc: &toml_edit::DocumentMut) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let providers = match doc.as_table().get("providers") {
        Some(toml_edit::Item::Table(t)) => t,
        _ => return out,
    };
    for (section_name, item) in providers.iter() {
        // A provider section may be a [providers.foo] table or an inline
        // `foo = { api_key = "..." }` table value; treat both shapes.
        if let Some(value) = inline_api_key_value(item)
            && !value.trim().is_empty()
        {
            out.insert(section_name.to_string(), value.to_string());
        }
    }
    out
}

fn inline_api_key_value(item: &toml_edit::Item) -> Option<&str> {
    match item {
        toml_edit::Item::Table(t) => match t.get("api_key") {
            Some(toml_edit::Item::Value(toml_edit::Value::String(s))) => Some(s.value()),
            _ => None,
        },
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => match t.get("api_key") {
            Some(toml_edit::Value::String(s)) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }
}

fn doc_has_inline_key(doc: &toml_edit::DocumentMut, section: &str) -> bool {
    let Some(toml_edit::Item::Table(providers)) = doc.as_table().get("providers") else {
        return false;
    };
    providers
        .get(section)
        .and_then(inline_api_key_value)
        .is_some_and(|value| !value.trim().is_empty())
}

fn tier_has_inline_key(path: &Path, section: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc_has_inline_key(&doc, section)
}

fn redact_secret(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let len = trimmed.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let prefix: String = trimmed.chars().take(4).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn print_inline_keys_table(list: &InlineKeyList) {
    if list.entries.is_empty() {
        println!("No inline provider api_key entries found in user, project, or local settings.");
        return;
    }
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(list.entries.len() + 1);
    rows.push([
        "PROVIDER".to_string(),
        "TIER".to_string(),
        "KEY".to_string(),
        "PATH".to_string(),
    ]);
    for entry in &list.entries {
        rows.push([
            entry.provider.clone(),
            entry.tier.as_str().to_string(),
            entry.redacted.clone(),
            entry.path.display().to_string(),
        ]);
    }
    print_table_rows(&rows);
}

// Detail about where one provider's key resolves: which tier (if any)
// holds the inline value, and which env var (if any) is set. The CLI
// surface shows the highest-priority source first; the JSON form keeps
// every signal so scripts can decide for themselves.
#[derive(Debug, Clone)]
struct StatusRow {
    provider: &'static str,
    section: &'static str,
    inline_tier: Option<TierLabel>,
    inline_path: Option<PathBuf>,
    env_var: &'static str,
    env_set: bool,
    fallback_env_var: Option<&'static str>,
    fallback_env_set: bool,
}

impl StatusRow {
    fn effective_source(&self) -> &'static str {
        if self.inline_tier.is_some() {
            "inline"
        } else if self.env_set {
            "env"
        } else if self.fallback_env_set {
            "env (fallback)"
        } else {
            "missing"
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "section": self.section,
            "inline_tier": self.inline_tier.map(|t| t.as_str()),
            "inline_path": self.inline_path.as_ref().map(|p| p.display().to_string()),
            "env_var": self.env_var,
            "env_set": self.env_set,
            "fallback_env_var": self.fallback_env_var,
            "fallback_env_set": self.fallback_env_set,
            "effective_source": self.effective_source(),
        })
    }
}

fn compute_status_row(
    provider: KnownProvider,
    sources: &SeparatedSources,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> StatusRow {
    let tiers: [(Option<&squeezy_core::TierSource>, TierLabel); 3] = [
        // Highest precedence first: repo (per-machine local), then
        // project (./squeezy.toml), then user (~/.squeezy/settings.toml).
        // Matches `load_settings_from_paths` merge order; the last tier
        // to write `api_key` wins, so we report that tier here.
        (sources.repo.as_ref(), TierLabel::Repo),
        (sources.project.as_ref(), TierLabel::Project),
        (sources.user.as_ref(), TierLabel::User),
    ];
    let mut inline_tier = None;
    let mut inline_path = None;
    for (tier, label) in tiers {
        let Some(tier) = tier else { continue };
        if doc_has_inline_key(&tier.doc, provider.section) {
            inline_tier = Some(label);
            inline_path = Some(tier.path.clone());
            break;
        }
    }
    let env_set = env_lookup(provider.env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let fallback_env_set = provider
        .fallback_env
        .and_then(env_lookup)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    StatusRow {
        provider: provider.cli,
        section: provider.section,
        inline_tier,
        inline_path,
        env_var: provider.env,
        env_set,
        fallback_env_var: provider.fallback_env,
        fallback_env_set,
    }
}

fn print_status_table(rows: &[StatusRow]) {
    if rows.is_empty() {
        println!("No providers to report.");
        return;
    }
    let mut grid: Vec<[String; 4]> = Vec::with_capacity(rows.len() + 1);
    grid.push([
        "PROVIDER".to_string(),
        "SOURCE".to_string(),
        "ENV".to_string(),
        "INLINE".to_string(),
    ]);
    for row in rows {
        let env_cell = if row.env_set {
            format!("{} (set)", row.env_var)
        } else if let Some(fallback) = row.fallback_env_var
            && row.fallback_env_set
        {
            format!("{} (fallback set)", fallback)
        } else {
            row.env_var.to_string()
        };
        let inline_cell = match (&row.inline_tier, &row.inline_path) {
            (Some(tier), Some(path)) => format!("{} ({})", tier.as_str(), path.display()),
            _ => "-".to_string(),
        };
        grid.push([
            row.provider.to_string(),
            row.effective_source().to_string(),
            env_cell,
            inline_cell,
        ]);
    }
    print_table_rows(&grid);
}

fn print_table_rows(rows: &[[String; 4]]) {
    let widths: Vec<usize> = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        );
    }
}

// --- Anthropic OAuth subcommand --------------------------------------------

async fn handle_anthropic_oauth(command: &AnthropicOauthCommand) -> squeezy_core::Result<()> {
    match command {
        AnthropicOauthCommand::Login(args) => run_anthropic_oauth_login(args).await,
        AnthropicOauthCommand::Logout => run_anthropic_oauth_logout(),
        AnthropicOauthCommand::Status => run_anthropic_oauth_status(),
    }
}

async fn run_anthropic_oauth_login(args: &AnthropicLoginArgs) -> squeezy_core::Result<()> {
    let storage_path = squeezy_llm::oauth_anthropic_default_storage_path()?;
    let config = AnthropicLoginConfig::default();
    let codes = generate_pkce()?;
    let authorize_url = config.authorize_url(&codes);

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Squeezy Anthropic OAuth login")?;
    writeln!(
        stdout,
        "  Tokens will be stored at: {}",
        storage_path.display()
    )?;
    writeln!(stdout, "  Authorize URL:")?;
    writeln!(stdout, "    {authorize_url}")?;
    if !args.no_browser {
        if try_open_browser(&authorize_url) {
            writeln!(stdout, "  Opened the authorize URL in your browser.")?;
        } else {
            writeln!(
                stdout,
                "  Could not auto-open a browser; copy the URL above by hand."
            )?;
        }
    } else {
        writeln!(stdout, "  --no-browser: not launching a browser.")?;
    }
    writeln!(
        stdout,
        "  After approving the consent screen, copy the redirect URL (or the code) here."
    )?;
    stdout.flush()?;
    drop(stdout);

    let raw_input = read_authorization_input()?;
    let parsed = parse_authorization_input(&raw_input);
    let code = parsed.code.ok_or_else(|| {
        SqueezyError::Config(
            "no `code` parameter found in the pasted input; paste either the full \
             callback URL (http://localhost:54545/callback?code=...&state=...), \
             the bare code, or a `code#state` pair"
                .to_string(),
        )
    })?;
    let state = parsed.state.unwrap_or_else(|| codes.verifier.clone());
    if state != codes.verifier {
        return Err(SqueezyError::Config(
            "OAuth state mismatch: the pasted `state` parameter did not match the value \
             squeezy generated for this login. Re-run `squeezy auth anthropic login` from \
             the same terminal session."
                .to_string(),
        ));
    }

    let http = reqwest::Client::new();
    let response =
        exchange_authorization_code(&http, &config, &code, &state, &codes.verifier).await?;
    let now_ms = current_unix_ms();
    let tokens = PersistedTokens::from_token_response(&response, now_ms);
    squeezy_llm::oauth_anthropic_write_tokens(&storage_path, &tokens)?;
    println!(
        "Saved Anthropic OAuth tokens to {} (mode 0600).",
        storage_path.display()
    );
    Ok(())
}

fn run_anthropic_oauth_logout() -> squeezy_core::Result<()> {
    let path = squeezy_llm::oauth_anthropic_default_storage_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("removed {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("no Anthropic OAuth tokens at {}", path.display());
            Ok(())
        }
        Err(err) => Err(SqueezyError::Config(format!(
            "failed to remove {}: {err}",
            path.display()
        ))),
    }
}

fn run_anthropic_oauth_status() -> squeezy_core::Result<()> {
    let path = squeezy_llm::oauth_anthropic_default_storage_path()?;
    match squeezy_llm::oauth_anthropic_read_tokens(&path)? {
        None => {
            println!(
                "no Anthropic OAuth tokens at {}; run `squeezy auth anthropic login`",
                path.display()
            );
            Ok(())
        }
        Some(tokens) => {
            println!("anthropic oauth: configured");
            println!("  path: {}", path.display());
            println!(
                "  access_token: {}",
                redact_oauth_token(&tokens.access_token)
            );
            println!(
                "  refresh_token: {}",
                redact_oauth_token(&tokens.refresh_token)
            );
            let now = current_unix_ms();
            let label = if tokens.expires_at_unix_ms <= now {
                "expired".to_string()
            } else {
                let secs = (tokens.expires_at_unix_ms - now) / 1000;
                let minutes = secs / 60;
                format!("expires in {minutes}m ({secs}s)")
            };
            println!("  expiry: {label}");
            if let Some(scope) = tokens.scope.as_deref() {
                println!("  scope: {scope}");
            }
            // Surface the AnthropicOAuthSource label without exposing the
            // token so a follow-up `doctor` step can shell out to this
            // command for diagnostics.
            let _ = AnthropicOAuthSource::from_tokens(tokens, path.clone());
            Ok(())
        }
    }
}

fn redact_oauth_token(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let prefix: String = trimmed.chars().take(12).collect();
    format!("{prefix}…")
}

fn read_authorization_input() -> squeezy_core::Result<String> {
    if io::stdin().is_terminal() {
        eprint!("Paste authorization code or full redirect URL: ");
        let _ = io::stderr().flush();
    }
    let mut reader = BufReader::new(io::stdin());
    let mut buffer = String::new();
    reader.read_line(&mut buffer).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to read OAuth authorization input from stdin: {err}"
        ))
    })?;
    Ok(buffer)
}

/// Best-effort browser launch. Returns `false` when no launcher
/// succeeded — the caller falls back to printing the URL for manual
/// copy-paste. Honors `SQUEEZY_OAUTH_BROWSER` for headless tests so
/// they can confirm the URL was emitted without spawning a process.
fn try_open_browser(url: &str) -> bool {
    if let Ok(override_cmd) = env::var("SQUEEZY_OAUTH_BROWSER")
        && override_cmd.trim() == "0"
    {
        return false;
    }
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        // `cmd /c start "" <url>` is the canonical Windows shell launcher;
        // the empty title string keeps `start` from swallowing the URL as
        // the window title.
        &[("cmd", &["/c", "start", ""])]
    } else {
        // Most Linux desktops + BSDs ship xdg-open; fall back to a couple
        // of common alternates so an unusual installation still works.
        &[
            ("xdg-open", &[]),
            ("gio", &["open"]),
            ("sensible-browser", &[]),
        ]
    };
    for (cmd, extra_args) in candidates {
        let mut command = Command::new(cmd);
        for arg in *extra_args {
            command.arg(arg);
        }
        command.arg(url);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        if command.status().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
