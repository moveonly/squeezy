//! PKCE (RFC 7636) helpers for the OAuth subscription providers.
//!
//! Generates a 32-byte random verifier, encodes it as base64url, and
//! derives the SHA-256 challenge the authorize endpoint expects. The
//! verifier doubles as the OAuth `state` parameter in pi's Anthropic
//! flow, so we keep both values together in [`PkceCodes`] to make that
//! reuse explicit at the call site.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use squeezy_core::{Result, SqueezyError};

/// PKCE verifier (kept private to the client) and its SHA-256
/// challenge (sent to the authorize endpoint). The verifier is also
/// reused as the OAuth `state` value by pi's Anthropic flow, so
/// callers should treat both fields as bound to the same login
/// attempt.
#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a fresh PKCE verifier + S256 challenge pair backed by the
/// OS CSPRNG. The verifier is 32 random bytes encoded base64url (43
/// characters), the challenge is `base64url(sha256(verifier))`.
pub fn generate_pkce() -> Result<PkceCodes> {
    let mut verifier_bytes = [0u8; 32];
    getrandom::fill(&mut verifier_bytes)
        .map_err(|err| SqueezyError::Config(format!("PKCE random bytes failed: {err}")))?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = challenge_for(&verifier);
    Ok(PkceCodes {
        verifier,
        challenge,
    })
}

/// Compute the S256 challenge for an existing verifier. Exposed so
/// tests can assert the round-trip without relying on
/// [`generate_pkce`]'s entropy source.
pub fn challenge_for(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

#[cfg(test)]
#[path = "pkce_tests.rs"]
mod tests;
