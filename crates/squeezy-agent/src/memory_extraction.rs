//! Automatic, session-boundary memory extraction.
//!
//! A cheap (small/fast-model) auxiliary LLM pass that runs *after* a top-level
//! turn settles — never blocking the user — reads the new slice of the
//! conversation plus the current memory indexes, and proposes durable facts to
//! save or stale ones to delete. It writes through [`squeezy_store::memory`],
//! so the type the extractor picks routes each fact to the global or project
//! scope automatically. This is the "memory updates itself" half of the
//! feature; the inline `memory` tool is the explicit/override half.
//!
//! The pure pieces here (prompt building, response parsing, op application) are
//! unit-tested directly; the orchestration ([`run_extraction`]) is a thin LLM
//! call mirroring the turn-router judge.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest};
use squeezy_store::memory::{Memory, MemoryType};
use tokio_util::sync::CancellationToken;

/// Hard timeout for the extraction call. Generous (it runs in the background)
/// but bounded so a hung provider never leaks a task.
const EXTRACTION_TIMEOUT_MS: u64 = 30_000;

/// Output cap for the extraction call. Generous — memory writes are rare, so
/// when a pass does fire we give it room to be thorough rather than cheap.
const EXTRACTION_MAX_OUTPUT_TOKENS: u32 = 2_048;

/// Cap on each index passed to the extractor. The index is one line per memory,
/// so this comfortably fits hundreds of entries — kept generous so dedup and
/// contradiction checks see the whole store, not a truncated view.
pub(crate) const EXTRACTION_MAX_INDEX_BYTES: usize = 32_768;

/// Most ops we apply from a single extraction, a backstop against a runaway
/// model dumping the whole conversation into memory.
const EXTRACTION_MAX_OPS: usize = 8;

/// Minimum new user-authored characters since the last extraction before a pass
/// is worth running. Trivial turns ("yes", "thanks", a one-line tweak) fall
/// below it, so a substantive conversation triggers extraction periodically
/// rather than every turn — this is the cost governor.
pub(crate) const EXTRACTION_MIN_NEW_PROSE_CHARS: usize = 400;

pub(crate) const MEMORY_EXTRACTION_SYSTEM_PROMPT: &str = "\
You are the memory extractor for a coding agent. You see the most recent slice of a conversation \
between a user and the agent, plus the agent's CURRENT memory index (every memory already saved, both \
global and project). Extract the durable facts worth remembering for future sessions and emit memory \
operations that keep the store coherent.

