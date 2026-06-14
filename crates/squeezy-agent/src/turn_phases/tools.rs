//! Tool-round execution for a turn.
//!
//! Contains the model/planner tool-call execution pipeline plus the parallel
//! batching helpers it coordinates.

use super::super::*;

pub(crate) async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    context: ToolExecutionContext<'_>,
    broker: &mut CostBroker,
) -> Vec<ToolResult> {
    let _mcp_elicitation_handler = install_mcp_elicitation_handler(&context);
    let mut approved = Vec::new();
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    let mut recorded = vec![false; calls.len()];
    // Buffered `delegate*` calls (excluding `delegate_chain`, which runs
    // its own internal step sequence). The validation loop collects
    // these so they can run concurrently bounded by
    // `SUBAGENT_MAX_CONCURRENT` once the loop finishes, closing the gap
    // where the single-shot dispatcher never used the full concurrent budget.
    let mut delegate_batch_calls: Vec<(usize, ToolCall, SubagentKind)> = Vec::new();

    for (index, call) in calls.iter().enumerate() {
        if context.cancel.is_cancelled() {
            let result = ToolResult::cancelled(call);
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            return collect_recorded_results(
                results,
                recorded,
                broker,
                context.config,
                &context.telemetry,
            );
        }
        if call.name == TASK_STATE_TOOL_NAME {
            results[index] = Some(handle_task_state_call(&context, call).await);
            recorded[index] = true;
            continue;
        }
        if call.name == LOAD_TOOL_SCHEMA_TOOL_NAME {
            results[index] = Some(handle_load_tool_schema_call(&context, call).await);
            recorded[index] = true;
            continue;
        }
        if call.name == REQUEST_USER_INPUT_TOOL_NAME {
            let result = handle_request_user_input_call(&context, call).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }
        if has_invalid_tool_arguments(call) {
            let result = invalid_tool_arguments_result(call);
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }
        if call.name == DELEGATE_CHAIN_TOOL_NAME {
            // `delegate_chain` manages its own internal step sequencing and
            // bookkeeping; it does NOT join the concurrent `delegate_batch`
            // because each step would otherwise need to lock the broker
            // mid-future. The chain still ships through the
            // `record_and_emit_progress` flow so chain completions look
            // identical to single delegates from the parent's telemetry
            // perspective.
            let _ = context
                .tx
                .send(AgentEvent::ToolCallStarted {
                    turn_id: context.turn_id,
                    call: redact_tool_call(call.clone(), &context.redactor),
                    origin: context.origin,
                })
                .await;
            let result = Box::pin(handle_delegate_chain_call(&context, call, broker)).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }
        let delegate_batch_kind = match call.name.as_str() {
            DELEGATE_TOOL_NAME => Some(SubagentKind::Delegate),
            DELEGATE_PLAN_TOOL_NAME => Some(SubagentKind::Plan),
            DELEGATE_REVIEW_TOOL_NAME => Some(SubagentKind::Review),
            _ => None,
        };
        if let Some(kind) = delegate_batch_kind {
            // Anti-redundant-delegation gate (see the const docs above). Refuse a
            // whole-task `delegate` when the parent has already gathered
            // substantial context this task — a cold subagent would re-read the
            // same files for pure overhead. Recall-safe: `Denied` removes no
            // information (the parent already holds the context that tripped the
            // gate and keeps every read/grep/graph tool), and `Denied` is ignored
            // by the repeated-failure loop guard so it cannot abort the turn. An
            // early/context-isolating delegate (counters near zero) is exempt.
            if kind == SubagentKind::Delegate
                && (broker.metrics.bytes_read >= REDUNDANT_DELEGATE_READ_BYTES
                    || broker.metrics.tool_calls >= REDUNDANT_DELEGATE_EXPLORE_CALLS)
            {
                let result = control_tool_result(
                    call,
                    ToolStatus::Denied,
                    json!({
                        "ok": false,
                        "error": "delegate is redundant: substantial context for this task is already gathered in-context",
                        "parent_tool_calls": broker.metrics.tool_calls,
                        "parent_bytes_read": broker.metrics.bytes_read,
                        "guidance": "You have already read/searched substantial relevant context in this task. A delegate subagent starts cold and re-reads the same files — pure overhead. Finish in-context using what you have; use read_file/read_slice/grep and the graph tools directly for any remaining detail."
                    }),
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            // Pre-bump the `subagent_calls` counter before the future
            // is spawned so the in-flight tally stays conservative even
            // while several delegates run concurrently. Per-outcome
            // mutations (failure counters, kind-bucket execution rollup)
            // are deferred to `apply_subagent_dispatch` after each future
            // resolves so concurrent futures never race on the broker.
            record_subagent_call(&mut broker.metrics, kind);
            delegate_batch_calls.push((index, call.clone(), kind));
            continue;
        }
        if call.name == EXPLORE_TOOL_NAME {
            // `explore` keeps the original single-shot path. The task
            // scope only marks `delegate*` variants as parallel-safe; the
            // explore tool stays serial so its broader research session
            // (tool budget, exploration-state lock) doesn't have to
            // coordinate with itself across concurrent futures.
            let _ = context
                .tx
                .send(AgentEvent::ToolCallStarted {
                    turn_id: context.turn_id,
                    call: redact_tool_call(call.clone(), &context.redactor),
                    origin: context.origin,
                })
                .await;
            let result = Box::pin(handle_subagent_call(
                &context,
                call,
                SubagentKind::Explore,
                broker,
            ))
            .await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }

        let tool_sequence = match broker.reserve_call() {
            Ok(tool_sequence) => tool_sequence,
            Err((tool_sequence, reason)) => {
                let result = budget_denied_result(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
        };

        if let Some(reason) = exploration_read_denial_reason(&context, call).await {
            let result = ToolResult::denied(call, reason);
            broker.metrics.planner_refusals += 1;
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                call,
                &result,
                Duration::ZERO,
            );
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }

        let outcome = permission_decision(call, &context).await;
        // Fold any out-of-band reviewer spend into the active broker so the
        // live session-cost snapshot and cap checks stay accurate within this
        // turn (the persisted `state.cost` is already updated by the reviewer
        // path; this call keeps the broker's copy in sync).
        broker.record_out_of_band_session_cost(outcome.reviewer_usd_micros);
        match outcome.decision {
            ApprovalDecision::Approved => approved.push((index, call.clone(), tool_sequence)),
            ApprovalDecision::Denied(reason) => {
                let result = ToolResult::denied(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
            }
            ApprovalDecision::Cancelled => {
                let result = ToolResult::cancelled(call);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
                return collect_recorded_results(
                    results,
                    recorded,
                    broker,
                    context.config,
                    &context.telemetry,
                );
            }
        }
    }

    // Concurrency fast path: when there is a `delegate*` batch *and* the
    // parent's own approved calls are all parallel-safe reads, run the
    // subagent dispatch and the parent reads at the same time instead of
    // blocking the reads behind the subagent. Neither
    // `dispatch_delegate_batch` nor `dispatch_parallel_reads` borrows the
    // broker, so they `tokio::join!` safely; the resolved completions are
    // then folded into the broker serially below, preserving the original
    // "delegate results, then read results" recording order. The path
    // requires every approved call to be parallel-safe so we never reorder a
    // serial (non-parallel-safe) tool relative to the delegate. Per-turn
    // budget enforcement is preserved by `fold_parallel_read_completions`,
    // which gates the reads in input order (subagent tool spend accrues to
    // separate `subagent_*` metrics, so folding the delegate first does not
    // change the parent's own budget verdict).
    if !delegate_batch_calls.is_empty()
        && !context.cancel.is_cancelled()
        && approved
            .iter()
            .all(|(_, call, _)| context.tools.is_parallel_safe(call))
    {
        let delegate_calls = std::mem::take(&mut delegate_batch_calls);
        let read_order: Vec<(usize, ToolCall, u64)> = approved.clone();
        let (delegate_completions, read_completions) = tokio::join!(
            dispatch_delegate_batch(&context, delegate_calls),
            dispatch_parallel_reads(&context, approved),
        );
        // Fold the delegate first, then the reads, matching the original
        // "delegate results, then read results" broker-mutation order. The
        // reads are folded with the same incremental per-turn budget
        // enforcement as the non-concurrent path.
        apply_delegate_completions(
            &context,
            broker,
            &mut results,
            &mut recorded,
            delegate_completions,
        )
        .await;
        fold_parallel_read_completions(
            &context,
            broker,
            &mut results,
            read_order,
            read_completions,
        )
        .await;
        let mut out = collect_recorded_results(
            results,
            recorded,
            broker,
            context.config,
            &context.telemetry,
        );
        mark_intra_batch_duplicates(&calls, &mut out, context.tools);
        return out;
    }

    if !delegate_batch_calls.is_empty() {
        flush_delegate_batch(
            &context,
            broker,
            &mut results,
            &mut recorded,
            std::mem::take(&mut delegate_batch_calls),
        )
        .await;
    }

    let mut parallel_batch = Vec::new();
    for (index, call, tool_sequence) in approved {
        if context.cancel.is_cancelled() {
            let result = ToolResult::cancelled(&call);
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                &call,
                &result,
                Duration::ZERO,
            );
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            break;
        }
        if context.tools.is_parallel_safe(&call) {
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &call,
                    &result,
                    Duration::ZERO,
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            parallel_batch.push((index, call, tool_sequence));
        } else {
            flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &call,
                    &result,
                    Duration::ZERO,
                );
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            results[index] = Some(result);
            recorded[index] = true;
        }
    }
    flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;

    let mut out = collect_recorded_results(
        results,
        recorded,
        broker,
        context.config,
        &context.telemetry,
    );
    mark_intra_batch_duplicates(&calls, &mut out, context.tools);
    out
}

/// Stamp a `duplicate_of` hint onto any tool result whose call has the
/// same `(tool_name, args_sha256)` as an earlier call in the same batch,
/// for tools where re-running can only produce the same answer
/// (`is_parallel_safe`). The execution still happens — flipping that to
/// a real skip needs to thread through cancellation, event emission,
/// and broker accounting — but the marker gives the model immediate
/// feedback so it stops issuing the same grep three times in a row.
fn mark_intra_batch_duplicates(
    calls: &[ToolCall],
    results: &mut [ToolResult],
    tools: &ToolRegistry,
) {
    let mut first_by_key: BTreeMap<(String, String), String> = BTreeMap::new();
    for (call, result) in calls.iter().zip(results.iter_mut()) {
        if !tools.is_parallel_safe(call) {
            continue;
        }
        let Some(args_sha) = tool_call_args_sha256(call) else {
            continue;
        };
        let key = (call.name.clone(), args_sha);
        match first_by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(call.call_id.clone());
            }
            std::collections::btree_map::Entry::Occupied(slot) => {
                if let Some(obj) = result.content.as_object_mut() {
                    obj.insert("duplicate_of".to_string(), json!(slot.get().clone()));
                    obj.entry("hint").or_insert_with(|| {
                        json!(
                            "This call is identical to an earlier call in the same response. \
                             Do not issue duplicate tool calls; reuse the earlier output."
                        )
                    });
                }
            }
        }
    }
}

