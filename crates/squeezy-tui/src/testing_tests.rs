use super::*;
use squeezy_llm::{LlmProvider, LlmRequest, LlmStream};
use tokio_util::sync::CancellationToken;

/// Minimal provider stub: returns its configured name and never
/// streams. Lets the harness build without a live LLM, so the test
/// can render a frame and read the banner-derived model row.
struct NamedProvider(&'static str);

impl LlmProvider for NamedProvider {
    fn name(&self) -> &'static str {
        self.0
    }
    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        use futures_util::stream;
        Box::pin(stream::iter(Vec::new()))
    }
}

#[test]
fn harness_banner_uses_real_provider_name_not_eval_harness() {
    let mut config = AppConfig::default();
    config.model = "test-model".to_string();
    let provider: Arc<dyn LlmProvider> = Arc::new(NamedProvider("anthropic"));
    let mut harness = TuiHarness::new(config, SessionMode::default(), provider, 120, 36, None)
        .expect("build TuiHarness");
    let snapshot = harness.render_frame().expect("render frame");
    let plain = snapshot.plain_text;
    assert!(
        plain.contains("anthropic:test-model"),
        "expected banner to contain `anthropic:test-model`, frame was:\n{plain}"
    );
    assert!(
        !plain.contains("eval-harness:"),
        "banner still carries the harness literal `eval-harness:`; frame was:\n{plain}"
    );
}