Respond with ONLY a JSON array (no prose, no markdown fences). Each op is one object:
  {\"op\":\"save\",\"name\":\"<slug>\",\"type\":\"user|feedback|project|reference\",\"description\":\"<one line>\",\"body\":\"<one paragraph>\"}
  {\"op\":\"delete\",\"name\":\"<slug>\"}
If nothing durable is worth saving and nothing needs correcting, respond with exactly [].

## Choosing the type (this routes the memory's scope — choose deliberately)
- user — who the user is: role, expertise, durable preferences or constraints. Saved globally.
  e.g. {\"op\":\"save\",\"name\":\"backend-then-react\",\"type\":\"user\",\"description\":\"Senior Go/backend, new to this React frontend\",\"body\":\"Deep Go/backend experience, new to this repo's React frontend — frame frontend explanations in backend terms.\"}
- feedback — how the user wants you to COLLABORATE in general: corrections ('no, not that') and \
confirmations ('yes, keep doing that'). Saved globally. Lead the body with the rule, then a **Why:** \
line and a **How to apply:** line. Guidance specific to THIS repo (a testing policy, a build \
invariant) is a `project` memory, not feedback.
  e.g. {\"op\":\"save\",\"name\":\"terse-no-summaries\",\"type\":\"feedback\",\"description\":\"Wants terse replies, no trailing summaries\",\"body\":\"Keep replies terse with no end-of-turn summary.\\n**Why:** the user reads the diff themselves.\\n**How to apply:** drop recap sections.\"}
- project — ongoing work, decisions, incidents, or repo-specific conventions in THIS repository, not \
derivable from code or git. Saved to this repo. Convert relative dates to absolute. Add **Why:** / \
**How to apply:** lines.
  e.g. {\"op\":\"save\",\"name\":\"auth-rewrite-compliance\",\"type\":\"project\",\"description\":\"Auth rewrite is compliance-driven, not tech-debt\",\"body\":\"The auth-middleware rewrite is driven by legal/compliance around session-token storage.\\n**Why:** legal flagged it.\\n**How to apply:** favor compliance over ergonomics in scope decisions.\"}
- reference — where information lives in an external system (issue tracker, dashboard, channel). \
Saved to this repo.
  e.g. {\"op\":\"save\",\"name\":\"pipeline-bugs-linear\",\"type\":\"reference\",\"description\":\"Pipeline bugs tracked in Linear INGEST\",\"body\":\"Pipeline bugs are tracked in the Linear project INGEST.\"}

## Keep the store coherent — dedup and resolve contradictions
The index lists every saved memory as `- [Title](memory/<slug>.md) - hook`; the <slug> is the name.
- If a memory already covers a fact, do NOT add a near-duplicate.
- If this conversation refines an existing memory, `save` with that memory's EXISTING <slug> (same \
name) to overwrite it in place — never mint a new slug for the same topic.
- If this conversation CONTRADICTS or supersedes a saved memory (the user changed their mind, a \
decision was reversed), overwrite that <slug> with the corrected fact OR `delete` it. Never leave two \
memories that disagree.

## Do NOT save
Never save secrets, API keys, credentials, tokens, or personal data. Also skip: code patterns, \
conventions, architecture, file paths, or project structure (re-derivable from the code); git history \
or who-changed-what; debugging fix recipes (the fix is in the code); anything already in AGENTS.md; \
ephemeral state that only mattered this turn. When unsure, omit — a small, high-signal, \
non-contradictory store is the goal.

## Format
One fact per op, one paragraph. `name`: a 2-4 word slug, lowercase letters/digits/'-'/'_' only. \
`description`: one line. Emit only the JSON array.";

/// One operation the extractor proposes, already validated into typed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryOp {
    Save {
        name: String,
        ty: MemoryType,
        description: String,
        body: String,
        title: Option<String>,
        hook: Option<String>,
    },
    Delete {
        name: String,
    },
}

/// What an extraction pass actually persisted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtractionResult {
    pub saved: Vec<(String, MemoryType)>,
    pub deleted: Vec<String>,
    pub skipped: usize,
}

impl ExtractionResult {
    pub fn is_empty(&self) -> bool {
        self.saved.is_empty() && self.deleted.is_empty()
    }

    /// A compact one-line summary for logs / surfacing, or `None` when nothing
    /// changed.
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let saved: Vec<String> = self
            .saved
            .iter()
            .map(|(name, ty)| format!("{name} ({})", ty.as_str()))
            .collect();
        let mut parts = Vec::new();
        if !saved.is_empty() {
            parts.push(format!("saved {}", saved.join(", ")));
        }
        if !self.deleted.is_empty() {
            parts.push(format!("removed {}", self.deleted.join(", ")));
        }
        Some(parts.join("; "))
    }
}

#[derive(Deserialize)]
struct RawOp {
    op: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    ty: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    hook: Option<String>,
}

/// Slice out the first top-level JSON array in `response`, tolerating models
/// that wrap it in prose or ```json fences.
fn extract_json_array(response: &str) -> Option<&str> {
    let start = response.find('[')?;
    let end = response.rfind(']')?;
    (end > start).then(|| &response[start..=end])
}

/// Parse an extraction reply into validated ops, dropping anything malformed.
pub(crate) fn parse_extraction_ops(response: &str) -> Vec<MemoryOp> {
    let Some(json) = extract_json_array(response) else {
        return Vec::new();
    };
    let raws: Vec<RawOp> = serde_json::from_str(json).unwrap_or_default();
    let mut ops = Vec::new();
    for raw in raws.into_iter().take(EXTRACTION_MAX_OPS) {
        match raw.op.trim().to_ascii_lowercase().as_str() {
            "save" => {
                let Some(ty) = raw.ty.as_deref().and_then(MemoryType::parse) else {
                    continue;
                };
                let (Some(name), Some(description), Some(body)) =
                    (raw.name, raw.description, raw.body)
                else {
                    continue;
                };
                ops.push(MemoryOp::Save {
                    name,
                    ty,
                    description,
                    body,
                    title: raw.title,
                    hook: raw.hook,
                });
            }
            "delete" => {
                if let Some(name) = raw.name {
                    ops.push(MemoryOp::Delete { name });
                }
            }
            _ => {}
        }
    }
    ops
}

/// Apply parsed ops to the memory store, tolerating individual failures (a bad
/// slug, an empty body) by counting them as skipped rather than aborting.
pub(crate) fn apply_ops(memory: &Memory, ops: Vec<MemoryOp>) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    for op in ops {
        match op {
            MemoryOp::Save {
                name,
                ty,
                description,
                body,
                title,
                hook,
            } => match memory.save(
                &name,
                ty,
                &description,
                &body,
                title.as_deref(),
                hook.as_deref(),
            ) {
                Ok(saved) => result.saved.push((saved.name, ty)),
                Err(_) => result.skipped += 1,
            },
            MemoryOp::Delete { name } => match memory.delete(&name) {
                Ok(true) => result.deleted.push(name),
                Ok(false) => {}
                Err(_) => result.skipped += 1,
            },
        }
    }
    result
}

/// Render the extraction call's user content: the conversation slice plus the
/// current indexes (for dedup).
pub(crate) fn build_extraction_input(
    conversation_slice: &str,
    global_index: &str,
    project_index: &str,
) -> String {
    let global = if global_index.trim().is_empty() {
        "(empty)"
    } else {
        global_index.trim()
    };
    let project = if project_index.trim().is_empty() {
        "(empty)"
    } else {
        project_index.trim()
    };
    format!(
        "## Current global memory index\n{global}\n\n\
         ## Current project memory index\n{project}\n\n\
         ## Recent conversation\n{}\n\n\
         Emit the JSON array of memory operations now.",
        conversation_slice.trim()
    )
}

/// Run one extraction pass: build the request, make the cheap LLM call, parse
/// the reply, and apply it to the memory store for `workspace_root`.
///
/// Returns `None` when the LLM call itself failed (timeout / provider error) so
/// the caller can leave its high-water mark unadvanced and retry the slice next
/// turn; `Some(result)` when the call ran (even if it proposed nothing), so the
/// slice is considered handled.
pub(crate) async fn run_extraction(
    provider: &Arc<dyn LlmProvider>,
    model: Arc<str>,
    workspace_root: &Path,
    conversation_slice: &str,
    global_index: &str,
    project_index: &str,
) -> Option<ExtractionResult> {
    let input = build_extraction_input(conversation_slice, global_index, project_index);
    let request = LlmRequest {
        model,
        instructions: Arc::from(MEMORY_EXTRACTION_SYSTEM_PROMPT),
        input: Arc::from(vec![LlmInputItem::UserText(input)]),
        max_output_tokens: Some(EXTRACTION_MAX_OUTPUT_TOKENS),
        tools: Arc::from(Vec::new()),
        store: false,
        ..LlmRequest::default()
    };
    let cancel = CancellationToken::new();
    let mut stream = provider.stream_response(request, cancel);
    let fetch = async {
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LlmEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(LlmEvent::Completed { .. }) => break,
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        Some(text)
    };
    let raw = tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(EXTRACTION_TIMEOUT_MS)) => None,
        result = fetch => result,
    };
    // `None` means the call failed — let the caller retry the slice. A parsed
    // (even empty) reply means the slice was handled.
    let text = raw?;
    let ops = parse_extraction_ops(&text);
    Some(apply_ops(&Memory::new(Some(workspace_root)), ops))
}

#[cfg(test)]
#[path = "memory_extraction_tests.rs"]
mod tests;