async fn replay_tool_calls(
    replay: &ReplayRuntime,
    calls: Vec<ToolCall>,
    turn_id: TurnId,
    tx: mpsc::Sender<AgentEvent>,
    broker: &mut CostBroker,
) -> squeezy_core::Result<Vec<ToolResult>> {
    let results = replay.replay_tool_results(&calls)?;
    for (call, result) in calls.iter().zip(results.iter()) {
        let _ = tx
            .send(AgentEvent::ToolCallStarted {
                turn_id,
                call: call.clone(),
                origin: ToolOrigin::Model,
            })
            .await;
        record_and_emit_progress(broker, result, &tx, turn_id).await;
        let _ = tx
            .send(AgentEvent::ToolCallCompleted {
                turn_id,
                result: result.clone(),
            })
            .await;
    }
    Ok(results)
}

fn collect_recorded_results(
    results: Vec<Option<ToolResult>>,
    _recorded: Vec<bool>,
    _broker: &mut CostBroker,
    _config: &AppConfig,
    _telemetry: &TelemetryClient,
) -> Vec<ToolResult> {
    results.into_iter().flatten().collect()
}

pub(super) fn cancelled_tool_result(result: &ToolResult) -> bool {
    result.status == ToolStatus::Cancelled
}

/// Fan out a batch of `delegate*` calls (sans `delegate_chain`) and
/// resolve them concurrently bounded by [`SUBAGENT_MAX_CONCURRENT`].
///
/// Each future calls [`run_subagent_dispatch`] independently — the
/// broker borrow stays serial because every future returns a
/// [`SubagentDispatchOutcome`] that the caller folds back via
/// [`apply_subagent_dispatch`] after collection. Pre-bumped
/// `subagent_calls` counters happen in the validation loop before we
/// reach this helper.
///
/// `recorded` mirrors the caller's tracking vec; entries are marked
/// `true` here so the surrounding pipeline does not re-emit a deny /
/// approval event for these slots.
async fn flush_delegate_batch(
    context: &ToolExecutionContext<'_>,
    broker: &mut CostBroker,
    results: &mut [Option<ToolResult>],
    recorded: &mut [bool],
    calls: Vec<(usize, ToolCall, SubagentKind)>,
) {
    if calls.is_empty() {
        return;
    }
    let completions = dispatch_delegate_batch(context, calls).await;
    apply_delegate_completions(context, broker, results, recorded, completions).await;
}

