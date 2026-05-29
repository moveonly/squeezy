//! OAuth-backed [`ApiKeySource`] implementations for vendor subscription
//! credentials (Anthropic Claude Pro/Max, OpenAI ChatGPT Plus/Pro via
//! Codex, GitHub Copilot, …).
//!
//! Each provider's flow lives in its own submodule so the constants
//! (client id, scopes, endpoints) stay close to the wire format they
//! describe. Shared helpers — PKCE generation, base64url encoding,
//! local HTTP callback server — sit at this module's root so a new
//! OAuth subagent can be added without copy-pasting the cryptographic
//! primitives.
//!
//! The submodules return an `Arc<dyn ApiKeySource>` so the existing
//! provider clients (which already hold their credential through that
//! trait, per `crates/squeezy-llm/src/credentials.rs`) keep working
//! unchanged: the same `bearer_auth` path stamps the rotating access
//! token on every request, and the auth-retry layer
//! ([`crate::retry::send_with_auth_retry`]) handles `401`/`403`
//! refreshes.
//!
//! [`ApiKeySource`]: crate::credentials::ApiKeySource

pub mod anthropic;
pub mod github_copilot;
pub(crate) mod openai_codex;
mod pkce;

pub use anthropic::{
    ANTHROPIC_OAUTH_TOKEN_PREFIX, AnthropicLoginConfig, AnthropicOAuthSource, PersistedTokens,
    TokenResponse, anthropic_oauth_beta_header,
    default_storage_path as anthropic_default_storage_path, exchange_authorization_code,
    is_anthropic_oauth_token, parse_authorization_input, read_tokens as anthropic_read_tokens,
    refresh_anthropic_token, write_tokens as anthropic_write_tokens,
};
pub use github_copilot::{
    AUTH_FILE_NAME as GITHUB_COPILOT_AUTH_FILE_NAME, DEFAULT_POLICY_MODELS,
    DeviceCodeResponse as GitHubCopilotDeviceCodeResponse, DevicePollOutcome,
    GitHubCopilotLoginHooks, GitHubCopilotLoginOutcome, GitHubCopilotOAuthSource,
    GitHubCopilotProvider, GitHubCopilotUrls, PersistedGitHubCopilotTokens,
    PolicyEnablementOutcome, auth_file_path as github_copilot_auth_file_path,
    base_url_from_token as github_copilot_base_url_from_token,
    default_auth_path as default_github_copilot_auth_path, enable_models as enable_copilot_models,
    login_github_copilot_interactive, normalize_domain as normalize_github_domain,
    poll_for_github_token, read_tokens as github_copilot_read_tokens, refresh_copilot_token,
    resolve_base_url as resolve_github_copilot_base_url,
    start_device_flow as start_github_copilot_device_flow,
    write_tokens as github_copilot_write_tokens,
};
pub use openai_codex::{
    OPENAI_CODEX_AUTH_FILE_NAME, OpenAiCodexLoginOutcome, OpenAiCodexOAuthSource,
    OpenAiCodexProvider, codex_auth_file_path, default_codex_auth_path, load_codex_token,
    login_openai_codex_interactive, save_codex_token,
};
pub use pkce::{PkceCodes, generate_pkce};
