use std::{env, fs, path::PathBuf};

use clap::Args;
use serde_json::json;
use squeezy_core::{AppConfig, ProviderConfig, Result};
use squeezy_store::{SessionStore, ensure_repo_profile};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
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
        if self.json {
            let body = json!({
                "version": self.version,
                "target": self.target,
                "ok": self.checks.iter().all(|c| c.status != Status::Fail),
                "warnings": self.checks.iter().filter(|c| c.status == Status::Warn).count(),
                "failures": self.checks.iter().filter(|c| c.status == Status::Fail).count(),
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
        } else if self.checks.iter().any(|c| c.status == Status::Warn) {
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

pub fn run(args: &DoctorArgs) -> Result<DoctorReport> {
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

        checks.push(session_store_check(config));
    }

    checks.push(sandbox_check());

    // Warnings (e.g. missing optional API keys, missing sandbox tool) print as
    // such but do not fail the command: smoke tests in CI / brew test run in
    // environments where keys are absent and still need the binary to come up
    // green. Only hard failures (config load broken, session store unwritable)
    // produce a non-zero exit, matching the old `--health` contract.
    let failures = checks.iter().filter(|c| c.status == Status::Fail).count();
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
            env_check(&c.api_key_env, c.api_key_keychain.as_deref()),
        ),
        ProviderConfig::Anthropic(c) => (
            "anthropic",
            env_check(&c.api_key_env, c.api_key_keychain.as_deref()),
        ),
        ProviderConfig::Google(c) => (
            "google",
            env_check(&c.api_key_env, c.api_key_keychain.as_deref()),
        ),
        ProviderConfig::AzureOpenAi(c) => (
            "azure_openai",
            env_check(&c.api_key_env, c.api_key_keychain.as_deref()),
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
        ProviderConfig::OpenAiCompatible(c) => (
            c.preset.as_str(),
            env_check(&c.api_key_env, c.api_key_keychain.as_deref()),
        ),
    }
}

fn env_check(env_name: &str, keychain: Option<&str>) -> (Status, String) {
    if env::var(env_name).is_ok() {
        return (Status::Ok, format!("{env_name} is set"));
    }
    if let Some(keychain) = keychain {
        return (
            Status::Warn,
            format!("{env_name} not set; will try keychain entry {keychain}"),
        );
    }
    (
        Status::Warn,
        format!("{env_name} not set (set it before starting a session)"),
    )
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

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
