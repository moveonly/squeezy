use std::path::Path;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use squeezy_core::AppConfig;
use squeezy_llm::{LlmEvent, LlmRequest, provider_from_config};
use tokio_util::sync::CancellationToken;

use crate::Scenario;
use crate::driver::EvalError;
use crate::findings::Finding;
use crate::tickets::TicketDraft;

/// Cap on bytes from each artifact so we do not blow the model context.
const ARTIFACT_BUDGET_BYTES: usize = 32_768;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TriageResponse {
    #[serde(default)]
    tickets: Vec<TicketDraft>,
}

pub async fn triage(
    scenario: &Scenario,
    config: &AppConfig,
    trace_path: &Path,
    frames_path: &Path,
    auto_findings: &[Finding],
) -> Result<Vec<TicketDraft>, EvalError> {
    let trace_excerpt = tail_text(trace_path, ARTIFACT_BUDGET_BYTES)?;
    let frames_excerpt = tail_text(frames_path, ARTIFACT_BUDGET_BYTES)?;

    let model = scenario
        .triage
        .model
        .clone()
        .unwrap_or_else(|| config.model.clone());

    let instructions = build_instructions(
        scenario.triage.focus.as_deref(),
        scenario.triage.extra_prompt.as_deref(),
        auto_findings,
    );
    let user = format!(
        "Scenario id: {id}\nScenario title: {title}\n\n--- trace.jsonl tail ---\n{trace}\n\n--- frames.jsonl tail ---\n{frames}\n",
        id = scenario.id,
        title = scenario.title,
        trace = trace_excerpt,
        frames = frames_excerpt,
    );

    let request = LlmRequest::user_text(model, instructions, user, Some(2048));
    let provider = provider_from_config(&config.provider)
        .map_err(|err| EvalError::Provider(format!("{err}")))?;

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let mut acc = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(LlmEvent::TextDelta(delta)) => acc.push_str(&delta),
            Ok(LlmEvent::Completed { .. }) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(EvalError::Provider(format!("triage stream: {err}")));
            }
        }
    }

    let response: TriageResponse = extract_json(&acc)
        .ok_or_else(|| EvalError::Internal("triage response missing JSON".into()))?;
    Ok(response.tickets)
}

fn build_instructions(
    focus: Option<&str>,
    extra: Option<&str>,
    auto_findings: &[Finding],
) -> String {
    let mut text = String::from(
        "You are a senior software-quality reviewer. You will receive the tail of a Squeezy QA \
         run: a JSONL event trace and per-turn assistant frames. Identify concrete bugs, perf \
         issues, UX papercuts, or tooling gaps that an engineer should fix.\n\n\
         Respond with a single JSON object on the form\n\
         {\"tickets\":[{\"id\":\"\",\"title\":\"\",\"severity\":\"minor|major|critical\",\
         \"category\":\"perf|ux|correctness|safety|tooling\",\"summary\":\"\",\"repro\":\"\",\
         \"evidence\":[{\"trace_event\":N,\"frame\":N}],\"suggested_fix\":\"\"}]}\n\n\
         If nothing is wrong, return {\"tickets\":[]}. Output only the JSON object — no prose, \
         no backticks.",
    );
    if let Some(focus) = focus.map(str::trim).filter(|s| !s.is_empty()) {
        text.push_str("\n\nFocus area: ");
        text.push_str(focus);
        text.push_str(". Drop findings unrelated to this surface area.");
    }
    if !auto_findings.is_empty() {
        text.push_str(
            "\n\nThe harness already flagged the following auto-findings — DO NOT re-report them \
             or any restatement thereof. Only surface issues that are NOT already covered here:\n",
        );
        for f in auto_findings {
            text.push_str(&format!("- [{}] {}\n", f.rule_id, f.summary));
        }
    }
    if let Some(extra) = extra.map(str::trim).filter(|s| !s.is_empty()) {
        text.push_str("\n\n");
        text.push_str(extra);
    }
    text
}

fn extract_json(text: &str) -> Option<TriageResponse> {
    let trimmed = text.trim();
    if let Ok(parsed) = serde_json::from_str::<TriageResponse>(trimmed) {
        return Some(parsed);
    }
    // Tolerate a single ```json ... ``` fence.
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && end > start
    {
        let candidate = &trimmed[start..=end];
        if let Ok(parsed) = serde_json::from_str::<TriageResponse>(candidate) {
            return Some(parsed);
        }
    }
    None
}

fn tail_text(path: &Path, budget: usize) -> Result<String, EvalError> {
    let data = std::fs::read_to_string(path)
        .map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
    if data.len() <= budget {
        return Ok(data);
    }
    // Take the tail to bias toward the most recent events.
    let start = data.len() - budget;
    // Snap to the next newline so we don't break a line in half.
    let snapped = data[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);
    Ok(data[snapped..].to_string())
}

#[cfg(test)]
#[path = "triage_tests.rs"]
mod tests;
