use std::io::{self, BufRead, BufReader, IsTerminal, Write};

use clap::{Args, Subcommand};
use squeezy_core::SqueezyError;
#[cfg(test)]
use squeezy_core::{ProviderAuthMeta, provider_auth_metadata};

mod browser;
mod flows;
mod provider;
mod rendering;

pub(crate) use browser::is_headless_linux;
use flows::{handle_anthropic_oauth, handle_github_copilot_command, handle_openai_codex_command};
pub(crate) use provider::{
    collect_inline_keys, handle_auth_remove_at_path, handle_auth_set_at_path,
    handle_auth_status_with_env, provider_section_for,
};
use provider::{handle_auth_list, handle_auth_remove, handle_auth_set, handle_auth_status};

#[cfg(test)]
static KNOWN_PROVIDERS: std::sync::LazyLock<Vec<ProviderAuthMeta>> =
    std::sync::LazyLock::new(provider_auth_metadata);

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
    /// login. Comma-separated; defaults to the curated Copilot policy model bundle.
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
    /// Skip the localhost callback server entirely and accept the full
    /// redirect URL or bare authorization code pasted from the browser.
    /// Use this on locked-down Windows desktops where firewall rules,
    /// browser isolation, or enterprise endpoint software blocks the
    /// localhost callback, or in any environment where binding
    /// 127.0.0.1:1455 is not possible.
    #[arg(
        long,
        help = "Skip localhost callback; paste redirect URL or code instead (Windows/locked-down)"
    )]
    pub manual: bool,
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
        AuthCommand::OpenAiCodex { command } => handle_openai_codex_command(command).await,
        AuthCommand::GitHubCopilot { command } => handle_github_copilot_command(command),
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

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