/// Fan out a `delegate*` batch and resolve every dispatch concurrently
/// *without* touching the broker.
///
/// Split out of [`flush_delegate_batch`] so the pure-async fan-out can be
/// `tokio::join!`ed with the parent's own parallel-safe read batch: neither
/// future borrows `&mut CostBroker`, so they run truly concurrently and the
/// parent's independent reads no longer block on the subagent. The returned
/// completions are folded back into the broker afterward by
/// [`apply_delegate_completions`] so all metric mutations stay serial.
async fn dispatch_delegate_batch(
    context: &ToolExecutionContext<'_>,
    calls: Vec<(usize, ToolCall, SubagentKind)>,
) -> Vec<(usize, SubagentKind, SubagentDispatchOutcome)> {
    if calls.is_empty() {
        return Vec::new();
    }

    // Emit `ToolCallStarted` for each delegate call in input order so the
    // TUI / event subscribers still see the start lines before the model
    // turn proceeds. The actual subagent work happens inside the
    // buffered futures below.
    for (_, call, _) in &calls {
        let _ = context
            .tx
            .send(AgentEvent::ToolCallStarted {
                turn_id: context.turn_id,
                call: redact_tool_call(call.clone(), &context.redactor),
                origin: context.origin,
            })
            .await;
    }

    let cap = context.config.subagents.max_concurrent.max(1);
    futures_util::stream::iter(calls.into_iter().map(|(index, call, kind)| {
        let context = context.clone();
        async move {
            let outcome = Box::pin(run_subagent_dispatch(&context, &call, kind)).await;
            (index, kind, outcome)
        }
    }))
    .buffer_unordered(cap)
    .collect::<Vec<_>>()
    .await
}

