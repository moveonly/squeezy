//! Provider-aware token estimation with EMA calibration.
//!
//! Pure logic lives here; persistence of calibration state is the
//! responsibility of `squeezy-store`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default bytes-per-token ratio used as a starting estimate when no
/// calibration sample has been recorded.
pub const DEFAULT_BYTES_PER_TOKEN: f64 = 4.0;

/// Smoothing factor applied to new observations when blending into an
/// existing exponential moving average.
pub const DEFAULT_EMA_ALPHA: f64 = 0.2;

/// Returns the provider-specific default bytes-per-token ratio.
///
/// These defaults come from public tokenizer behaviour: GPT-family
/// BPE tokens are roughly 4 bytes of English text, Claude is similar,
/// Gemini's SentencePiece runs slightly larger, and Llama-derived
/// Ollama tokenizers track close to 4.
pub fn default_bytes_per_token(provider: &str) -> f64 {
    match provider {
        "anthropic" => 4.0,
        "google" => 4.4,
        "ollama" => 3.6,
        "openai" | "azure_openai" | "bedrock" => 4.0,
        _ => DEFAULT_BYTES_PER_TOKEN,
    }
}

/// Estimate the token count for `text` using a bytes-per-token ratio.
pub fn estimate_tokens(text: &str, bytes_per_token: f64) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let bytes = text.len() as f64;
    let estimate = (bytes / bytes_per_token.max(0.1)).ceil();
    estimate.max(1.0) as u64
}

/// Per-provider calibration state that can be persisted across runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCalibration {
    pub providers: BTreeMap<String, ProviderCalibration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProviderCalibration {
    pub bytes_per_token: f64,
    pub samples: u32,
}

impl ProviderCalibration {
    pub fn seed(provider: &str) -> Self {
        Self {
            bytes_per_token: default_bytes_per_token(provider),
            samples: 0,
        }
    }
}

impl TokenCalibration {
    pub fn bytes_per_token(&self, provider: &str) -> f64 {
        self.providers
            .get(provider)
            .map(|entry| entry.bytes_per_token)
            .unwrap_or_else(|| default_bytes_per_token(provider))
    }

    /// Blend a fresh sample into the calibration using an EMA.
    ///
    /// `observed_bytes` is the byte length of the prompt we measured
    /// and `observed_tokens` is the count the provider actually
    /// reported back in its usage payload.
    pub fn record_sample(&mut self, provider: &str, observed_bytes: u64, observed_tokens: u64) {
        if observed_bytes == 0 || observed_tokens == 0 {
            return;
        }
        let ratio = observed_bytes as f64 / observed_tokens as f64;
        let entry = self
            .providers
            .entry(provider.to_string())
            .or_insert_with(|| ProviderCalibration::seed(provider));
        if entry.samples == 0 {
            entry.bytes_per_token = ratio;
        } else {
            entry.bytes_per_token =
                DEFAULT_EMA_ALPHA * ratio + (1.0 - DEFAULT_EMA_ALPHA) * entry.bytes_per_token;
        }
        entry.samples = entry.samples.saturating_add(1);
    }
}

#[cfg(test)]
#[path = "tokens_tests.rs"]
mod tests;
