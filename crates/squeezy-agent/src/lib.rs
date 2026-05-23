use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, CostSnapshot, PermissionMode, PermissionScope, SqueezyError, TranscriptItem, TurnId,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmToolSpec};
use squeezy_tools::{
    ToolCall, ToolCostHint, ToolOutputConfig, ToolReceipt, ToolRegistry, ToolResult, ToolSpec,
    ToolStatus, WebToolConfig, sha256_hex,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_TOOL_ROUNDS: usize = 8;

#[derive(Clone)]
pub struct Agent {
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    next_turn_id: Arc<AtomicU64>,
    next_approval_id: Arc<AtomicU64>,
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        let output_config = ToolOutputConfig {
            spill_threshold_bytes: config.tool_spill_threshold_bytes,
            preview_bytes: config.tool_preview_bytes,
            retention_days: config.tool_output_retention_days,
        };
        let web_config = WebToolConfig {
            exa_mcp_url: config.exa_mcp_url.clone(),
            exa_api_key: env::var(&config.exa_api_key_env).ok(),
        };
        let tools = ToolRegistry::new_with_configs(
            config.workspace_root.clone(),
            output_config,
            web_config.clone(),
        )
        .unwrap_or_else(|_| {
            ToolRegistry::new_with_configs(".", output_config, web_config)
                .expect("current directory must be a valid tool root")
        });
        Self {
            config,
            provider,
            tools,
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_approval_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn start_turn(
        &self,
        input: String,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel(128);
        let provider = self.provider.clone();
        let config = self.config.clone();
        let tools = self.tools.clone();
        let tool_specs = tools
            .specs()
            .into_iter()
            .map(llm_tool_spec)
            .collect::<Vec<_>>();
        let turn_id = TurnId::new(self.next_turn_id.fetch_add(1, Ordering::Relaxed));
        let approval_ids = self.next_approval_id.clone();

        tokio::spawn(async move {
            if tx
                .send(AgentEvent::UserMessage {
                    turn_id,
                    message: TranscriptItem::user(input.clone()),
                })
                .await
                .is_err()
            {
                return;
            }

            let outcome = TurnRuntime {
                turn_id,
                provider,
                config,
                tools,
                tool_specs,
                tx: tx.clone(),
                cancel,
                approval_ids,
            }
            .run(input)
            .await;

            if let Err(error) = outcome {
                let _ = tx.send(AgentEvent::Failed { turn_id, error }).await;
            }
        });

        rx
    }
}

struct TurnRuntime {
    turn_id: TurnId,
    provider: Arc<dyn LlmProvider>,
    config: AppConfig,
    tools: ToolRegistry,
    tool_specs: Vec<LlmToolSpec>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
}

impl TurnRuntime {
    async fn run(self, input: String) -> squeezy_core::Result<()> {
        let mut conversation = vec![LlmInputItem::UserText(input)];
        let mut next_input = conversation.clone();
        let mut previous_response_id = None;
        let mut assistant_text = String::new();
        let mut total_cost = CostSnapshot::default();
        let mut seen_tool_outputs = SeenToolOutputs::default();

        for _round in 0..MAX_TOOL_ROUNDS {
            let request = LlmRequest {
                model: self.config.model.clone(),
                instructions: self.config.instructions.clone(),
                input: next_input.clone(),
                max_output_tokens: self.config.max_output_tokens,
                previous_response_id: previous_response_id.clone(),
                tools: self.tool_specs.clone(),
                store: self.config.store_responses,
            };
            let mut stream = self.provider.stream_response(request, self.cancel.clone());
            let mut tool_calls = Vec::new();
            let mut completed = false;
            let mut response_id = None;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(LlmEvent::Started) => {
                        if self
                            .tx
                            .send(AgentEvent::Started {
                                turn_id: self.turn_id,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    Ok(LlmEvent::TextDelta(delta)) => {
                        assistant_text.push_str(&delta);
                        if self
                            .tx
                            .send(AgentEvent::AssistantDelta {
                                turn_id: self.turn_id,
                                delta,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    Ok(LlmEvent::ToolCall(tool_call)) => {
                        let call = ToolCall {
                            call_id: tool_call.call_id,
                            name: tool_call.name,
                            arguments: tool_call.arguments,
                        };
                        if self
                            .tx
                            .send(AgentEvent::ToolCallQueued {
                                turn_id: self.turn_id,
                                call: call.clone(),
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        tool_calls.push(call);
                    }
                    Ok(LlmEvent::Completed {
                        response_id: id,
                        cost,
                    }) => {
                        merge_cost(&mut total_cost, &cost);
                        response_id = id;
                        completed = true;
                        break;
                    }
                    Ok(LlmEvent::Cancelled) => {
                        let _ = self
                            .tx
                            .send(AgentEvent::Cancelled {
                                turn_id: self.turn_id,
                            })
                            .await;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }

            if !completed {
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message: TranscriptItem::assistant(assistant_text),
                        response_id: None,
                        cost: total_cost,
                    })
                    .await;
                return Ok(());
            }

            if tool_calls.is_empty() {
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message: TranscriptItem::assistant(assistant_text),
                        response_id,
                        cost: total_cost,
                    })
                    .await;
                return Ok(());
            }

            let results = execute_tool_calls(
                self.turn_id,
                tool_calls.clone(),
                self.tools.clone(),
                &self.config,
                self.tx.clone(),
                self.cancel.clone(),
                self.approval_ids.clone(),
            )
            .await;
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            seen_tool_outputs.remember_results(&results);

            let outputs = results
                .into_iter()
                .map(|pending| {
                    let output = pending.result.model_output();
                    LlmInputItem::FunctionCallOutput {
                        call_id: pending.result.call_id,
                        output,
                    }
                })
                .collect::<Vec<_>>();

            if self.config.store_responses {
                previous_response_id = response_id;
                next_input = outputs;
            } else {
                previous_response_id = None;
                conversation.extend(tool_calls.into_iter().map(llm_function_call_item));
                conversation.extend(outputs.clone());
                next_input = conversation.clone();
            }
        }

        Err(SqueezyError::Agent(format!(
            "stopped after {MAX_TOOL_ROUNDS} tool rounds"
        )))
    }
}

async fn execute_tool_calls(
    turn_id: TurnId,
    calls: Vec<ToolCall>,
    tools: ToolRegistry,
    config: &AppConfig,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
) -> Vec<ToolResult> {
    let mut approved = Vec::new();
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];

    for (index, call) in calls.iter().enumerate() {
        match permission_decision(
            turn_id,
            call,
            &tools,
            config,
            &tx,
            &cancel,
            approval_ids.clone(),
        )
        .await
        {
            ApprovalDecision::Approved => approved.push((index, call.clone())),
            ApprovalDecision::Denied(reason) => {
                let result = ToolResult::denied(call, reason);
                let _ = tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
            }
            ApprovalDecision::Cancelled => {
                let result = ToolResult::cancelled(call);
                let _ = tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                return results.into_iter().flatten().collect();
            }
        }
    }

    let mut parallel_batch = Vec::new();
    for (index, call) in approved {
        if tools.is_parallel_safe(&call) {
            parallel_batch.push((index, call));
        } else {
            flush_parallel_batch(
                turn_id,
                &tools,
                &tx,
                &cancel,
                &mut results,
                &mut parallel_batch,
                config.max_parallel_tools,
            )
            .await;
            let result =
                run_one_tool(turn_id, tools.clone(), call, tx.clone(), cancel.clone()).await;
            results[index] = Some(result);
        }
    }
    flush_parallel_batch(
        turn_id,
        &tools,
        &tx,
        &cancel,
        &mut results,
        &mut parallel_batch,
        config.max_parallel_tools,
    )
    .await;

    results.into_iter().flatten().collect()
}

async fn flush_parallel_batch(
    turn_id: TurnId,
    tools: &ToolRegistry,
    tx: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
    results: &mut [Option<ToolResult>],
    batch: &mut Vec<(usize, ToolCall)>,
    max_parallel_tools: usize,
) {
    if batch.is_empty() {
        return;
    }

    let calls = std::mem::take(batch);
    let completions = stream::iter(calls.into_iter().map(|(index, call)| {
        let tools = tools.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        async move {
            let result = run_one_tool(turn_id, tools, call, tx, cancel).await;
            (index, result)
        }
    }))
    .buffer_unordered(max_parallel_tools.max(1))
    .collect::<Vec<_>>()
    .await;

    for (index, result) in completions {
        results[index] = Some(result);
    }
}

async fn run_one_tool(
    turn_id: TurnId,
    tools: ToolRegistry,
    call: ToolCall,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> ToolResult {
    let _ = tx
        .send(AgentEvent::ToolCallStarted {
            turn_id,
            call: call.clone(),
        })
        .await;
    let result = tools.execute(call, cancel).await;
    let _ = tx
        .send(AgentEvent::ToolCallCompleted {
            turn_id,
            result: result.clone(),
        })
        .await;
    result
}

async fn permission_decision(
    turn_id: TurnId,
    call: &ToolCall,
    tools: &ToolRegistry,
    config: &AppConfig,
    tx: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
    approval_ids: Arc<AtomicU64>,
) -> ApprovalDecision {
    let scope = tools.permission_scope(call);
    match config.permissions.mode_for(scope) {
        PermissionMode::Allow => ApprovalDecision::Approved,
        PermissionMode::Deny => ApprovalDecision::Denied(format!("{scope:?} permission is denied")),
        PermissionMode::Ask => {
            let (decision_tx, decision_rx) = oneshot::channel();
            let request = ToolApprovalRequest {
                id: approval_ids.fetch_add(1, Ordering::Relaxed),
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                scope,
                summary: tools.describe_call(call),
            };
            let send_approval = tx.send(AgentEvent::ApprovalRequested {
                turn_id,
                request,
                decision_tx,
            });
            let send_result = tokio::select! {
                _ = cancel.cancelled() => return ApprovalDecision::Cancelled,
                result = send_approval => result,
            };
            if send_result.is_err() {
                return ApprovalDecision::Denied("approval channel closed".to_string());
            }
            match tokio::select! {
                _ = cancel.cancelled() => return ApprovalDecision::Cancelled,
                decision = decision_rx => decision,
            } {
                Ok(ToolApprovalDecision::Approved) => ApprovalDecision::Approved,
                Ok(ToolApprovalDecision::Denied) => {
                    ApprovalDecision::Denied("user denied tool call".to_string())
                }
                Err(_) => ApprovalDecision::Denied("approval was not answered".to_string()),
            }
        }
    }
}

fn llm_tool_spec(spec: ToolSpec) -> LlmToolSpec {
    LlmToolSpec {
        name: spec.name,
        description: spec.description,
        parameters: spec.parameters,
        strict: false,
    }
}

fn llm_function_call_item(call: ToolCall) -> LlmInputItem {
    LlmInputItem::FunctionCall {
        call_id: call.call_id,
        name: call.name,
        arguments: call.arguments,
    }
}

fn merge_cost(total: &mut CostSnapshot, next: &CostSnapshot) {
    total.input_tokens = add_optional(total.input_tokens, next.input_tokens);
    total.output_tokens = add_optional(total.output_tokens, next.output_tokens);
    total.cached_input_tokens = add_optional(total.cached_input_tokens, next.cached_input_tokens);
    total.estimated_usd_micros =
        add_optional(total.estimated_usd_micros, next.estimated_usd_micros);
}

#[derive(Debug, Clone)]
struct SeenToolOutput {
    call_id: String,
    tool_name: String,
    stable_output_sha256: String,
    content_sha256: Option<String>,
    model_output_bytes: usize,
}

impl SeenToolOutput {
    fn from_result(result: &ToolResult) -> Self {
        Self {
            call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            stable_output_sha256: stable_output_sha256(result),
            content_sha256: result.receipt.content_sha256.clone(),
            model_output_bytes: result.model_output().len(),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingToolResult {
    result: ToolResult,
    remember: Option<SeenToolOutput>,
    same_as_current_call_id: Option<String>,
}

#[derive(Debug, Default)]
struct SeenToolOutputs {
    by_tool_output: BTreeMap<(String, String), SeenToolOutput>,
}

impl SeenToolOutputs {
    fn prepare_results(&self, results: Vec<ToolResult>) -> Vec<PendingToolResult> {
        let mut prepared = Vec::with_capacity(results.len());
        let mut seen = self
            .by_tool_output
            .iter()
            .map(|(key, seen)| {
                (
                    key.clone(),
                    RoundSeenToolOutput {
                        output: seen.clone(),
                        current_round: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        for result in results {
            prepared.push(Self::prepare_result(result, &mut seen));
        }
        prepared
    }

    fn prepare_result(
        result: ToolResult,
        seen: &mut BTreeMap<(String, String), RoundSeenToolOutput>,
    ) -> PendingToolResult {
        if !is_receipt_stub_candidate(&result) {
            return PendingToolResult {
                result,
                remember: None,
                same_as_current_call_id: None,
            };
        }

        let key = (result.tool_name.clone(), stable_output_sha256(&result));
        if let Some(seen) = seen.get(&key) {
            return PendingToolResult {
                result: receipt_stub_result(result, &seen.output),
                remember: None,
                same_as_current_call_id: seen.current_round.then(|| seen.output.call_id.clone()),
            };
        }

        let output = SeenToolOutput::from_result(&result);
        seen.insert(
            key,
            RoundSeenToolOutput {
                output: output.clone(),
                current_round: true,
            },
        );
        PendingToolResult {
            remember: Some(output),
            result,
            same_as_current_call_id: None,
        }
    }

    fn remember_results(&mut self, results: &[PendingToolResult]) {
        for result in results {
            if let Some(seen) = result.remember.clone() {
                self.by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RoundSeenToolOutput {
    output: SeenToolOutput,
    current_round: bool,
}

fn is_receipt_stub_candidate(result: &ToolResult) -> bool {
    result.status == ToolStatus::Success
        && matches!(
            result.tool_name.as_str(),
            "glob" | "grep" | "read_file" | "read_tool_output" | "webfetch" | "websearch"
        )
}

fn stable_output_sha256(result: &ToolResult) -> String {
    result
        .content
        .get("original_output_sha256")
        .and_then(Value::as_str)
        .unwrap_or(&result.receipt.output_sha256)
        .to_string()
}

fn receipt_stub_result(result: ToolResult, seen: &SeenToolOutput) -> ToolResult {
    let content = json!({
        "receipt_stub": true,
        "message": "identical tool output already sent to the model in this turn",
        "same_as_call_id": &seen.call_id,
        "same_as_tool_name": &seen.tool_name,
        "original_output_sha256": &seen.stable_output_sha256,
        "original_content_sha256": &seen.content_sha256,
        "original_model_output_bytes": seen.model_output_bytes,
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();
    let mut cost_hint = result.cost_hint;
    cost_hint.output_bytes = output_bytes.len() as u64;
    cost_hint.truncated = true;

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: result.status,
        content,
        cost_hint,
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
    }
}

fn pack_tool_results(
    results: Vec<PendingToolResult>,
    budget_bytes: usize,
) -> Vec<PendingToolResult> {
    if budget_bytes == 0 {
        return results;
    }

    let mut used = 0usize;
    let mut visible_current_call_ids = BTreeSet::new();
    results
        .into_iter()
        .map(|mut pending| {
            if pending
                .same_as_current_call_id
                .as_ref()
                .is_some_and(|call_id| !visible_current_call_ids.contains(call_id))
            {
                pending.result = receipt_stub_reference_omitted(pending.result);
                pending.remember = None;
                pending.same_as_current_call_id = None;
            }

            let bytes = pending.result.model_output().len();
            if used.saturating_add(bytes) <= budget_bytes {
                used += bytes;
                if pending.remember.is_some() {
                    visible_current_call_ids.insert(pending.result.call_id.clone());
                }
                pending
            } else {
                let compact = pending
                    .result
                    .aggregate_budget_exceeded(budget_bytes, bytes);
                used = used.saturating_add(compact.model_output().len());
                PendingToolResult {
                    result: compact,
                    remember: None,
                    same_as_current_call_id: None,
                }
            }
        })
        .collect()
}

fn receipt_stub_reference_omitted(result: ToolResult) -> ToolResult {
    let content = json!({
        "error": "tool result omitted because the identical result it references was omitted by the aggregate tool-result budget",
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: ToolStatus::Error,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output_bytes.len() as u64,
            truncated: true,
            ..Default::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
    }
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub id: u64,
    pub call_id: String,
    pub tool_name: String,
    pub scope: PermissionScope,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approved,
    Denied,
}

enum ApprovalDecision {
    Approved,
    Denied(String),
    Cancelled,
}

#[derive(Debug)]
pub enum AgentEvent {
    UserMessage {
        turn_id: TurnId,
        message: TranscriptItem,
    },
    Started {
        turn_id: TurnId,
    },
    AssistantDelta {
        turn_id: TurnId,
        delta: String,
    },
    ToolCallQueued {
        turn_id: TurnId,
        call: ToolCall,
    },
    ToolCallStarted {
        turn_id: TurnId,
        call: ToolCall,
    },
    ToolCallCompleted {
        turn_id: TurnId,
        result: ToolResult,
    },
    ApprovalRequested {
        turn_id: TurnId,
        request: ToolApprovalRequest,
        decision_tx: oneshot::Sender<ToolApprovalDecision>,
    },
    Completed {
        turn_id: TurnId,
        message: TranscriptItem,
        response_id: Option<String>,
        cost: CostSnapshot,
    },
    Cancelled {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        error: SqueezyError,
    },
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
