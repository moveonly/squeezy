//! OAuth-driven [`crate::credentials::ApiKeySource`] implementations.
//!
//! Subscription providers (Claude Pro/Max, ChatGPT Plus/Pro, GitHub
//! Copilot) issue short-lived access tokens that have to refresh
//! mid-session. Each submodule owns one vendor's specifics — endpoint
//! URLs, PKCE parameters, token persistence shape, beta headers — and
//! exposes an [`crate::credentials::ApiKeySource`] the existing
//! provider clients can hold under the same `Arc<dyn ApiKeySource>` as
//! their static-key cousins.

pub mod anthropic;
mod pkce;

pub use anthropic::{
    ANTHROPIC_OAUTH_TOKEN_PREFIX, AnthropicLoginConfig, AnthropicOAuthSource, PersistedTokens,
    TokenResponse, anthropic_oauth_beta_header,
    default_storage_path as anthropic_default_storage_path, exchange_authorization_code,
    is_anthropic_oauth_token, parse_authorization_input, read_tokens as anthropic_read_tokens,
    refresh_anthropic_token, write_tokens as anthropic_write_tokens,
};
pub use pkce::{PkceCodes, generate_pkce};
