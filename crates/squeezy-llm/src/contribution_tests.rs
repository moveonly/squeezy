use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use squeezy_core::Result;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{LlmProvider, LlmRequest, LlmStream};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_path(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-contribution-{}-{}-{}",
        prefix,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("settings.toml")
}

// ---- A throwaway provider that only exists to verify the trait wiring.
//
// `EchoProvider` is `Send + Sync` and ignores all requests; the registry
// only cares that it can be type-erased into `Arc<dyn LlmProvider>` and
// that its identifying fields survive the round-trip through the
// contribution + TOML pipeline.

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "fields exercise the config round-trip; not read by name"
)]
struct EchoProvider {
    base_url: String,
    api_key_env: String,
}

impl LlmProvider for EchoProvider {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        unreachable!("EchoProvider is only used to exercise the registry plumbing")
    }
}

#[derive(Debug, Deserialize)]
struct EchoConfig {
    api_key_env: String,
    #[serde(default = "default_echo_base_url")]
    base_url: String,
}

fn default_echo_base_url() -> String {
    "https://echo.example/v1".to_string()
}

struct EchoContribution;

impl ProviderContribution for EchoContribution {
    type Config = EchoConfig;

    fn id() -> &'static str {
        "echo"
    }

    fn build(config: EchoConfig) -> Result<Box<dyn LlmProvider>> {
        Ok(Box::new(EchoProvider {
            base_url: config.base_url,
            api_key_env: config.api_key_env,
        }))
    }
}

#[test]
fn registry_starts_empty() {
    let contributions = ProviderContributions::new();
    assert!(contributions.is_empty());
    assert_eq!(contributions.len(), 0);
    assert!(contributions.ids().next().is_none());
    assert!(!contributions.contains("echo"));
}

#[test]
fn register_records_the_id() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    assert!(!contributions.is_empty());
    assert_eq!(contributions.len(), 1);
    assert!(contributions.contains("echo"));
    let ids: Vec<&str> = contributions.ids().collect();
    assert_eq!(ids, vec!["echo"]);
}

#[test]
#[should_panic(expected = "already registered")]
fn duplicate_registration_panics() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();
    contributions.register::<EchoContribution>();
}

#[test]
fn build_from_toml_str_constructs_registered_providers() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let toml = r#"
[providers.echo]
api_key_env = "ECHO_KEY"
base_url = "https://my-echo.example/v1"
"#;
    let loaded = contributions
        .build_from_toml_str(toml)
        .expect("loader returns Ok");

    assert_eq!(loaded.providers.len(), 1, "exactly one provider built");
    assert!(loaded.unhandled.is_empty(), "no unhandled ids expected");
    assert_eq!(loaded.providers[0].0, "echo");
    assert_eq!(loaded.providers[0].1.name(), "echo");
    assert!(loaded.source_path.is_none(), "in-memory string has no path");
}

#[test]
fn unregistered_provider_sections_land_in_unhandled() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let toml = r#"
[providers.openai]
api_key_env = "OPENAI_API_KEY"

[providers.echo]
api_key_env = "ECHO_KEY"
"#;
    let loaded = contributions
        .build_from_toml_str(toml)
        .expect("loader returns Ok");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "echo");
    assert_eq!(loaded.unhandled, vec!["openai".to_string()]);
}

#[test]
fn build_from_toml_str_omits_root_table_returns_empty() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let loaded = contributions
        .build_from_toml_str("[model]\nprovider = \"openai\"\n")
        .expect("missing [providers] table is not an error");

    assert!(loaded.providers.is_empty());
    assert!(loaded.unhandled.is_empty());
}

#[test]
fn build_from_path_returns_empty_when_file_missing() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let path = std::env::temp_dir().join(format!(
        "squeezy-contribution-missing-{}-{}.toml",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    assert!(
        !path.exists(),
        "test invariant: temp path must not exist yet"
    );

    let loaded = contributions
        .build_from_path(&path)
        .expect("missing file is not an error");

    assert!(loaded.providers.is_empty());
    assert!(loaded.unhandled.is_empty());
}

#[test]
fn build_from_path_decodes_real_file() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let path = temp_path("from-path");
    std::fs::write(
        &path,
        "[providers.echo]\napi_key_env = \"ECHO_KEY\"\nbase_url = \"https://example.test\"\n",
    )
    .expect("seed settings.toml");

    let loaded = contributions
        .build_from_path(&path)
        .expect("settings.toml decoded");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "echo");
}