/// Fold resolved delegate completions back into the broker and emit their
/// `ToolCallCompleted` events. Counterpart to [`dispatch_delegate_batch`];
/// keeps every broker mutation serial so concurrent fan-out never races on
/// the shared metrics.
async fn apply_delegate_completions(
    context: &ToolExecutionContext<'_>,
    broker: &mut CostBroker,
    results: &mut [Option<ToolResult>],
    recorded: &mut [bool],
    completions: Vec<(usize, SubagentKind, SubagentDispatchOutcome)>,
) {
    for (index, kind, outcome) in completions {
        apply_subagent_dispatch(broker, kind, &outcome);
        record_and_emit_progress(broker, &outcome.result, &context.tx, context.turn_id).await;
        let _ = context
            .tx
            .send(AgentEvent::ToolCallCompleted {
                turn_id: context.turn_id,
                result: outcome.result.clone(),
            })
            .await;
        results[index] = Some(outcome.result);
        recorded[index] = true;
    }
}

async fn flush_parallel_batch(
    context: &ToolExecutionContext<'_>,
    broker: &mut CostBroker,
    results: &mut [Option<ToolResult>],
    batch: &mut Vec<(usize, ToolCall, u64)>,
) {
    if batch.is_empty() {
        return;
    }

    let calls = std::mem::take(batch);
    if context.cancel.is_cancelled() {
        for (index, call, tool_sequence) in calls {
            let result = ToolResult::cancelled(&call);
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                &call,
                &result,
                Duration::ZERO,
            );
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
        }
        return;
    }
    // Run the reads *concurrently* (independent reads must not serialize
    // behind one another — that one-at-a-time `.await` per read dominated
    // turn latency), then fold them back with incremental per-turn budget
    // enforcement. See [`fold_parallel_read_completions`].
    let order: Vec<(usize, ToolCall, u64)> = calls.clone();
    let completions = dispatch_parallel_reads(context, calls).await;
    fold_parallel_read_completions(context, broker, results, order, completions).await;
}

