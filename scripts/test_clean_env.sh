#!/usr/bin/env bash
# Wrap `cargo nextest run` with a fresh, credential-free environment.
#
# Every `crates/squeezy-llm/tests/*_costly.rs` integration test is
# `#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, ..."]`
# and only runs when the costly cargo feature, the master env flag, and
# the vendor-specific API key are all present. CI already passes
# `--profile ci` (which leaves `--ignored` off) and never opts into the
# cargo feature, so by construction the paid tests stay off. This
# wrapper is the defense-in-depth belt: it strips every vendor key plus
# the master flag before invoking nextest, so a misconfigured runner, a
# stale `export` in the developer's shell, or a future CI step that
# happens to populate one of these variables cannot accidentally bill
# a real provider.
#
# Usage:
#   scripts/test_clean_env.sh                              # `cargo nextest run`
#   scripts/test_clean_env.sh --workspace --all-targets    # forwarded to nextest
#   scripts/test_clean_env.sh --profile ci -p squeezy-llm  # forwarded to nextest
#
# Run locally before merging a PR that touches `*_costly.rs` to confirm
# the new tests still skip cleanly when no credentials are around.
# Wired into `.github/workflows/ci.yml` as the default test invocation.

set -euo pipefail

# Keep in sync with `fallback_env_var()` in
# `crates/squeezy-llm/src/credentials.rs` and the per-provider gates in
# `crates/squeezy-llm/tests/*_costly.rs`. When a new provider lands,
# extend this list (and the `--api-key-env` field on the provider
# config) at the same time.
CREDENTIAL_ENVS=(
  # Master gate consulted by every `*_costly.rs` test
  # (`require_env_flag(common::COSTLY_FLAG)`).
  SQUEEZY_RUN_COSTLY_TESTS

  # Vendor-named keys probed by the costly tests directly.
  OPENAI_API_KEY
  ANTHROPIC_API_KEY
  GOOGLE_API_KEY
  GEMINI_API_KEY
  AZURE_OPENAI_API_KEY
  AZURE_OPENAI_BASE_URL
  AZURE_OPENAI_API_VERSION
  XAI_API_KEY
  DEEPSEEK_API_KEY
  GROQ_API_KEY
  OPENROUTER_API_KEY
  PORTKEY_API_KEY
  PORTKEY_VIRTUAL_KEY
  AI_GATEWAY_API_KEY
  MISTRAL_API_KEY
  TOGETHER_API_KEY
  FIREWORKS_API_KEY
  CEREBRAS_API_KEY
  LMSTUDIO_API_KEY
  VLLM_API_KEY
  LLAMACPP_API_KEY
  VERTEX_ACCESS_TOKEN
  VERTEX_PROJECT
  VERTEX_LOCATION

  # AWS credential chain consulted by the Bedrock costly test.
  AWS_ACCESS_KEY_ID
  AWS_SECRET_ACCESS_KEY
  AWS_SESSION_TOKEN
  AWS_PROFILE
  AWS_REGION
  AWS_DEFAULT_REGION

  # `SQUEEZY_<PROVIDER>_KEY` fallbacks resolved by `fallback_env_var()`
  # in crates/squeezy-llm/src/credentials.rs. The auth-resolution chain
  # treats these as equivalent to the vendor-named variables, so they
  # need to be cleared too or a stale `SQUEEZY_OPENAI_KEY` would still
  # authenticate.
  SQUEEZY_OPENAI_KEY
  SQUEEZY_ANTHROPIC_KEY
  SQUEEZY_GOOGLE_KEY
  SQUEEZY_GEMINI_KEY
  SQUEEZY_AZURE_KEY
  SQUEEZY_AZURE_OPENAI_KEY
  SQUEEZY_XAI_KEY
  SQUEEZY_DEEPSEEK_KEY
  SQUEEZY_GROQ_KEY
  SQUEEZY_OPENROUTER_KEY
  SQUEEZY_PORTKEY_KEY
  SQUEEZY_VERCEL_KEY
  SQUEEZY_AI_GATEWAY_KEY
  SQUEEZY_MISTRAL_KEY
  SQUEEZY_TOGETHER_KEY
  SQUEEZY_FIREWORKS_KEY
  SQUEEZY_CEREBRAS_KEY
  SQUEEZY_LMSTUDIO_KEY
  SQUEEZY_VLLM_KEY
  SQUEEZY_LLAMACPP_KEY
  SQUEEZY_VERTEX_KEY

  # CI/CD broadcast channels for the credentials file/blob (see
  # `read_credentials_file_for` and `read_credentials_json_env_for`).
  SQUEEZY_CREDENTIALS_JSON
  SQUEEZY_CREDENTIALS_FILE
)

for name in "${CREDENTIAL_ENVS[@]}"; do
  unset "$name"
done

exec cargo nextest run "$@"