#[test]
fn bad_toml_is_reported_as_config_error() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let err = contributions
        .build_from_toml_str("this is not [valid")
        .expect_err("malformed TOML must surface as Config error");

    assert!(
        matches!(err, squeezy_core::SqueezyError::Config(_)),
        "expected Config error, got {err:?}"
    );
}

#[test]
fn section_deserialization_errors_reference_provider_id() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    // Missing required `api_key_env` field.
    let err = contributions
        .build_from_toml_str("[providers.echo]\nbase_url = \"https://x\"\n")
        .expect_err("missing required field must error");

    let message = match err {
        squeezy_core::SqueezyError::Config(detail) => detail,
        other => panic!("expected Config error, got {other:?}"),
    };
    assert!(
        message.contains("providers.echo"),
        "error must mention the provider id: {message}"
    );
}

#[test]
fn build_from_path_annotates_config_errors_with_path() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<EchoContribution>();

    let path = temp_path("annotated");
    std::fs::write(&path, "[providers.echo]\nbase_url = \"https://x\"\n").expect("seed");

    let err = contributions
        .build_from_path(&path)
        .expect_err("missing api_key_env triggers Config error");

    let message = match err {
        squeezy_core::SqueezyError::Config(detail) => detail,
        other => panic!("expected Config error, got {other:?}"),
    };
    let path_str = path.display().to_string();
    assert!(
        message.contains(&path_str),
        "expected the path {path_str:?} in error: {message}"
    );
}

// ---- Built-in wrappers ------------------------------------------------

#[test]
fn openai_contribution_builds_provider_from_minimal_toml() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<OpenAiContribution>();

    // SAFETY: tests touch process-global env. Mirror the credentials
    // test harness pattern and write the key directly so we don't depend
    // on env var availability across `cargo nextest` shards.
    let toml = r#"
[providers.openai]
api_key_env = "OPENAI_API_KEY"
api_key = "sk-test"
base_url = "https://example.test/v1"
"#;
    let loaded = contributions
        .build_from_toml_str(toml)
        .expect("openai wrapper builds");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "openai");
    assert_eq!(loaded.providers[0].1.name(), "openai");
}

#[test]
fn anthropic_contribution_builds_provider_from_minimal_toml() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<AnthropicContribution>();

    let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
api_key = "sk-ant-test"
"#;
    let loaded = contributions
        .build_from_toml_str(toml)
        .expect("anthropic wrapper builds");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "anthropic");
    assert_eq!(loaded.providers[0].1.name(), "anthropic");
}

#[test]
fn google_contribution_builds_provider_from_minimal_toml() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<GoogleContribution>();

    let toml = r#"
[providers.google]
api_key_env = "GOOGLE_API_KEY"
api_key = "AIza-test"
"#;
    let loaded = contributions
        .build_from_toml_str(toml)
        .expect("google wrapper builds");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "google");
    assert_eq!(loaded.providers[0].1.name(), "google");
}

#[test]
fn ollama_contribution_builds_with_defaults() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<OllamaContribution>();

    let loaded = contributions
        .build_from_toml_str("[providers.ollama]\n")
        .expect("ollama wrapper accepts empty section");

    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers[0].0, "ollama");
    assert_eq!(loaded.providers[0].1.name(), "ollama");
}

#[test]
fn ollama_contribution_rejects_unknown_route_style() {
    let mut contributions = ProviderContributions::new();
    contributions.register::<OllamaContribution>();

    let err = contributions
        .build_from_toml_str(
            "[providers.ollama]\nbase_url = \"http://localhost:11434\"\nroute_style = \"banana\"\n",
        )
        .expect_err("unknown route style is a config error");

    let message = match err {
        squeezy_core::SqueezyError::Config(detail) => detail,
        other => panic!("expected Config error, got {other:?}"),
    };
    assert!(
        message.contains("route_style"),
        "error must mention the offending field: {message}"
    );
}

#[test]
fn multiple_built_in_contributions_register_together() {
    let mut contributions = ProviderContributions::new();
    contributions
        .register::<OpenAiContribution>()
        .register::<AnthropicContribution>()
        .register::<GoogleContribution>()
        .register::<OllamaContribution>();

    assert_eq!(contributions.len(), 4);
    let mut ids: Vec<&str> = contributions.ids().collect();
    ids.sort();
    assert_eq!(ids, vec!["anthropic", "google", "ollama", "openai"]);
}
