use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use squeezy_core::SqueezyError;
use squeezy_llm::{
    AnthropicLoginConfig, AnthropicOAuthSource, DEFAULT_POLICY_MODELS,
    GitHubCopilotDeviceCodeResponse, GitHubCopilotLoginHooks, GitHubCopilotLoginOutcome,
    OpenAiCodexLoginOutcome, PersistedTokens, codex_auth_file_path, exchange_authorization_code,
    generate_pkce, github_copilot_auth_file_path, github_copilot_read_tokens,
    login_github_copilot_interactive, login_openai_codex_manual,
    login_openai_codex_with_auto_fallback, normalize_github_domain, parse_authorization_input,
};
use tokio_util::sync::CancellationToken;

use super::browser::{is_headless_linux, open_browser, try_open_browser};
use super::rendering::{expires_in_human, redact_oauth_token};
use super::{
    AnthropicLoginArgs, AnthropicOauthCommand, GitHubCopilotCommand, GitHubCopilotLoginArgs,
    OpenAiCodexCommand, OpenAiCodexLoginArgs,
};

pub(super) async fn handle_openai_codex_command(
    command: &OpenAiCodexCommand,
) -> squeezy_core::Result<()> {
    match command {
        OpenAiCodexCommand::Login(args) => handle_openai_codex_login(args).await,
        OpenAiCodexCommand::Logout => handle_openai_codex_logout(),
        OpenAiCodexCommand::Status => handle_openai_codex_status(),
    }
}

async fn handle_openai_codex_login(args: &OpenAiCodexLoginArgs) -> squeezy_core::Result<()> {
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
    let manual = args.manual;

    let auth_path_for_login = auth_path.clone();

    // Shared paste-input reader: used by both the --manual path and the
    // auto-fallback path inside login_openai_codex_with_auto_fallback.
    let read_paste_input = || {
        eprint!("Paste redirect URL or code: ");
        let _ = io::stderr().flush();
        let mut buf = String::new();
        BufReader::new(io::stdin())
            .read_line(&mut buf)
            .map_err(|err| {
                SqueezyError::Config(format!("failed to read authorization input: {err}"))
            })?;
        Ok(buf)
    };

    let outcome: OpenAiCodexLoginOutcome = if manual {
        // --manual: skip the localhost listener entirely and accept a
        // pasted redirect URL or bare code. Useful on Windows when the
        // firewall or browser isolation blocks 127.0.0.1:1455.
        login_openai_codex_manual(
            &originator,
            &auth_path_for_login,
            |url| {
                eprintln!("Open this URL in your browser to sign in to ChatGPT Plus/Pro:");
                eprintln!();
                eprintln!("    {url}");
                eprintln!();
                eprintln!("After approving the consent screen, paste the full redirect URL");
                eprintln!("  (http://localhost:1455/auth/callback?code=…&state=…) or the");
                eprintln!("  bare authorization code here:");
                if !no_browser {
                    if !try_open_browser(url) {
                        eprintln!("Could not launch browser. Copy the URL above.");
                    }
                } else {
                    eprintln!("(--no-browser: not launching a browser)");
                }
                Ok(())
            },
            read_paste_input,
        )
        .await?
    } else {
        // Interactive path with automatic in-session fallback:
        // - Show the URL and try to open a browser.
        // - Bind 127.0.0.1:1455 and wait for the OAuth callback.
        // - On port bind failure or timeout, fall back to the paste flow
        //   in the same invocation, reusing the same URL so the user
        //   does not need to start over.
        login_openai_codex_with_auto_fallback(
            &originator,
            &auth_path_for_login,
            |url| {
                eprintln!("Open this URL in your browser to sign in to ChatGPT Plus/Pro:");
                eprintln!();
                eprintln!("    {url}");
                eprintln!();
                eprintln!(
                    "Squeezy is listening for the OAuth callback on \
                     http://localhost:1455/auth/callback"
                );
                #[cfg(target_os = "windows")]
                eprintln!(
                    "(Windows: if your browser or firewall blocks localhost callbacks, \
                     squeezy will automatically fall back to the paste flow)"
                );
                if no_browser {
                    eprintln!("(--no-browser: not launching a browser; waiting for callback…)");
                    return Ok(());
                }
                if !try_open_browser(url) {
                    eprintln!("Could not launch browser. Open the URL above manually.");
                    eprintln!(
                        "Waiting for the callback on port 1455, or it will fall back \
                         to the paste flow after {} seconds.",
                        squeezy_llm::OPENAI_CODEX_INTERACTIVE_LOGIN_TIMEOUT_SECS
                    );
                } else {
                    eprintln!("Browser launched. Waiting for callback…");
                }
                Ok(())
            },
            |url, reason| {
                eprintln!();
                eprintln!(
                    "Falling back to paste flow ({reason}). \
                     Open this authorize URL in your browser if it is not already open:"
                );
                eprintln!("    {url}");
                eprintln!();
                eprintln!("After approving, paste the full redirect URL or bare code here:");
            },
            read_paste_input,
        )
        .await?
    };

    let expires_in = expires_in_human(outcome.expires_at_unix_ms);
    #[cfg(unix)]
    let protection_note = " (mode 0600)";
    #[cfg(not(unix))]
    let protection_note = " (file-backed; verify ACLs on shared/enterprise profiles)";
    println!(
        "signed in to ChatGPT (account {}); token saved to {}{}{}",
        outcome.account_id,
        outcome.auth_file.display(),
        expires_in
            .map(|s| format!("; access token valid for ~{s}"))
            .unwrap_or_default(),
        protection_note
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

pub(super) fn handle_github_copilot_command(
    command: &GitHubCopilotCommand,
) -> squeezy_core::Result<()> {
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
        if is_headless_linux() {
            eprintln!(
                "(headless Linux: no DISPLAY or WAYLAND_DISPLAY detected; \
                 open the URL above in a browser on another machine and enter the device code)"
            );
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

// --- Anthropic OAuth subcommand --------------------------------------------

pub(super) async fn handle_anthropic_oauth(
    command: &AnthropicOauthCommand,
) -> squeezy_core::Result<()> {
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
        if is_headless_linux() {
            writeln!(
                stdout,
                "  (headless Linux: no DISPLAY or WAYLAND_DISPLAY detected; \
                 open the URL above in a browser on another machine)"
            )?;
        } else if try_open_browser(&authorize_url) {
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
    #[cfg(unix)]
    println!(
        "Saved Anthropic OAuth tokens to {} (mode 0600).",
        storage_path.display()
    );
    #[cfg(not(unix))]
    println!(
        "Saved Anthropic OAuth tokens to {} \
         (file-backed; verify ACLs on shared or enterprise-managed profiles).",
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

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