/// Fold concurrently dispatched parallel-read completions back into the
/// broker in the *original call order*, enforcing the per-turn byte/file
/// budget incrementally.
///
/// The reads are dispatched concurrently by [`dispatch_parallel_reads`] so
/// independent reads never serialize behind one another. Folding in input
/// order preserves the prior budget contract: once an earlier read pushes the
/// turn past its byte/file limit, every later read is returned to the model as
/// budget-denied rather than counted. The physical I/O for the dispatched
/// reads is bounded by the per-turn tool-call reservation gate, so the
/// guard-rail's purpose (bounding model-visible context + accounting) holds.
async fn fold_parallel_read_completions(
    context: &ToolExecutionContext<'_>,
    broker: &mut CostBroker,
    results: &mut [Option<ToolResult>],
    order: Vec<(usize, ToolCall, u64)>,
    completions: Vec<(usize, ToolResult)>,
) {
    let mut executed: std::collections::HashMap<usize, ToolResult> =
        completions.into_iter().collect();
    for (index, call, tool_sequence) in order {
        if broker.enforces_result_budgets()
            && let Some(reason) = broker.deny_reason()
        {
            // The turn crossed its byte/file budget on an earlier read in
            // this batch. Override this read's *model-visible* result with a
            // budget denial and drop its actual output so its bytes are not
            // billed — the per-turn guard-rail is preserved. The read already
            // emitted its own `ToolCallStarted`/`ToolCallCompleted` via
            // `run_one_tool` during the concurrent dispatch, so we record the
            // denial without emitting a second completion event for the same
            // call.
            executed.remove(&index);
            let result = budget_denied_result(&call, reason);
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                &call,
                &result,
                Duration::ZERO,
            );
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            results[index] = Some(result);
            continue;
        }
        let result = executed
            .remove(&index)
            .unwrap_or_else(|| ToolResult::cancelled(&call));
        record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
        results[index] = Some(result);
    }
}

/// Fan out a batch of parallel-safe tool calls and resolve them concurrently
/// *without* touching the broker.
///
/// Split out so the fan-out can be `tokio::join!`ed with a `delegate*`
/// dispatch — the parent's independent reads then progress alongside the
/// subagent instead of waiting for it. The completions (in arbitrary order)
/// are folded into the broker by the caller via
/// [`fold_parallel_read_completions`], which keeps every metric mutation
/// serial and enforces the per-turn byte/file budget in input order.
async fn dispatch_parallel_reads(
    context: &ToolExecutionContext<'_>,
    calls: Vec<(usize, ToolCall, u64)>,
) -> Vec<(usize, ToolResult)> {
    if calls.is_empty() {
        return Vec::new();
    }
    futures_util::stream::iter(calls.into_iter().map(|(index, call, tool_sequence)| {
        let context = context.clone();
        async move {
            let result = run_one_tool(context, tool_sequence, call).await;
            (index, result)
        }
    }))
    .buffer_unordered(context.config.max_parallel_tools.max(1))
    .collect::<Vec<_>>()
    .await
}
