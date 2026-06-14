//! Turn orchestration phases.
//!
//! This module owns the hot `TurnRuntime::run` path and keeps phase-specific
//! orchestration out of the crate root. The request/stream/tool/terminal
//! submodules provide the named boundaries used by the run loop.

pub(super) mod request;
pub(super) mod stream;
pub(super) mod terminal;
pub(super) mod tools;

use super::*;
use request::{
    effective_tool_choice, request_beta_headers, request_reasoning_effort_for_tier,
    request_response_verbosity,
};
use stream::next_llm_stream_event;
use tools::{cancelled_tool_result, execute_tool_calls};

impl TurnRuntime {
    pub(super) async fn run(mut self, input: String) -> squeezy_core::Result<()> {
        // Open a per-turn span so all events emitted during this turn carry
        // the same span_id. `begin_turn` returns None when telemetry is
        // disabled; end_turn is called in finish_turn/finish_cancelled_turn.
        self.telemetry.begin_turn();
        // Session-scoped hooks fire on the first turn so handlers
        // installed via `Agent::set_hooks` *after* `Agent::new`
        // still observe the boundary. Cheap when no hooks are
        // registered (each helper short-circuits before building a
        // payload).
        if self.turn_id.get() == 1 {
            self.dispatch_setup();
            self.dispatch_session_start();
            // Emit a session-level banner when the Windows sandbox is running
            // at the job-object-only (disabled) tier. At the restricted-token
            // or elevated tiers, filesystem isolation is partially or fully
            // enforced, so the "no isolation" caveat does not apply.
            #[cfg(target_os = "windows")]
            {
                use squeezy_core::WindowsSandboxLevel;
                if self.config.permissions.shell_sandbox.windows_sandbox_level
                    == WindowsSandboxLevel::Disabled
                {
                    let _ = self
                        .tx
                        .send(AgentEvent::WindowsSandboxActive {
                            turn_id: self.turn_id,
                        })
                        .await;
                }
            }
        }
        let original_input = input.clone();
        let display_tracks_input = self.display_input == original_input;
        let mut display_input = std::mem::take(&mut self.display_input);
        // UserPromptSubmit gives handlers a chance to rewrite the
        // user's input before any skill activation or routing. The
        // `mutate.prompt` field of any handler's reply replaces the
        // in-flight prompt; the chain runs in registration order so
        // later handlers see earlier rewrites.
        let input = self.dispatch_user_prompt_submit(input);
        if display_tracks_input {
            display_input = input.clone();
        }
        let task_title = input.clone();
        let activation = self.tools.activate_skills_for_input(&input)?;
        for warning in &activation.warnings {
            self.log_event(
                "skill_activation_warning",
                Some(self.turn_id),
                Some(format!(
                    "skill {} skipped: {}",
                    warning.name, warning.message
                )),
                json!({
                    "skill": warning.name,
                    "message": warning.message,
                }),
            );
            let _ = self
                .tx
                .send(AgentEvent::SkillActivationWarning {
                    turn_id: self.turn_id,
                    name: warning.name.clone(),
                    message: warning.message.clone(),
                })
                .await;
        }
        // Partition by execution-context mode declared in `SKILL.md`
        // frontmatter. Inline-mode skills (the default) keep the
        // existing `<active_skills>` system-prompt injection. Fork-mode
        // skills are surfaced via a separate `<fork_skills>` block so
        // the model treats them as candidates for a focused subagent
        // dispatch rather than instructions for the parent turn.
        let (inline_skills, fork_skills): (Vec<_>, Vec<_>) =
            activation.skills.iter().cloned().partition(|skill| {
                !matches!(
                    skill.summary.context_mode,
                    squeezy_skills::SkillContextMode::Fork
                )
            });
        let (active_block, skill_metrics) =
            self.tools.format_active_skills_with_metrics(&inline_skills);
        let fork_block = self.tools.format_fork_skills(&fork_skills);
        // Fire skill activation telemetry for all activated skills.
        if !activation.skills.is_empty() {
            // Count sources from the *activated* skills only, not the whole
            // catalog; using discovery.by_source would count every discovered
            // skill regardless of whether it was activated.
            let mut source_counts = std::collections::BTreeMap::new();
            for skill in &activation.skills {
                *source_counts
                    .entry(skill.summary.source.as_str().to_string())
                    .or_default() += 1u64;
            }
            let explicit_count = activation
                .kinds
                .iter()
                .filter(|k| matches!(k, SkillActivationKind::Explicit))
                .count() as u32;
            let trigger_count = activation
                .kinds
                .iter()
                .filter(|k| matches!(k, SkillActivationKind::Trigger))
                .count() as u32;
            let implicit_shell_count = activation
                .kinds
                .iter()
                .filter(|k| matches!(k, SkillActivationKind::ImplicitShell))
                .count() as u32;
            self.telemetry
                .spawn(TelemetryEvent::skill_activated(SkillActivationReport {
                    total: skill_metrics.total as u32,
                    included: skill_metrics.included as u32,
                    dropped: skill_metrics.dropped as u32,
                    body_truncated: skill_metrics.body_truncated as u32,
                    preamble_emitted: false,
                    preamble_omitted_count: 0,
                    explicit_count,
                    trigger_count,
                    implicit_shell_count,
                    source_counts,
                }));
        }
        // `manifest.tool_deps` declared by an activated skill must
        // match an advertised tool name (or `mcp:<server>` for a
        // ready MCP). When a dep is missing the skill body would
        // happily reference it anyway, so surface a structured
        // refusal note in the system prompt before the LLM call.
        let mut all_active_skills = inline_skills.clone();
        all_active_skills.extend(fork_skills.iter().cloned());
        let missing_deps = self.tools.audit_skill_tool_deps(&all_active_skills);
        let dep_warnings = if missing_deps.is_empty() {
            None
        } else {
            for (skill, missing) in &missing_deps {
                tracing::warn!(
                    target: "squeezy_skills",
                    skill = %skill,
                    missing = ?missing,
                    "skill manifest declares tool_deps that are not available in this session"
                );
                let message = format!(
                    "skill `{skill}` requires tool(s) not available in this session: {}. \
                     The skill will refuse rather than improvise.",
                    missing.join(", ")
                );
                // Use try_send (non-blocking) to avoid adding a new await point
                // inside the per-turn activation hot path, which would increase
                // the async future size and risk stack overflows on constrained
                // platforms.
                if let Err(err) = self.tx.try_send(AgentEvent::SkillActivationWarning {
                    turn_id: self.turn_id,
                    name: skill.clone(),
                    message,
                }) {
                    tracing::error!(
                        target: "squeezy_agent",
                        skill = %skill,
                        %err,
                        "tool_deps warning event dropped: channel at capacity or closed"
                    );
                }
            }
            Some(format_skill_tool_dep_warnings(&missing_deps))
        };
        let base_instructions = match (active_block, fork_block, dep_warnings) {
            (Some(active), Some(fork), Some(warn)) => format!(
                "{}\n\n{}\n\n{}\n\n{}",
                self.config.instructions, active, fork, warn
            ),
            (Some(active), Some(fork), None) => {
                format!("{}\n\n{}\n\n{}", self.config.instructions, active, fork)
            }
            (Some(active), None, Some(warn)) => {
                format!("{}\n\n{}\n\n{}", self.config.instructions, active, warn)
            }
            (Some(active), None, None) => format!("{}\n\n{}", self.config.instructions, active),
            (None, Some(fork), Some(warn)) => {
                format!("{}\n\n{}\n\n{}", self.config.instructions, fork, warn)
            }
            (None, Some(fork), None) => format!("{}\n\n{}", self.config.instructions, fork),
            (None, None, Some(warn)) => format!("{}\n\n{}", self.config.instructions, warn),
            (None, None, None) => self.config.instructions.clone(),
        };
        let native_text_verbosity = capabilities_for(self.provider.name(), &self.config.model)
            .is_some_and(|capabilities| capabilities.text_verbosity);
        let verbosity_instructions = instructions_with_response_verbosity(
            &base_instructions,
            self.config.tui.response_verbosity,
            native_text_verbosity,
        );
        // G3: optional batching nudge. Off by default (byte-for-byte
        // unchanged prompt); when enabled it lands in a deterministic
        // position so the per-session cache prefix stays stable.
        let batch_hint_instructions = instructions_with_batch_hint(
            &verbosity_instructions,
            self.config.batch_tool_calls_hint,
        );
        // Plan mode is enforced by tool-filtering elsewhere; the overlay
        // here tells the model *why* its toolbox shrank and what the
        // expected output contract (`<proposed_plan>`) looks like.
        let active_mode = load_session_mode(&self.session_mode);
        let session_id_for_plan_mode = self.session_id();
        let mode_instructions = plan_mode::instructions_for_mode(
            &batch_hint_instructions,
            active_mode,
            &self.config.workspace_root,
            session_id_for_plan_mode.as_deref(),
        );
        let mut prior_state = self.conversation_state.lock().await.clone();
        // Pinned context must reach the model on every turn, not only
        // after a compaction has occurred. Inline it into the per-turn
        // instructions so a `/pin` is immediately visible to the model
        // even on sessions that never cross the compaction threshold.
        let raw_instructions = instructions_with_pinned_context(
            &mode_instructions,
            &prior_state.context_compaction.pinned,
        );
        let active_attachments = prior_state
            .context_attachments
            .iter()
            .filter(|attachment| attachment.is_active())
            .cloned()
            .collect::<Vec<_>>();
        let user_transcript = TranscriptItem::user(format_user_text_with_context(
            &display_input,
            &active_attachments,
        ));
        // Redact at insertion time so the conversation upholds the
        // "already redacted" invariant. The per-round LLM request build
        // then sends `next_input` straight through without rebuilding
        // the vector via `redact_llm_input_items`.
        let user_item = redact_input_item(
            LlmInputItem::UserText(format_user_text_with_context(
                &activation.task_input,
                &active_attachments,
            )),
            &self.redactor,
        );
        // F18: fan vision-routable image attachments into
        // `LlmInputItem::Image` items so the bytes reach the provider
        // verbatim. They sit immediately after the user text so each
        // provider's request encoder can coalesce them into a single
        // multimodal user message (Anthropic content blocks, OpenAI
        // `input_image`, Bedrock `ImageBlock`, …). The provider's
        // `ensure_vision_support` call then surfaces a structured
        // `ProviderRequest` error if the active model lacks vision.
        let mut image_items = image_input_items_for_attachments(&active_attachments);
        image_items.extend(document_input_items_for_attachments(&active_attachments));
        image_items.extend(std::mem::take(&mut self.transient_input_items));
        // Upgrade any legacy conversation items resumed from disk so the
        // invariant holds for the rest of this turn. Idempotent and
        // cheap for items already in redacted form.
        let mut conversation = redact_llm_input_items(
            std::mem::take(&mut prior_state.conversation),
            &self.redactor,
        );
        conversation.push(user_item.clone());
        for image_item in &image_items {
            conversation.push(image_item.clone());
        }
        let mut context_compaction = prior_state.context_compaction.clone();
        // Trim pre-pass: before the lossy summarize gate, reclaim older bulky
        // `FunctionCallOutput` bodies (reads/shell/web) in place so they are
        // cleared before any summary head replaces the older slice. Cheap and
        // structure-preserving. It rewrites earlier items, so a successful trim
        // invalidates response-id reuse below (forces a full resend).
        let post_turn_trimmed =
            if let Some(report) = maybe_micro_compact(&mut conversation, &self.config, None) {
                self.log_event(
                    "context_micro_compacted",
                    Some(self.turn_id),
                    Some(format!(
                        "post-turn trim cleared {} tool outputs, freed {} bytes",
                        report.cleared_call_ids.len(),
                        report.bytes_saved,
                    )),
                    json!({
                        "cleared_call_ids": &report.cleared_call_ids,
                        "bytes_saved": report.bytes_saved,
                        "before_estimated_tokens": report.before_estimated_tokens,
                        "after_estimated_tokens": report.after_estimated_tokens,
                        "phase": "post_turn",
                    }),
                );
                true
            } else {
                false
            };
        // PreCompact hook fires only when the auto trigger's
        // thresholds are crossed so handlers don't see a hook on every
        // turn — only when compaction will actually run. PostCompact
        // mirrors the report's before/after counts so observers can
        // measure the rewrite. The no-hook path stays allocation-free.
        let overhead_tokens = self.last_request_overhead_tokens.load(Ordering::Relaxed);
        let compaction_decision =
            context_compaction_decision(&conversation, &self.config, overhead_tokens);
        if compaction_decision.should_compact {
            self.dispatch_pre_compact(compaction_decision.estimate.estimated_tokens);
        }
        if let Some(report) = maybe_compact_conversation(
            &mut conversation,
            &mut context_compaction,
            &active_attachments,
            self.store.as_deref(),
            &self.provider,
            self.session_log.as_ref(),
            &self.redactor,
            &self.config,
            ContextCompactionTrigger::Auto,
            overhead_tokens,
        )
        .await
        {
            self.dispatch_post_compact(
                report.record.before.estimated_tokens,
                report.record.after.estimated_tokens,
            );
            self.log_event(
                "context_compacted",
                Some(self.turn_id),
                Some(format!(
                    "compacted context gen={} {}->{} estimated tokens",
                    report.record.generation,
                    report.record.before.estimated_tokens,
                    report.record.after.estimated_tokens
                )),
                json!({
                    "record": report.record,
                    "summary": report.summary,
                    "replacement_id": report.record.replacement_id,
                    "conversation": report.post_compact,
                }),
            );
            let _ = self
                .tx
                .send(AgentEvent::ContextCompacted {
                    turn_id: self.turn_id,
                    report,
                })
                .await;
        }
        // Response-id reuse is gated on the compaction generation being
        // unchanged for this turn. Invariant: `maybe_compact_conversation`
        // is the sole bumper of `context_compaction.generation` between
        // a turn's `prior_state` snapshot and this point — if some future
        // caller starts bumping it elsewhere (e.g. on resume), the
        // previous_response_id must be invalidated the same way to keep
        // the provider state consistent.
        let mut previous_response_id = if self.config.store_responses {
            // A post-turn trim rewrote earlier outputs in place; reusing the
            // server-side response id would leave the provider on its untrimmed
            // copy, so force a full resend just like a generation bump does.
            if !post_turn_trimmed
                && context_compaction.generation == prior_state.context_compaction.generation
            {
                prior_state.previous_response_id.take()
            } else {
                None
            }
        } else {
            None
        };
        let mut next_input = if previous_response_id.is_some() && self.config.store_responses {
            // Sending only the latest user delta to the server-side
            // store path still needs the image fan-out — the
            // attachments are turn-scoped context the provider must
            // see alongside the new user text.
            let mut delta = Vec::with_capacity(1 + image_items.len());
            delta.push(user_item.clone());
            delta.extend(image_items.iter().cloned());
            delta
        } else {
            conversation.clone()
        };
        let mut total_cost = CostSnapshot::default();
        let mut seen_tool_outputs = SeenToolOutputs::from_store(self.store.clone());
        let mut broker = CostBroker::new(&self.config);
        broker.seed_session(&prior_state.cost, prior_state.token_calibration.clone());
        let exploration_plan = self
            .config
            .exploration_graph
            .then(|| compile_exploration_plan(&input))
            .flatten();
        let exploration_state = Arc::new(Mutex::new(ExplorationTurnState::from_plan(
            exploration_plan.as_ref(),
        )));
        broker.metrics.redactions += std::mem::take(&mut self.seed_redactions);
        // Instructions are static across the turn's tool rounds; redact
        // them once so the cost is not paid (or double-counted) per round.
        let redacted_instructions = self.redactor.redact(&raw_instructions);
        broker.metrics.redactions += redacted_instructions.redactions;
        let mut request_instructions = redacted_instructions.text;
        let mut active_skill_names = activation
            .skills
            .iter()
            .map(|skill| skill.summary.name.clone())
            .collect::<BTreeSet<_>>();
        // Holding a single stream redactor across rounds keeps the tail
        // buffer alive so a secret straddling a tool-call boundary is
        // still redacted before being released downstream.
        let mut assistant_stream = StreamRedactor::new(self.redactor.clone());
        // The Completed event's message is the concatenation of every
        // AssistantDelta we have already emitted plus the final flushed
        // tail. Building it as we go (rather than re-redacting the raw
        // text at the end) keeps ordinals stable between what streamed
        // into the TUI and what lands in the transcript.
        let mut assistant_message = String::new();
        self.log_event(
            "user_message",
            Some(self.turn_id),
            user_item_summary(&user_item),
            json!({}),
        );
        self.record_replay(
            SessionReplayEventKind::UserMessage,
            json!({ "input": input }),
        );
        self.publish_task_state(TaskStateSnapshot::starting(task_title.clone()))
            .await;
        if self.cancel.is_cancelled() {
            self.finish_cancelled_turn(
                &task_title,
                &total_cost,
                &broker.metrics,
                &broker.calibration,
            )
            .await;
            return Ok(());
        }

        if let Some(plan) = exploration_plan.clone()
            && !plan.calls.is_empty()
        {
            broker.metrics.planner_turns += 1;
            broker.metrics.planner_tool_calls += plan.calls.len() as u64;
            self.log_event(
                "exploration_plan",
                Some(self.turn_id),
                Some(format!("{} planner preflight", plan.intent.as_str())),
                json!({
                    "intent": plan.intent.as_str(),
                    "query": plan.query,
                    "calls": plan
                        .calls
                        .iter()
                        .map(|call| call.name.clone())
                        .collect::<Vec<_>>(),
                }),
            );
            let planned_calls = plan.calls;
            let mut planner_items = planned_calls
                .iter()
                .cloned()
                .map(|call| llm_function_call_item(call, &self.redactor))
                .collect::<Vec<_>>();
            let results = if let Some(replay) = &self.replay {
                replay_tool_calls(
                    replay,
                    planned_calls.clone(),
                    self.turn_id,
                    self.tx.clone(),
                    &mut broker,
                )
                .await?
            } else {
                execute_tool_calls(
                    planned_calls.clone(),
                    ToolExecutionContext {
                        turn_id: self.turn_id,
                        origin: ToolOrigin::Planner,
                        provider: self.provider.clone(),
                        tools: &self.tools,
                        jobs: &self.jobs,
                        config: &self.config,
                        telemetry: self.telemetry.clone(),
                        redactor: self.redactor.clone(),
                        tx: self.tx.clone(),
                        cancel: self.cancel.clone(),
                        approval_ids: self.approval_ids.clone(),
                        session_rules: self.session_rules.clone(),
                        ai_reviewer_state: self.ai_reviewer_state.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        conversation_state: Some(self.conversation_state.clone()),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                        subagents: self.subagents.clone(),
                        subagent_catalog: self.subagent_catalog.clone(),
                        store: self.store.clone(),
                        hooks: self.hooks.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
                return Ok(());
            }
            if self.append_implicit_skill_instructions(
                &results,
                &mut active_skill_names,
                &mut request_instructions,
                &mut broker.metrics,
            ) {
                previous_response_id = None;
            }
            // The planner is advisory: once the preflight block has executed,
            // the model has the planner outputs (success or not) in context, so
            // we lift the raw-read guard to avoid locking the turn on misfires
            // or non-`Success` graph results.
            exploration_state.lock().await.mark_preflight_complete();
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            self.record_replay_tool_results(&planned_calls, &results);
            for pending in &results {
                broker.record_model_result(&pending.result);
            }
            seen_tool_outputs.remember_results(&results);

            let outputs = results
                .into_iter()
                .map(|pending| {
                    let output = self.redactor.redact(&pending.result.model_output()).text;
                    LlmInputItem::FunctionCallOutput {
                        call_id: pending.result.call_id,
                        output,
                        content_parts: None,
                        is_error: tool_status_is_model_error(pending.result.status),
                    }
                })
                .collect::<Vec<_>>();
            planner_items.extend(outputs.clone());
            conversation.extend(planner_items.clone());
            for output in &outputs {
                self.log_event(
                    "tool_result",
                    Some(self.turn_id),
                    tool_output_summary(output),
                    json!({ "output": resume_item_for_json(output.clone()), "source": "exploration_graph" }),
                );
            }
            if self.config.store_responses {
                next_input = vec![user_item.clone()];
                next_input.extend(planner_items);
            } else {
                next_input = conversation.clone();
            }
        }

        let mut last_tool_round_summary = None;
        let mut loop_guard = ToolLoopGuard::default();
        // Per-turn cache of `<instructions> + <tool_index>` keyed by
        // session mode. `request_instructions`, `self.all_tool_specs`,
        // and `self.config.tools` are turn-stable; only `active_mode`
        // (which the TUI can flip mid-turn) varies, and the rare implicit
        // skill append below invalidates this on a revision boundary.
        let mut instructions_cache: [Option<String>; 2] = [None, None];
        // Fire the PreTurn hook once per user turn, immediately before
        // the first round's LLM request is built. Handlers can append
        // turn-scoped instructions via the typed mutate contract —
        // see `dispatch_pre_turn`. The returned text is appended to
        // `request_instructions` so the next-round builder picks it
        // up the same way it picks up implicit skill instructions.
        if let Some(extra) = self.dispatch_pre_turn() {
            request_instructions.push_str("\n\n");
            request_instructions.push_str(&extra);
            instructions_cache = [None, None];
        }
        // One-shot "the model promised follow-up tool use but stopped"
        // recovery latch. Set when a round ends with `finish_reason=stop`,
        // zero tool calls, AND the assistant text contains an intent
        // phrase that named a tool action (the canonical Qwen3 chatty-
        // preamble pattern). Forces one extra round with a synthetic
        // user nudge ("Continue. Call the tool you described, or give
        // the final answer.") before letting the turn end. Capped at
        // one retry per turn to prevent infinite loops if the model
        // ignores the nudge.
        let mut replan_retry_used = false;
        // Append-only visible text from rounds that are internally retried.
        // A retry may be useful for weak "I will call a tool" stops, but it
        // must never let a later nudge response replace text the user already
        // saw in the same turn.
        let mut deferred_retry_visible_assistant = String::new();
        let mut pause_turn_reissues = 0usize;
        // One-shot corrective retry for a Gemini `MALFORMED_FUNCTION_CALL`
        // stop (tool-call arguments the upstream parser rejected, leaving
        // no usable call). Bounded so a model that keeps emitting bad JSON
        // can't loop the turn forever.
        let mut malformed_retry_used = false;
        // Per-turn model routing decision. The classifier runs once at
        // the top of the turn; `current_model` is what each round
        // dispatches on. On mid-turn escalation it is overwritten with
        // the parent model for the rest of the turn — the conversation
        // state survives because `replace_provider` is not needed for
        // a within-provider swap on the wire.
        let parent_model_str = self.config.model.clone();
        let parent_model: Arc<str> = Arc::from(parent_model_str.clone());
        let routing_override = {
            let mut state = self.routing_state.lock().expect("routing state lock");
            let snapshot = state.pending_override;
            // Force-cheap / force-parent are one-shot per turn; clear them
            // so the next prompt routes on its own merits.
            state.pending_override.force_cheap = false;
            state.pending_override.force_parent = false;
            snapshot
        };
        let (sticky_active, sticky_remaining_after_tick) = {
            let mut state = self.routing_state.lock().expect("routing state lock");
            let was_sticky = state.sticky.tick();
            (was_sticky, state.sticky.remaining_turns)
        };
        // Mirror the post-tick value into `ConversationState` so a
        // resume snapshot taken after this turn reflects the
        // decremented sticky window (not the value the previous turn
        // engaged).
        self.conversation_state
            .lock()
            .await
            .set_routing_sticky_remaining_turns(sticky_remaining_after_tick);
        // The cost/capability ladder for the active provider — weak (small-fast)
        // → medium (Sonnet-class) → strong (the parent/headline model). The
        // classifier picks a starting rung; mid-turn escalation steps up it one
        // rung at a time. Owned (no borrow of `self.config`), so it stays valid
        // across the `&mut self` calls in the turn loop below.
        let routing_ladder = squeezy_core::TierLadder::resolve(
            &self.config,
            self.provider.name(),
            cheap_model_for(self.provider.name(), &self.config).as_deref(),
            &parent_model_str,
        );
        let classify_result = turn_router::classify_turn(
            turn_router::ClassifyTurnInputs {
                user_input: &task_title,
                provider: &self.provider,
                provider_name: self.provider.name(),
                parent_model: &parent_model_str,
                ladder: &routing_ladder,
                config: &self.config,
                has_image_input: !image_items.is_empty(),
                has_large_attachment: has_large_non_image_attachment(
                    &active_attachments,
                    self.config.routing.large_attachment_bypass_bytes,
                ),
                turn_index: self.turn_id.get(),
                prior_turn_was_hard: prior_state.routing_prior_turn_was_hard(),
                session_mode: active_mode,
                overrides: routing_override,
                sticky: sticky_active,
                linux_sandbox_sensitive_parent: self.config.routing.linux_sandbox_sensitive_parent,
            },
            self.cancel.clone(),
        )
        .await;
        let decision = classify_result.decision;
        let judge_cost = classify_result.judge_cost;
        let judge_model = classify_result.judge_model;
        // The judge's per-task effort (when `[routing].judge_effort` is on)
        // overrides the static tier→effort map — but only while the turn is on
        // the rung the judge actually assigned. Once an escalation (or a
        // context bump) moves to a different rung, the judge's estimate no
        // longer applies and the rung's own default effort takes over.
        let judge_assigned_tier = decision.tier();
        let judge_effort = classify_result.judge_effort;
        // Fold the judge call's spend into the broker so its tokens
        // count against `max_session_cost_usd_micros` and surface in
        // the turn's provider cost — that's the bill the provider
        // already sent over the wire. Stamp the same number into
        // `routing_judge_usd_micros` so the audit field shows the
        // judge's share separately from the main turn's request.
        // `record_provider_cost` consumes the one-shot warn latch when the
        // session crosses `cost_warn_percent`. If the judge's spend is the
        // call that crosses it, we must emit the warning here: the main
        // turn's later `record_provider_cost` would see `warn_emitted` and
        // return `None`, so dropping the status would lose the user-facing
        // heads-up entirely.
        if judge_cost.estimated_usd_micros.is_some()
            || judge_cost.input_tokens.is_some()
            || judge_cost.output_tokens.is_some()
        {
            let judge_model_for_cost = judge_model.as_deref().unwrap_or(parent_model_str.as_str());
            if let Some(status) = broker.record_provider_cost(
                self.provider.name(),
                judge_model_for_cost,
                CostOrigin::Main,
                &judge_cost,
            ) {
                let _ = self
                    .tx
                    .send(AgentEvent::CostWarning {
                        turn_id: self.turn_id,
                        status,
                    })
                    .await;
            }
            broker.metrics.routing_judge_usd_micros = judge_cost
                .estimated_usd_micros
                .unwrap_or(0)
                .saturating_add(broker.metrics.routing_judge_usd_micros);
            // The judge call is real billable spend. Fold it into the turn's
            // cost snapshot too (not just the broker's provider aggregate) so
            // it lands in `state.cost`, keeping the `/cost` headline equal to
            // the per-model ledger's main-origin total.
            merge_cost(&mut total_cost, &judge_cost);
        }
        // The rung this turn starts on (`Strong` == the parent/headline model).
        // Mutated as mid-turn escalation steps it up the ladder.
        let mut current_tier = decision.tier();
        let mut current_model: Arc<str> = match &decision {
            turn_router::TurnRoutingDecision::Cheap { model, .. } => model.clone(),
            turn_router::TurnRoutingDecision::Parent => parent_model.clone(),
        };
        let mut on_cheap_turn = decision.is_cheap();
        // Context-aware tier selection (NO compaction). A routed turn must fit
        // the chosen rung's effective window exactly as the conversation already
        // stands; if it does not, climb the ladder to the cheapest rung that
        // does. Routing must never shrink the context the parent resumes on next
        // turn (the Opus→Haiku→broken-context hazard), so we step UP, never
        // compact down. If nothing below the parent fits, the turn runs on the
        // parent with no savings. Compaction stays owned by the parent model's
        // own pressure logic.
        if on_cheap_turn {
            let mut context_bumped_to: Option<Arc<str>> = None;
            while current_tier != ModelTier::Strong {
                let observed_ceiling = {
                    let state = self.conversation_state.lock().await;
                    state
                        .observed_context_ceilings
                        .get(&(self.provider.name().to_string(), current_model.to_string()))
                        .copied()
                };
                if model_fits_conversation(
                    &self.config,
                    self.provider.name(),
                    self.configured_model_context_window,
                    &current_model,
                    &conversation,
                    observed_ceiling,
                ) {
                    break;
                }
                // The current rung cannot hold the context as-is — step up one.
                match routing_ladder.next_up(current_tier) {
                    Some((next_tier, next_model)) => {
                        current_tier = next_tier;
                        current_model = Arc::from(next_model);
                        context_bumped_to = Some(current_model.clone());
                    }
                    None => {
                        current_tier = ModelTier::Strong;
                        current_model = parent_model.clone();
                    }
                }
            }
            if current_tier == ModelTier::Strong {
                // No rung below the parent could hold the context: the turn stays
                // on the parent (no savings), same outcome as the old binary
                // fit-check, surfaced under the same stable reason token.
                self.log_event(
                    "routing_skipped_context",
                    Some(self.turn_id),
                    Some(format!(
                        "no rung below the parent {parent_model_str} can fit the current context; \
                         staying on parent (no compaction)"
                    )),
                    json!({ "parent_model": parent_model_str }),
                );
                let _ = self
                    .tx
                    .send(AgentEvent::TurnRouted {
                        turn_id: self.turn_id,
                        from: parent_model_str.clone(),
                        to: parent_model_str.clone(),
                        reason: "reroute_skipped_context".to_string(),
                        effort: None,
                    })
                    .await;
                on_cheap_turn = false;
            } else if let Some(bumped) = context_bumped_to {
                self.log_event(
                    "routing_context_bumped",
                    Some(self.turn_id),
                    Some(format!(
                        "context did not fit the judged rung; bumped up to {bumped}"
                    )),
                    json!({ "model": bumped.to_string() }),
                );
            }
        }
        if on_cheap_turn {
            broker.metrics.routed_to_cheap = true;
            // Cache isolation FIRST: rather than switch the main loop to the cheap
            // model (which cold-rewrites the parent's prompt cache on any later
            // escalation), run the scoped cheap work in a subagent on its own
            // cache namespace while the main loop stays pinned to the parent.
            // Engaged per `[routing].cache_isolation` (default Auto: only when the
            // prefix is large enough to pay for it). On success the subagent's
            // summary is the answer and the turn finishes here, emitting only the
            // `⇄ isolated` note — no redundant `↓ rerouted` note. Otherwise we
            // fall through to announcing the reroute and running the in-loop cheap
            // turn.
            let caching_supported = capabilities_for(self.provider.name(), &parent_model_str)
                .is_some_and(|caps| caps.prompt_caching);
            let prefix_tokens = estimate_context(&conversation).estimated_tokens;
            if turn_router::should_isolate(&self.config.routing, prefix_tokens, caching_supported)
                && self
                    .run_isolated_cheap_turn(
                        &task_title,
                        &current_model,
                        &parent_model_str,
                        &mut conversation,
                        &mut broker,
                        &mut total_cost,
                        user_transcript.clone(),
                        context_compaction.clone(),
                    )
                    .await
                    .is_some()
            {
                return Ok(());
            }
            if let Some(reason_label) = decision.reason_label() {
                self.telemetry
                    .spawn(TelemetryEvent::routing_routed(&reason_label));
                let effort = request_reasoning_effort_for_tier(
                    &self.config,
                    self.provider.name(),
                    &current_model,
                    current_tier,
                    if current_tier == judge_assigned_tier {
                        judge_effort
                    } else {
                        None
                    },
                );
                let _ = self
                    .tx
                    .send(AgentEvent::TurnRouted {
                        turn_id: self.turn_id,
                        from: parent_model_str.clone(),
                        to: current_model.to_string(),
                        reason: reason_label,
                        effort,
                    })
                    .await;
            }
        }
        let mut escalation_state = turn_router::EscalationState::default();
        let mut cheap_provider_error_retry_used = false;
        let mut context_overflow_retry_used = false;
        let mut routing_diversity_results_seen = 0u64;
        let mut routing_diversity_paths = BTreeSet::new();
        for round in 0..MAX_TOOL_ROUNDS {
            if self.cancel.is_cancelled() {
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
                return Ok(());
            }
            // Two-stage cost-cap check: the post-hoc `session_cap_reached`
            // catches a session that crossed the cap on a prior round's
            // recorded provider cost, while `projected_session_cap_overrun`
            // is the *pre-flight* gate that refuses to dispatch the next
            // round when the upcoming spend would push the running total
            // past the cap. Without the second stage the cap can only fire
            // after the over-cap tokens have already been billed (see
            // bd ticket squeezy-xt2o / wave2-16 finding 2: anthropic run
            // landed at 124% of cap before the post-hoc check tripped).
            let cap_status = broker.session_cap_reached().or_else(|| {
                // Include fixed request overhead (instructions + tool schemas)
                // so the pre-flight cost estimate matches what will actually be
                // billed. `estimate_context` only walks conversation items; the
                // overhead from the most-recent assembled request closes the gap.
                //
                // Bootstrap note: `last_request_overhead_tokens` starts at 0
                // and is only written after the first request body is assembled
                // (see the `self.last_request_overhead_tokens.store(...)` call
                // below). On the very first round of a fresh turn, overhead = 0,
                // so the cap projection still under-counts instructions and tool
                // schemas by one round. Every subsequent round uses the prior
                // round's measured overhead and is accurate. A single-round
                // overshoot on the first dispatch is acceptable; fully closing
                // the gap would require assembling a skeleton request before the
                // gate check, which is a larger refactor.
                let overhead = self.last_request_overhead_tokens.load(Ordering::Relaxed);
                let projected_input_tokens = estimate_context(&conversation)
                    .estimated_tokens
                    .saturating_add(overhead);
                let projected_output_tokens = CostBroker::projected_output_tokens(
                    self.config.max_output_tokens,
                    squeezy_llm::model_info_for(self.provider.name(), &current_model)
                        .and_then(|info| info.limits.map(|limits| limits.max_output_tokens)),
                );
                broker.projected_session_cap_overrun(
                    self.provider.name(),
                    &current_model,
                    projected_input_tokens,
                    projected_output_tokens,
                )
            });
            if let Some(status) = cap_status {
                self.stamp_routing_savings(&mut broker.metrics);
                self.publish_terminal_task_state(
                    TaskStateStatus::Failed,
                    Some(format_cap_reached_reason(status)),
                    &task_title,
                )
                .await;
                self.persist_turn_accounting(
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                    false,
                )
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Failed {
                        turn_id: self.turn_id,
                        error: SqueezyError::Agent(format_cap_reached_reason(status)),
                        session_cost: Some(broker.session_cost_snapshot()),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }
            // Adaptive pressure governor (gate variant, B6): below the hard
            // cap but at the pressure threshold, refuse to *start* another
            // round and surface a clear cost-pressure status instead of
            // silently degrading per-turn budgets. Only engages when a cap is
            // configured; the one-shot latch fires it at most once per turn.
            if let Some(status) = broker.pressure_gate() {
                self.stamp_routing_savings(&mut broker.metrics);
                self.publish_terminal_task_state(
                    TaskStateStatus::Failed,
                    Some(format_pressure_gate_reason(status)),
                    &task_title,
                )
                .await;
                self.persist_turn_accounting(
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                    false,
                )
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Failed {
                        turn_id: self.turn_id,
                        error: SqueezyError::Agent(format_pressure_gate_reason(status)),
                        session_cost: Some(broker.session_cost_snapshot()),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }
            // Pre-flight round-input gate (idea G5). Default-off: when
            // `max_round_input_tokens` is unset this whole block is a single
            // `Option` check that returns `None`, so behaviour is unchanged.
            // When set, estimate the assembled request's input tokens with the
            // same `estimate_context` the cap check and compaction use; if the
            // round is over the ceiling, take the cheaper action *first* —
            // force a mid-turn compaction — and only gate the dispatch if the
            // round is *still* over. This converts the existing reactive
            // overflow handling into a proactive one for the round we're about
            // to pay for.
            // Note: the round-input gate counts conversation items only
            // (not fixed overhead). The overhead (instructions + tool schemas)
            // is constant per round and cannot be reduced by compaction, so
            // including it would gate legitimate rounds when overhead is large.
            // The session-cap projection includes overhead for accurate cost
            // estimation (see cap_status check above).
            if let Some(initial_gate) = round_input_gate_status(
                self.config.max_round_input_tokens,
                estimate_context(&conversation).estimated_tokens,
                self.provider.name(),
                &current_model,
                CostBroker::projected_output_tokens(
                    self.config.max_output_tokens,
                    squeezy_llm::model_info_for(self.provider.name(), &current_model)
                        .and_then(|info| info.limits.map(|limits| limits.max_output_tokens)),
                ),
            ) {
                self.dispatch_pre_compact(initial_gate.estimated_input_tokens);
                // Force compaction regardless of the standard compaction
                // thresholds: the gate's own ceiling is the trigger here, so
                // `force = true` makes the extractive (and strategy-aware)
                // pipeline run even when the conversation is below the normal
                // `min_items` / `estimated_tokens` budgets.
                let gate_report = compact_conversation_with_strategy(
                    &mut conversation,
                    &mut context_compaction,
                    &active_attachments,
                    self.store.as_deref(),
                    &self.provider,
                    self.session_log.as_ref(),
                    &self.redactor,
                    &self.config,
                    ContextCompactionTrigger::Auto,
                    true,
                    0,
                )
                .await;
                if let Some(report) = gate_report {
                    self.dispatch_post_compact(
                        report.record.before.estimated_tokens,
                        report.record.after.estimated_tokens,
                    );
                    self.log_event(
                        "context_compacted",
                        Some(self.turn_id),
                        Some(format!(
                            "round-input gate compacted gen={} {}->{} estimated tokens",
                            report.record.generation,
                            report.record.before.estimated_tokens,
                            report.record.after.estimated_tokens,
                        )),
                        json!({
                            "record": report.record,
                            "summary": report.summary,
                            "replacement_id": report.record.replacement_id,
                            "conversation": report.post_compact,
                            "phase": "round_input_gate",
                        }),
                    );
                    let _ = self
                        .tx
                        .send(AgentEvent::ContextCompacted {
                            turn_id: self.turn_id,
                            report,
                        })
                        .await;
                    // Compaction rewrote `conversation`, so the server-side
                    // response-id reuse path must be invalidated and the full
                    // (now-smaller) conversation resent rather than only the
                    // latest user delta — mirrors the mid-turn compaction
                    // handling after a tool round.
                    previous_response_id = None;
                    next_input = conversation.clone();
                }
                // Re-estimate after compaction. If the round is still over the
                // ceiling, gate the dispatch with a clear status instead of
                // paying for the oversized round.
                if let Some(status) = round_input_gate_status(
                    self.config.max_round_input_tokens,
                    estimate_context(&conversation).estimated_tokens,
                    self.provider.name(),
                    &current_model,
                    CostBroker::projected_output_tokens(
                        self.config.max_output_tokens,
                        squeezy_llm::model_info_for(self.provider.name(), &current_model)
                            .and_then(|info| info.limits.map(|limits| limits.max_output_tokens)),
                    ),
                ) {
                    let reason = format_round_input_gate_reason(status);
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.publish_terminal_task_state(
                        TaskStateStatus::Failed,
                        Some(reason.clone()),
                        &task_title,
                    )
                    .await;
                    self.persist_turn_accounting(
                        &total_cost,
                        &broker.metrics,
                        &broker.calibration,
                        false,
                    )
                    .await;
                    let _ = self
                        .tx
                        .send(AgentEvent::Failed {
                            turn_id: self.turn_id,
                            error: SqueezyError::Agent(reason),
                            session_cost: Some(broker.session_cost_snapshot()),
                        })
                        .await;
                    self.finish_turn(&broker.metrics).await;
                    return Ok(());
                }
            }
            let active_mode = load_session_mode(&self.session_mode);
            let loaded_tool_schemas = self.loaded_tool_schemas.lock().await.clone();
            let plan_edit_allowed = plan_mode::plan_edit_allowed_in_workspace(
                active_mode,
                &self.config.workspace_root,
                self.session_id().as_deref(),
            );
            let mode_slot = active_mode as usize;
            if instructions_cache[mode_slot].is_none() {
                instructions_cache[mode_slot] = Some(instructions_with_tool_index(
                    &request_instructions,
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                    plan_edit_allowed,
                ));
            }
            let cached_instructions = instructions_cache[mode_slot]
                .as_ref()
                .expect("instructions cache populated above")
                .clone();
            // Mid-turn escalation: if the routed turn has tripped any signal we
            // tracked over the previous round, step UP one rung of the ladder
            // (weak → medium → strong) from this round onward and let the sticky
            // window suppress routing on the next user prompt. The detector is
            // re-armed with a fresh per-rung budget so a still-cheap rung can
            // escalate again; escalation never steps back DOWN within a turn.
            if on_cheap_turn
                && let Some(reason) = escalation_state.maybe_trigger(
                    broker.metrics.tool_calls,
                    broker.metrics.tool_errors,
                    broker.metrics.budget_denials,
                    "",
                    on_cheap_turn,
                    &self.config.routing,
                    self.config.max_tool_calls_per_turn,
                )
                && let Some((next_tier, next_model)) = routing_ladder.next_up(current_tier)
            {
                let from_model = current_model.to_string();
                let to_model: Arc<str> = Arc::from(next_model);
                current_tier = next_tier;
                current_model = to_model.clone();
                on_cheap_turn = current_tier != ModelTier::Strong;
                if current_tier == ModelTier::Strong {
                    broker.metrics.escalated_to_parent = true;
                }
                escalation_state.rearm_for_next_rung(
                    broker.metrics.tool_calls,
                    broker.metrics.tool_errors,
                    broker.metrics.budget_denials,
                );
                self.emit_escalation(
                    from_model,
                    to_model.to_string(),
                    current_tier,
                    reason.as_str(),
                )
                .await;
            }
            // Tell the model which rung it is on — and, after an escalation, tell
            // the strong model not to trust the weaker model's earlier work
            // (see `tier_trust_note`). Appended per round so it tracks the live
            // tier; constant within a rung, so it does not churn the prompt cache.
            let effective_instructions =
                match tier_trust_note(current_tier, broker.metrics.routed_to_cheap) {
                    Some(note) => format!("{cached_instructions}\n\n{note}"),
                    None => cached_instructions,
                };
            let request = LlmRequest {
                model: current_model.clone(),
                instructions: Arc::from(effective_instructions),
                input: Arc::from(next_input.as_slice()),
                max_output_tokens: self.config.max_output_tokens,
                temperature: self.config.temperature,
                top_p: self.config.top_p,
                seed: self.config.seed,
                stop: self.config.stop.clone(),
                frequency_penalty: self.config.frequency_penalty,
                presence_penalty: self.config.presence_penalty,
                response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
                reasoning_effort: request_reasoning_effort_for_tier(
                    &self.config,
                    self.provider.name(),
                    &current_model,
                    current_tier,
                    if current_tier == judge_assigned_tier {
                        judge_effort
                    } else {
                        None
                    },
                ),
                previous_response_id: previous_response_id.clone(),
                cache_key: None,
                cache: self.session_prompt_cache_key().into(),
                tools: Arc::from(request_tool_specs(
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                    &loaded_tool_schemas,
                    plan_edit_allowed,
                )),
                store: self.config.store_responses,
                tool_choice: effective_tool_choice(self.config.tool_choice.as_deref(), round),
                output_schema: None,
                // G3: forward the operator's `parallel_tool_calls` choice
                // so the model can batch independent tool calls into one
                // turn, re-sending the growing prefix on fewer rounds.
                // `None` (the default) leaves the provider's default —
                // parallel on OpenAI Responses / Chat-Completions — in
                // place, so behavior is unchanged unless opted in.
                parallel_tool_calls: self.config.parallel_tool_calls,
                beta_headers: request_beta_headers(&self.config, self.provider.name()),
                ..LlmRequest::default()
            };
            let request_model = Arc::clone(&request.model);
            let mut effective_model = Arc::clone(&request_model);
            let request_input_bytes = llm_request_input_bytes(&request);
            // Carry the fixed request overhead (system instructions + tool
            // schemas) into the next turn's post-turn compaction gate so it
            // does not under-count the real input size (finding #2). Uses the
            // shared `estimated_tokens` helper so the conversion matches
            // `estimate_context` exactly.
            self.last_request_overhead_tokens.store(
                estimated_tokens(llm_request_overhead_bytes(&request)),
                Ordering::Relaxed,
            );
            let observed_ceiling = {
                let state = self.conversation_state.lock().await;
                state
                    .observed_context_ceilings
                    .get(&(self.provider.name().to_string(), request_model.to_string()))
                    .copied()
            };
            let mut limit_input = ContextLimitInput::new(self.provider.name(), &request_model);
            limit_input.user_override = self.context_window_override_for_model(&request_model);
            limit_input.provider_live_window = self
                .provider_live_context_window_for_model(&request_model)
                .await;
            limit_input.observed_ceiling = observed_ceiling;
            limit_input.models_dev = squeezy_llm::cached_models_dev_view();
            limit_input.effective_percent_override = self
                .config
                .context_compaction
                .effective_context_window_percent;
            limit_input.baseline_reserve_override =
                self.config.context_compaction.baseline_reserve_tokens;
            let request_context =
                estimate_request_context_full(&limit_input, &request, Some(&broker.calibration));
            self.record_replay_request(&request);
            let mut stream = self
                .provider
                .stream_response(request.clone(), self.cancel.clone());
            let mut tool_calls = Vec::new();
            let mut completed = false;
            let mut response_id = None;
            let mut completed_cost = CostSnapshot::default();
            // Per-round terminal-state markers surfaced from the
            // provider stream's `LlmEvent::Completed`. Forwarded on the
            // terminal `AgentEvent::Completed` so eval / TUI consumers
            // can branch on the actual finish path. `stop_reason` is the
            // normalized provider stop kind (added by main); the
            // `reasoning_only_stop` flag is the orthogonal "model spent
            // the round on reasoning and stopped with nothing visible"
            // signal that drives the Phase 4 reasoning-only retry.
            let mut stop_reason: Option<StopReason> = None;
            let mut reasoning_only_stop = false;
            let mut round_text_started = false;
            // Running byte counters for the in-flight round, used to
            // estimate token cost on cancel before the provider has had
            // a chance to emit a `Completed` event with usage. Both
            // counters cover redactor-flushed text plus reasoning
            // deltas; together with `request_input_bytes` they feed
            // `partial_cancel_cost` so a mid-stream cancel attributes
            // the work the provider already did instead of reporting
            // zero.
            let mut round_output_bytes: u64 = 0;

            let mut provider_stream_error = None;
            let mut context_overflow_seen = false;
            loop {
                let Some(event) = (match next_llm_stream_event(
                    &mut stream,
                    &self.cancel,
                    self.config.stream_idle_timeout,
                )
                .await
                {
                    Ok(event) => event,
                    Err(error) => {
                        provider_stream_error = Some(error);
                        break;
                    }
                }) else {
                    break;
                };
                if self.cancel.is_cancelled() {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    broker.metrics.redactions += assistant_stream.total_redactions();
                    let partial = std::mem::take(&mut assistant_message);
                    self.preserve_partial_assistant_on_cancel(
                        partial,
                        &mut conversation,
                        user_transcript.clone(),
                        context_compaction.clone(),
                    )
                    .await;
                    self.fold_partial_cancel_cost(
                        &mut total_cost,
                        &mut broker,
                        effective_model.as_ref(),
                        request_input_bytes,
                        round_output_bytes,
                    )
                    .await;
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.finish_cancelled_turn(
                        &task_title,
                        &total_cost,
                        &broker.metrics,
                        &broker.calibration,
                    )
                    .await;
                    return Ok(());
                }
                match event {
                    LlmEvent::Started => {
                        self.record_replay_model_started();
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
                        if self
                            .tx
                            .send(AgentEvent::ContextUsageUpdate {
                                turn_id: self.turn_id,
                                input_tokens: request_context.input_tokens,
                                context_window_tokens: request_context.context_window_tokens,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::TextDelta(delta) => {
                        // Bill the raw provider delta against output
                        // bytes immediately, before the redactor's tail
                        // buffer can hide the work behind its
                        // `STREAM_TAIL_BYTES` window. A mid-stream cancel
                        // arriving before the redactor releases its
                        // first chunk would otherwise see
                        // `round_output_bytes = 0` and skip cost
                        // attribution even though the provider already
                        // sent the bytes.
                        round_output_bytes = round_output_bytes.saturating_add(delta.len() as u64);
                        let chunk = assistant_stream.push(&delta);
                        if chunk.text.is_empty() {
                            continue;
                        }
                        // Each tool-call round runs the model again and its text deltas
                        // flow into the same `assistant_message` buffer. Without a break,
                        // the prior round's text (often a short "I'm about to do X."
                        // preamble with no trailing newline) glues onto this round's
                        // first chunk in both the live TUI buffer and the final stored
                        // message. Inject a paragraph break before the first text chunk
                        // of any round after the first.
                        if !round_text_started && round > 0 && !assistant_message.is_empty() {
                            let separator = if assistant_message.ends_with("\n\n") {
                                ""
                            } else if assistant_message.ends_with('\n') {
                                "\n"
                            } else {
                                "\n\n"
                            };
                            if !separator.is_empty() {
                                assistant_message.push_str(separator);
                                if self
                                    .tx
                                    .send(AgentEvent::AssistantDelta {
                                        turn_id: self.turn_id,
                                        delta: separator.to_string(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    return Ok(());
                                }
                            }
                        }
                        round_text_started = true;
                        let delta = chunk.text;
                        self.record_replay_model_text_delta(&delta);
                        assistant_message.push_str(&delta);
                        if self
                            .tx
                            .send(AgentEvent::AssistantDelta {
                                turn_id: self.turn_id,
                                delta: delta.clone(),
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        // Mid-stream escalation: a refusal phrase in
                        // the new assistant text flips the router to
                        // the parent model *immediately* instead of
                        // waiting for the next round's preflight
                        // check. The detector carries a short tail so
                        // a phrase straddling two deltas still
                        // matches without rescanning the full
                        // accumulated assistant buffer. Tool-call
                        // ceiling and error
                        // threshold are also re-evaluated here for
                        // free; they only flip when the round-end
                        // accounting would have caught them anyway,
                        // but the early swap shaves wasted output.
                        if on_cheap_turn
                            && let Some(reason) = escalation_state.maybe_trigger(
                                broker.metrics.tool_calls,
                                broker.metrics.tool_errors,
                                broker.metrics.budget_denials,
                                &delta,
                                on_cheap_turn,
                                &self.config.routing,
                                self.config.max_tool_calls_per_turn,
                            )
                            && let Some((next_tier, next_model)) =
                                routing_ladder.next_up(current_tier)
                        {
                            let from_model = current_model.to_string();
                            let to_model: Arc<str> = Arc::from(next_model);
                            current_tier = next_tier;
                            current_model = to_model.clone();
                            on_cheap_turn = current_tier != ModelTier::Strong;
                            if current_tier == ModelTier::Strong {
                                broker.metrics.escalated_to_parent = true;
                            }
                            escalation_state.rearm_for_next_rung(
                                broker.metrics.tool_calls,
                                broker.metrics.tool_errors,
                                broker.metrics.budget_denials,
                            );
                            self.emit_escalation(
                                from_model,
                                to_model.to_string(),
                                current_tier,
                                reason.as_str(),
                            )
                            .await;
                        }
                    }
                    LlmEvent::Refusal { content } => {
                        // OpenAI Responses streams the safety-refusal text on
                        // a dedicated `response.refusal.delta` channel rather
                        // than as `TextDelta`. Without an explicit arm the
                        // refusal prose is dropped and only the generic
                        // `StopReason::Refusal` failure surfaces, so the user
                        // never sees *why* the model declined. Route the
                        // content through the same redactor stream + assistant
                        // buffer + `AssistantDelta` path as ordinary text so
                        // the verbatim refusal lands in the live view and the
                        // stored transcript. The terminal `StopReason::Refusal`
                        // arm below still fires for the canonical failure.
                        round_output_bytes =
                            round_output_bytes.saturating_add(content.len() as u64);
                        let chunk = assistant_stream.push(&content);
                        if chunk.text.is_empty() {
                            continue;
                        }
                        self.record_replay_model_text_delta(&chunk.text);
                        assistant_message.push_str(&chunk.text);
                        if self
                            .tx
                            .send(AgentEvent::AssistantDelta {
                                turn_id: self.turn_id,
                                delta: chunk.text,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::ReasoningDelta { text, .. } => {
                        round_output_bytes = round_output_bytes.saturating_add(text.len() as u64);
                        if self
                            .tx
                            .send(AgentEvent::ReasoningDelta {
                                turn_id: self.turn_id,
                                delta: text,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::ReasoningDone(payload) => {
                        let snapshot = ReasoningSnapshot::from_payload(payload.clone());
                        // Push the opaque blob into the conversation now so the
                        // model gets it back on every subsequent provider call
                        // in this turn (tool result → next model call → ...),
                        // not just at the end. Each reasoning segment is
                        // committed the moment it closes.
                        conversation.push(redact_input_item(
                            LlmInputItem::Reasoning(payload),
                            &self.redactor,
                        ));
                        if self
                            .tx
                            .send(AgentEvent::ReasoningSegment {
                                turn_id: self.turn_id,
                                snapshot,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::ToolCall(tool_call) => {
                        let call = ToolCall {
                            call_id: tool_call.call_id,
                            name: tool_call.name,
                            arguments: tool_call.arguments,
                        };
                        self.record_replay_model_tool_call(&call);
                        self.log_event(
                            "tool_call",
                            Some(self.turn_id),
                            Some(call.name.clone()),
                            json!({
                                "call_id": call.call_id,
                                "tool": call.name,
                                "arguments": call.arguments,
                            }),
                        );
                        if self
                            .tx
                            .send(AgentEvent::ToolCallQueued {
                                turn_id: self.turn_id,
                                call: redact_tool_call(call.clone(), &self.redactor),
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        tool_calls.push(call);
                    }
                    LlmEvent::Completed {
                        response_id: id,
                        mut cost,
                        stop_reason: completion_stop_reason,
                        reasoning_only_stop: round_reasoning_only_stop,
                    } => {
                        if cost.estimated_usd_micros.is_none() {
                            cost.estimated_usd_micros =
                                estimate_cost(self.provider.name(), &effective_model, &cost);
                        }
                        let warning = broker.record_provider_cost(
                            self.provider.name(),
                            &effective_model,
                            CostOrigin::Main,
                            &cost,
                        );
                        if broker.note_unenforceable_cap_round(&cost) {
                            let _ = self
                                .tx
                                .send(AgentEvent::CostCapUnenforceable {
                                    turn_id: self.turn_id,
                                    provider: self.provider.name().to_string(),
                                    model: effective_model.to_string(),
                                })
                                .await;
                        }
                        if broker.metrics.routed_to_cheap
                            && request_model.as_ref() != parent_model_str.as_str()
                        {
                            merge_cost(&mut broker.metrics.routing_cheap_main_provider, &cost);
                        }
                        broker.calibration.record_sample(
                            self.provider.name(),
                            request_input_bytes,
                            cost.input_tokens.unwrap_or(0),
                        );
                        if let Some(status) = warning {
                            let _ = self
                                .tx
                                .send(AgentEvent::CostWarning {
                                    turn_id: self.turn_id,
                                    status,
                                })
                                .await;
                        }
                        merge_cost(&mut total_cost, &cost);
                        completed_cost = cost;
                        response_id = id;
                        stop_reason = completion_stop_reason;
                        reasoning_only_stop = round_reasoning_only_stop;
                        completed = true;
                        break;
                    }
                    LlmEvent::Cancelled => {
                        if let Some(tail) = self
                            .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                            .await
                        {
                            self.record_replay_model_text_delta(&tail);
                        }
                        broker.metrics.redactions += assistant_stream.total_redactions();
                        let partial = std::mem::take(&mut assistant_message);
                        self.preserve_partial_assistant_on_cancel(
                            partial,
                            &mut conversation,
                            user_transcript.clone(),
                            context_compaction.clone(),
                        )
                        .await;
                        self.fold_partial_cancel_cost(
                            &mut total_cost,
                            &mut broker,
                            effective_model.as_ref(),
                            request_input_bytes,
                            round_output_bytes,
                        )
                        .await;
                        self.stamp_routing_savings(&mut broker.metrics);
                        self.finish_cancelled_turn(
                            &task_title,
                            &total_cost,
                            &broker.metrics,
                            &broker.calibration,
                        )
                        .await;
                        return Ok(());
                    }
                    LlmEvent::ContextOverflow { .. } => {
                        context_overflow_seen = true;
                    }
                    LlmEvent::ServerModel(model) => {
                        effective_model = Arc::from(model);
                    }
                    // `ToolCallDelta` is a progressive-args hint superseded
                    // by the canonical `ToolCall` event the loop already
                    // consumes; ignore it.
                    LlmEvent::ToolCallDelta { .. } => {}
                    // Citation annotations (OpenAI source annotations, xAI
                    // Live Search citations) are forwarded as
                    // `AgentEvent::Citation` when there is a consumer
                    // attached. Currently treated as a no-op here: the
                    // `AgentEvent::Citation` variant is declared and the
                    // infrastructure is ready, but emitting it from inside
                    // the large `TurnRuntime::run` stream loop would create
                    // a sizeof(AgentEvent)-byte (~1 KiB) temporary on the
                    // execution stack, which pushes borderline tests over the
                    // default thread stack limit. The emission will move to a
                    // dedicated transcript-sink path once that exists.
                    LlmEvent::Citation { .. } => {}
                    // `LlmEvent` is `#[non_exhaustive]`; unknown future
                    // variants flow past without disturbing the turn — they
                    // get a dedicated arm once consumers are taught about
                    // them.
                    _ => {}
                }
            }

            if let Some(error) = provider_stream_error {
                if context_overflow_seen {
                    // Most providers signal context overflow as a transport
                    // error here (classified into `ContextOverflow`), NOT as a
                    // clean `StopReason::ContextWindowExceeded`. Record the
                    // observed ceiling BEFORE the compaction below shrinks the
                    // conversation, so the recorded size reflects what actually
                    // overflowed.
                    let observed = estimate_context(&conversation).estimated_tokens;
                    self.record_observed_context_ceiling(
                        &current_model,
                        observed,
                        current_model == parent_model,
                    )
                    .await;
                }
                if context_overflow_seen
                    && !context_overflow_retry_used
                    && round_output_bytes == 0
                    && tool_calls.is_empty()
                    && !round_text_started
                {
                    context_overflow_retry_used = true;
                    if self
                        .try_provider_context_overflow_compaction(
                            &mut conversation,
                            &mut context_compaction,
                            &active_attachments,
                            &mut previous_response_id,
                            &mut next_input,
                        )
                        .await
                    {
                        continue;
                    }
                }
                if on_cheap_turn
                    && !cheap_provider_error_retry_used
                    && round_output_bytes == 0
                    && tool_calls.is_empty()
                    && !round_text_started
                {
                    cheap_provider_error_retry_used = true;
                    let reason = turn_router::EscalationReason::ProviderError;
                    let from_model = current_model.to_string();
                    // A provider error on a routed model (rate limit, "model not
                    // found", transient outage) is best recovered by jumping
                    // straight to the reliable parent rather than stepping one
                    // rung — a sibling rung may hit the same condition, and this
                    // is a one-shot recovery, not a quality cascade.
                    current_model = parent_model.clone();
                    current_tier = ModelTier::Strong;
                    on_cheap_turn = false;
                    broker.metrics.escalated_to_parent = true;
                    self.emit_escalation(
                        from_model,
                        parent_model_str.clone(),
                        ModelTier::Strong,
                        reason.as_str(),
                    )
                    .await;
                    continue;
                }
                // Terminal stream error after retries are exhausted (most
                // realistically a stream idle timeout). Mirror the cancel
                // paths' partial preservation before propagating: flush the
                // redactor tail, push the partial assistant text into the
                // conversation/transcript, and fold the partial spend the
                // provider already billed. Without this the bytes already
                // streamed to the TUI are dropped from persisted state and
                // the session cost under-reports the work. The error is
                // still returned so `run`'s caller surfaces `Failed`.
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }
                broker.metrics.redactions += assistant_stream.total_redactions();
                let partial = std::mem::take(&mut assistant_message);
                self.preserve_partial_assistant_on_cancel(
                    partial,
                    &mut conversation,
                    user_transcript.clone(),
                    context_compaction.clone(),
                )
                .await;
                self.fold_partial_cancel_cost(
                    &mut total_cost,
                    &mut broker,
                    effective_model.as_ref(),
                    request_input_bytes,
                    round_output_bytes,
                )
                .await;
                self.stamp_routing_savings(&mut broker.metrics);
                return Err(error);
            }

            if !completed {
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }
                broker.metrics.redactions += assistant_stream.total_redactions();
                let raw_assistant_text = std::mem::take(&mut assistant_message);
                // Reasoning blobs and segment events have already been pushed
                // by the `LlmEvent::ReasoningDone` arm above; only the
                // assistant text remains.
                //
                // Conversation state keeps the raw text (including any
                // `<proposed_plan>` block) so the model retains its own
                // prior plan when refining next turn. The displayed and
                // persisted transcript drops the block — the structured
                // Plan card is the canonical visualization.
                conversation.push(redact_input_item(
                    LlmInputItem::AssistantText(raw_assistant_text.clone()),
                    &self.redactor,
                ));
                let visible_assistant_text = merge_retried_visible_assistant_text(
                    &mut deferred_retry_visible_assistant,
                    &raw_assistant_text,
                    load_session_mode(&self.session_mode) == SessionMode::Plan,
                );
                let message =
                    TranscriptItem::assistant(self.display_assistant_text(&visible_assistant_text));
                self.stamp_routing_savings(&mut broker.metrics);
                self.publish_terminal_task_state(TaskStateStatus::Completed, None, &task_title)
                    .await;
                self.persist_turn_state(TurnPersistInput {
                    conversation: &conversation,
                    response_id: previous_response_id.clone(),
                    user: user_transcript.clone(),
                    assistant: message.clone(),
                    cost: &total_cost,
                    metrics: &broker.metrics,
                    context_compaction: context_compaction.clone(),
                    token_calibration: broker.calibration.clone(),
                })
                .await;
                let context_estimate = estimate_context(&conversation);
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id: None,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                        context_estimate,
                        stop_reason: stop_reason.clone(),
                        reasoning_only_stop,
                        session_cost: Some(broker.session_cost_snapshot()),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            // Record stop-reason and cache telemetry fields on the turn metrics.
            broker.metrics.stop_reason_token = stop_reason.as_ref().map(|r| match r {
                StopReason::EndTurn => "end_turn".to_string(),
                StopReason::ToolUse => "tool_use".to_string(),
                StopReason::MaxTokens => "max_tokens".to_string(),
                StopReason::ContextWindowExceeded => "context_window_exceeded".to_string(),
                StopReason::StopSequence => "stop_sequence".to_string(),
                StopReason::Refusal => "refusal".to_string(),
                StopReason::PauseTurn => "pause_turn".to_string(),
                StopReason::MalformedFunctionCall => "malformed_function_call".to_string(),
                StopReason::Other(_) | _ => "other".to_string(),
            });
            broker.metrics.reasoning_only_stop = reasoning_only_stop;
            // Use total_cost (accumulated across all rounds) so multi-round
            // turns with tool calls don't undercount these metrics.
            if total_cost.cached_input_tokens.is_some()
                || total_cost.cache_write_input_tokens.is_some()
            {
                broker.metrics.cache_supported = true;
            }
            broker.metrics.cache_write_tokens = total_cost.cache_write_input_tokens;
            broker.metrics.reasoning_output_tokens = total_cost.reasoning_output_tokens;

            // Explicit `stop_reason` branches. Truncation (max-tokens,
            // context-window-exceeded) and refusal previously surfaced
            // either as a provider transport error (Anthropic raised
            // `ProviderStream("max_tokens")` directly) or as a silent
            // empty assistant message; either way the user lost the
            // distinction. Route each through `AgentEvent::Failed` with a
            // descriptive error so the TUI can render a recovery hint
            // and future compaction-retry logic can hook in here without
            // touching every provider. `EndTurn` and `ToolUse` fall
            // through to the existing tool-calls / completion logic.
            if matches!(stop_reason, Some(StopReason::ContextWindowExceeded)) {
                let observed = estimate_context(&conversation).estimated_tokens;
                self.record_observed_context_ceiling(
                    &current_model,
                    observed,
                    current_model == parent_model,
                )
                .await;
            }
            if matches!(stop_reason, Some(StopReason::ContextWindowExceeded))
                && !context_overflow_retry_used
                && round_output_bytes == 0
                && tool_calls.is_empty()
                && !round_text_started
            {
                context_overflow_retry_used = true;
                if self
                    .try_provider_context_overflow_compaction(
                        &mut conversation,
                        &mut context_compaction,
                        &active_attachments,
                        &mut previous_response_id,
                        &mut next_input,
                    )
                    .await
                {
                    continue;
                }
            }
            match &stop_reason {
                Some(StopReason::MaxTokens) => {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        None,
                    );
                    let raw_assistant_text = std::mem::take(&mut assistant_message);
                    self.preserve_visible_assistant_before_terminal_failure(
                        merge_retried_visible_assistant_text(
                            &mut deferred_retry_visible_assistant,
                            &raw_assistant_text,
                            load_session_mode(&self.session_mode) == SessionMode::Plan,
                        ),
                        raw_assistant_text,
                        &mut conversation,
                        user_transcript.clone(),
                        context_compaction.clone(),
                    )
                    .await;
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.publish_terminal_task_state(
                        TaskStateStatus::Failed,
                        Some("response truncated by max_tokens".to_string()),
                        &task_title,
                    )
                    .await;
                    let _ = self
                        .tx
                        .send(AgentEvent::Failed {
                            turn_id: self.turn_id,
                            error: SqueezyError::Agent(
                                "model response stopped after max_tokens before completing; lower reasoning_effort, raise the provider's max_output_tokens, or run /compact and retry".to_string(),
                            ),
                            session_cost: Some(broker.session_cost_snapshot()),
                        })
                        .await;
                    self.finish_turn(&broker.metrics).await;
                    return Ok(());
                }
                Some(StopReason::ContextWindowExceeded) => {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        None,
                    );
                    let raw_assistant_text = std::mem::take(&mut assistant_message);
                    self.preserve_visible_assistant_before_terminal_failure(
                        merge_retried_visible_assistant_text(
                            &mut deferred_retry_visible_assistant,
                            &raw_assistant_text,
                            load_session_mode(&self.session_mode) == SessionMode::Plan,
                        ),
                        raw_assistant_text,
                        &mut conversation,
                        user_transcript.clone(),
                        context_compaction.clone(),
                    )
                    .await;
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.publish_terminal_task_state(
                        TaskStateStatus::Failed,
                        Some("context window exceeded".to_string()),
                        &task_title,
                    )
                    .await;
                    let _ = self
                        .tx
                        .send(AgentEvent::Failed {
                            turn_id: self.turn_id,
                            error: SqueezyError::Agent(
                                "model reported the context window was exceeded; run /compact and retry".to_string(),
                            ),
                            session_cost: Some(broker.session_cost_snapshot()),
                        })
                        .await;
                    self.finish_turn(&broker.metrics).await;
                    return Ok(());
                }
                Some(StopReason::Refusal) => {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        None,
                    );
                    let raw_assistant_text = std::mem::take(&mut assistant_message);
                    self.preserve_visible_assistant_before_terminal_failure(
                        merge_retried_visible_assistant_text(
                            &mut deferred_retry_visible_assistant,
                            &raw_assistant_text,
                            load_session_mode(&self.session_mode) == SessionMode::Plan,
                        ),
                        raw_assistant_text,
                        &mut conversation,
                        user_transcript.clone(),
                        context_compaction.clone(),
                    )
                    .await;
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.publish_terminal_task_state(
                        TaskStateStatus::Failed,
                        Some("model refused the request".to_string()),
                        &task_title,
                    )
                    .await;
                    let _ = self
                        .tx
                        .send(AgentEvent::Failed {
                            turn_id: self.turn_id,
                            error: SqueezyError::Agent(
                                "model refused to produce a response (provider safety filter)"
                                    .to_string(),
                            ),
                            session_cost: Some(broker.session_cost_snapshot()),
                        })
                        .await;
                    self.finish_turn(&broker.metrics).await;
                    return Ok(());
                }
                // Anthropic `pause_turn`: the model voluntarily paused
                // mid-turn (typically a hosted tool still processing) and
                // expects the caller to re-issue with the partial state.
                // When the pause carried local tool calls we fall through
                // to the normal tool-execution path below; otherwise retry
                // the partial conversation a small bounded number of times
                // before surfacing a clear failure.
                Some(StopReason::PauseTurn) if tool_calls.is_empty() => {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        None,
                    );
                    broker.metrics.redactions += assistant_stream.total_redactions();
                    if pause_turn_reissues < MAX_PAUSE_TURN_REISSUES {
                        pause_turn_reissues += 1;
                        let raw_assistant_text = std::mem::take(&mut assistant_message);
                        if !raw_assistant_text.is_empty() {
                            conversation.push(redact_input_item(
                                LlmInputItem::AssistantText(raw_assistant_text.clone()),
                                &self.redactor,
                            ));
                        }
                        previous_response_id = None;
                        next_input = conversation.clone();
                        tracing::debug!(
                            target: "squeezy_agent::pause_turn_reissue",
                            round,
                            pause_turn_reissues,
                            max_pause_turn_reissues = MAX_PAUSE_TURN_REISSUES,
                            partial_assistant_chars = raw_assistant_text.chars().count(),
                            "reissuing paused provider turn with partial conversation"
                        );
                        continue;
                    }
                    self.stamp_routing_savings(&mut broker.metrics);
                    self.publish_terminal_task_state(
                        TaskStateStatus::Failed,
                        Some("model paused the turn".to_string()),
                        &task_title,
                    )
                    .await;
                    let _ = self
                        .tx
                        .send(AgentEvent::Failed {
                            turn_id: self.turn_id,
                            error: SqueezyError::Agent(
                                "model paused the turn (pause_turn) without an actionable continuation after bounded re-issue; retry the turn".to_string(),
                            ),
                            session_cost: Some(broker.session_cost_snapshot()),
                        })
                        .await;
                    self.finish_turn(&broker.metrics).await;
                    return Ok(());
                }
                // `Some(StopReason::PauseTurn)` with tool calls present falls
                // through (via the `_` arm) to the existing tool-execution /
                // re-entry logic below.

                // Gemini `MALFORMED_FUNCTION_CALL`: the model tried to call a
                // tool but emitted arguments the upstream parser rejected, so
                // no usable call survives and the turn would otherwise end
                // with nothing. One bounded corrective retry — tell the model
                // its arguments were unparseable and ask it to re-issue with
                // valid JSON. Any visible text it produced first is preserved.
                // (When valid tool calls DID survive alongside the bad one,
                // fall through to execute them.)
                Some(StopReason::MalformedFunctionCall)
                    if !malformed_retry_used && tool_calls.is_empty() =>
                {
                    if let Some(tail) = self
                        .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                        .await
                    {
                        self.record_replay_model_text_delta(&tail);
                    }
                    let raw_assistant_text = std::mem::take(&mut assistant_message);
                    let preserved_visible_chars = append_deferred_visible_assistant_text(
                        &mut deferred_retry_visible_assistant,
                        &raw_assistant_text,
                        load_session_mode(&self.session_mode) == SessionMode::Plan,
                    );
                    if !raw_assistant_text.trim().is_empty() {
                        conversation.push(redact_input_item(
                            LlmInputItem::AssistantText(raw_assistant_text.clone()),
                            &self.redactor,
                        ));
                    }
                    let retry_metadata = json!({
                        "branch": "malformed_function_call",
                        "round": round,
                        "assistant_text_chars": raw_assistant_text.chars().count(),
                        "preserved_visible_chars": preserved_visible_chars,
                    });
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        Some(retry_metadata.clone()),
                    );
                    self.log_event(
                        "assistant_retry",
                        Some(self.turn_id),
                        Some(
                            "malformed_function_call retry: asked the model to re-issue valid JSON"
                                .to_string(),
                        ),
                        retry_metadata,
                    );
                    broker.metrics.redactions += assistant_stream.total_redactions();
                    let nudge_item = redact_input_item(
                        LlmInputItem::UserText(
                            "Your previous tool call could not be parsed — its arguments were not \
                             valid JSON. Re-issue the tool call now with correctly-formed JSON \
                             arguments."
                                .to_string(),
                        ),
                        &self.redactor,
                    );
                    conversation.push(nudge_item.clone());
                    if self.config.store_responses {
                        previous_response_id = response_id.clone();
                        next_input = vec![nudge_item];
                    } else {
                        previous_response_id = None;
                        next_input = conversation.clone();
                    }
                    malformed_retry_used = true;
                    tracing::debug!(
                        target: "squeezy_agent::malformed_function_call_retry",
                        round,
                        preserved_visible_chars,
                        "retrying after malformed tool-call arguments",
                    );
                    continue;
                }
                _ => {}
            }

            if tool_calls.is_empty() {
                // Flush any tail still buffered by the stream redactor
                // BEFORE the retry check — `assistant_text_has_unresolved_intent`
                // needs the complete assistant text, including the last
                // chunk the redactor was holding for cross-chunk redaction
                // scans. The downstream end-of-turn path also wants the
                // flushed text, so we move the flush up unconditionally
                // and re-use it for both branches.
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }

                // One-shot retry for the "model finished without
                // actionable output" failure modes. Three gating
                // shapes, each with its own nudge text:
                //
                // (1) `reasoning_only_stop` — model burned the round
                //     entirely on `reasoning_content` and finished
                //     with stop, no content, no tool call. Canonical
                //     Qwen3 / DeepSeek-R1 reasoning-mode collapse.
                //     Fires from any round so plan-mode turns that
                //     reasoning-out without emitting `<proposed_plan>`
                //     get a second chance.
                //
                // (2) "Promised tool use but stopped" — model emitted
                //     intent text ("Let me scan the codebase") with
                //     finish_reason=stop and zero tool calls AFTER
                //     successfully using a tool earlier this turn.
                //     The exact shape from the user's PortKey+Qwen
                //     screenshot. Gated on `round > 0` so a chatty
                //     preamble before round 0's tool burst isn't
                //     mistaken for the bug.
                //
                // Both branches push the assistant's text to
                // `conversation`, append a mode-aware synthetic user
                // nudge, and re-enter the round loop once. The retry
                // is one-shot per turn via `replan_retry_used` so the
                // model can't trap us in a forever loop.
                let active_mode = load_session_mode(&self.session_mode);
                let plan_mode = active_mode == SessionMode::Plan;
                let reasoning_only_branch = !replan_retry_used
                    && stop_reason == Some(StopReason::EndTurn)
                    && reasoning_only_stop;
                let promised_action_branch = !replan_retry_used
                    && round > 0
                    && stop_reason == Some(StopReason::EndTurn)
                    && assistant_text_has_unresolved_intent(&assistant_message);
                if reasoning_only_branch || promised_action_branch {
                    let raw_assistant_text = std::mem::take(&mut assistant_message);
                    conversation.push(redact_input_item(
                        LlmInputItem::AssistantText(raw_assistant_text.clone()),
                        &self.redactor,
                    ));
                    let retry_branch = if reasoning_only_branch {
                        "reasoning_only"
                    } else {
                        "promised_action"
                    };
                    let preserved_visible_chars = if promised_action_branch {
                        append_deferred_visible_assistant_text(
                            &mut deferred_retry_visible_assistant,
                            &raw_assistant_text,
                            load_session_mode(&self.session_mode) == SessionMode::Plan,
                        )
                    } else {
                        0
                    };
                    let retry_metadata = json!({
                        "branch": retry_branch,
                        "round": round,
                        "plan_mode": plan_mode,
                        "reasoning_only_stop": reasoning_only_stop,
                        "assistant_text_chars": raw_assistant_text.chars().count(),
                        "preserved_visible_chars": preserved_visible_chars,
                    });
                    self.record_replay_model_completed(
                        response_id.clone(),
                        &completed_cost,
                        stop_reason.as_ref(),
                        reasoning_only_stop,
                        Some(retry_metadata.clone()),
                    );
                    self.log_event(
                    "assistant_retry",
                    Some(self.turn_id),
                    Some(format!(
                        "{retry_branch} retry preserved {preserved_visible_chars} visible chars",
                    )),
                    retry_metadata,
                );
                    let nudge = if reasoning_only_branch {
                        if plan_mode {
                            "You finished thinking but produced no `<proposed_plan>...</proposed_plan>` block. \
                             Write your plan now in the tag. Skip further reasoning."
                        } else {
                            "You finished thinking but produced no visible content or tool call. \
                             Respond directly to the user now."
                        }
                    } else {
                        // G2 (action safety): grant permission to finish,
                        // do not command an action. A model that was
                        // actually done replies `DONE` (recognized as an
                        // ack, so its prior visible text is kept verbatim);
                        // a model that genuinely stalled picks up the work.
                        // This is what lets the same recovery run harmlessly
                        // on a strong model that didn't fail.
                        "If your previous response already fully answers the request, \
                         reply with just `DONE` and nothing else. Otherwise, finish the \
                         work now — call the tool you described, or give the final answer \
                         directly. Do not repeat what you already said."
                    };
                    let nudge_item = redact_input_item(
                        LlmInputItem::UserText(nudge.to_string()),
                        &self.redactor,
                    );
                    conversation.push(nudge_item.clone());
                    // Keep the stored-responses chain anchored on the
                    // round we just observed; the next round's request
                    // sends only the nudge as the delta. When not
                    // using stored responses, replay the full
                    // conversation including the nudge.
                    if self.config.store_responses {
                        previous_response_id = response_id.clone();
                        next_input = vec![nudge_item];
                    } else {
                        previous_response_id = None;
                        next_input = conversation.clone();
                    }
                    replan_retry_used = true;
                    tracing::debug!(
                        target: "squeezy_agent::stop_no_action_retry",
                        round,
                        stop_reason = ?stop_reason,
                        reasoning_only_stop,
                        plan_mode,
                        assistant_text_chars = raw_assistant_text.chars().count(),
                        preserved_visible_chars,
                        branch = retry_branch,
                        "retrying turn with mode-aware nudge",
                    );
                    continue;
                }
                self.record_replay_model_completed(
                    response_id.clone(),
                    &completed_cost,
                    stop_reason.as_ref(),
                    reasoning_only_stop,
                    None,
                );
                broker.metrics.redactions += assistant_stream.total_redactions();
                let raw_assistant_text = std::mem::take(&mut assistant_message);
                conversation.push(redact_input_item(
                    LlmInputItem::AssistantText(raw_assistant_text.clone()),
                    &self.redactor,
                ));
                let visible_assistant_text = merge_retried_visible_assistant_text(
                    &mut deferred_retry_visible_assistant,
                    &raw_assistant_text,
                    load_session_mode(&self.session_mode) == SessionMode::Plan,
                );
                let message =
                    TranscriptItem::assistant(self.display_assistant_text(&visible_assistant_text));
                self.stamp_routing_savings(&mut broker.metrics);
                self.publish_terminal_task_state(TaskStateStatus::Completed, None, &task_title)
                    .await;
                self.persist_turn_state(TurnPersistInput {
                    conversation: &conversation,
                    response_id: response_id.clone(),
                    user: user_transcript.clone(),
                    assistant: message.clone(),
                    cost: &total_cost,
                    metrics: &broker.metrics,
                    context_compaction: context_compaction.clone(),
                    token_calibration: broker.calibration.clone(),
                })
                .await;
                let context_estimate = estimate_context(&conversation);
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                        context_estimate,
                        stop_reason: stop_reason.clone(),
                        reasoning_only_stop,
                        session_cost: Some(broker.session_cost_snapshot()),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            self.record_replay_model_completed(
                response_id.clone(),
                &completed_cost,
                stop_reason.as_ref(),
                reasoning_only_stop,
                None,
            );

            let results = if let Some(replay) = &self.replay {
                replay_tool_calls(
                    replay,
                    tool_calls.clone(),
                    self.turn_id,
                    self.tx.clone(),
                    &mut broker,
                )
                .await?
            } else {
                execute_tool_calls(
                    tool_calls.clone(),
                    ToolExecutionContext {
                        turn_id: self.turn_id,
                        origin: ToolOrigin::Model,
                        provider: self.provider.clone(),
                        tools: &self.tools,
                        jobs: &self.jobs,
                        config: &self.config,
                        telemetry: self.telemetry.clone(),
                        redactor: self.redactor.clone(),
                        tx: self.tx.clone(),
                        cancel: self.cancel.clone(),
                        approval_ids: self.approval_ids.clone(),
                        session_rules: self.session_rules.clone(),
                        ai_reviewer_state: self.ai_reviewer_state.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        conversation_state: Some(self.conversation_state.clone()),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                        subagents: self.subagents.clone(),
                        subagent_catalog: self.subagent_catalog.clone(),
                        store: self.store.clone(),
                        hooks: self.hooks.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.stamp_routing_savings(&mut broker.metrics);
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
                return Ok(());
            }
            last_tool_round_summary = tool_round_failure_summary(&results);
            if let Some(reason) = loop_guard.observe_round(&tool_calls, &results) {
                // P0.2 fail-soft: the loop guard tripped (repeated identical
                // tool failure, or control-only rounds). Rather than returning
                // an error that surfaces as a zero-character answer, finalize
                // with whatever the model has already produced this turn plus
                // the stop reason. Flush the in-flight assistant stream so the
                // current round's preamble is included.
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }
                broker.metrics.redactions += assistant_stream.total_redactions();
                let raw_assistant_text = std::mem::take(&mut assistant_message);
                let visible_assistant_text = merge_retried_visible_assistant_text(
                    &mut deferred_retry_visible_assistant,
                    &raw_assistant_text,
                    load_session_mode(&self.session_mode) == SessionMode::Plan,
                );
                self.finish_soft_completion(
                    reason,
                    visible_assistant_text,
                    raw_assistant_text,
                    &mut conversation,
                    response_id.clone(),
                    user_transcript.clone(),
                    total_cost,
                    &mut broker.metrics,
                    context_compaction.clone(),
                    broker.calibration.clone(),
                    stop_reason.clone(),
                    &task_title,
                )
                .await;
                return Ok(());
            }
            let implicit_instructions_added = self.append_implicit_skill_instructions(
                &results,
                &mut active_skill_names,
                &mut request_instructions,
                &mut broker.metrics,
            );
            if implicit_instructions_added {
                instructions_cache = [None, None];
            }
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            self.record_replay_tool_results(&tool_calls, &results);
            for pending in &results {
                broker.record_model_result(&pending.result);
            }
            seen_tool_outputs.remember_results(&results);
            if on_cheap_turn && routing_diversity_results_seen < ROUTING_DIVERSITY_RESULT_WINDOW {
                let observed = collect_tool_round_paths(
                    &tool_calls,
                    &results,
                    ROUTING_DIVERSITY_RESULT_WINDOW - routing_diversity_results_seen,
                    &mut routing_diversity_paths,
                );
                routing_diversity_results_seen =
                    routing_diversity_results_seen.saturating_add(observed);
                if routing_diversity_paths.len() >= ROUTING_DIVERSITY_DISTINCT_PATHS
                    && let Some((next_tier, next_model)) = routing_ladder.next_up(current_tier)
                {
                    let reason = turn_router::EscalationReason::ToolDiversity;
                    let from_model = current_model.to_string();
                    let to_model: Arc<str> = Arc::from(next_model);
                    current_tier = next_tier;
                    current_model = to_model.clone();
                    on_cheap_turn = current_tier != ModelTier::Strong;
                    if current_tier == ModelTier::Strong {
                        broker.metrics.escalated_to_parent = true;
                    }
                    escalation_state.rearm_for_next_rung(
                        broker.metrics.tool_calls,
                        broker.metrics.tool_errors,
                        broker.metrics.budget_denials,
                    );
                    self.emit_escalation(
                        from_model,
                        to_model.to_string(),
                        current_tier,
                        reason.as_str(),
                    )
                    .await;
                }
            }

            // Capture each tool result's terminal status alongside its
            // model-visible output so the post-commit `PostTool` hook
            // below fires with the same status the agent reported for
            // the corresponding tool round.
            let outputs_with_status: Vec<(LlmInputItem, String, ToolStatus)> = results
                .into_iter()
                .map(|pending| {
                    let output = self.redactor.redact(&pending.result.model_output()).text;
                    let tool_name = pending.result.tool_name.clone();
                    let status = pending.result.status;
                    let item = LlmInputItem::FunctionCallOutput {
                        call_id: pending.result.call_id,
                        output,
                        content_parts: None,
                        is_error: tool_status_is_model_error(status),
                    };
                    (item, tool_name, status)
                })
                .collect();
            let outputs: Vec<LlmInputItem> = outputs_with_status
                .iter()
                .map(|(item, _, _)| item.clone())
                .collect();
            conversation.extend(
                tool_calls
                    .iter()
                    .cloned()
                    .map(|call| llm_function_call_item(call, &self.redactor)),
            );
            conversation.extend(outputs.clone());
            for output in &outputs {
                self.log_event(
                    "tool_result",
                    Some(self.turn_id),
                    tool_output_summary(output),
                    json!({ "output": resume_item_for_json(output.clone()) }),
                );
            }
            // PostTool fires after every output has landed in the
            // conversation buffer; handlers that rebuild transcript-
            // derived state (export, audit) see the post-commit view
            // of the turn with the same status the agent reported.
            if let Some(registry) = self.hooks.as_ref() {
                for (item, tool_name, status) in &outputs_with_status {
                    if let LlmInputItem::FunctionCallOutput { call_id, .. } = item {
                        dispatch_post_tool(registry, self.turn_id, tool_name, call_id, *status);
                    }
                }
            }

            // Expired-context masking by file-mutation lineage (M2). When
            // this round landed a successful in-place edit, the earlier
            // read/grep snapshots of the same file now show pre-edit text
            // and waste input tokens on every later turn. Splice the
            // changed spans out of those stale snapshots in place — scoped
            // to the edit's `search` text so surrounding context survives,
            // gated on `ToolStatus::Success` so errored/denied edits never
            // mutate prior reads. Reuses the micro-compaction placeholder
            // (zero extra model call) and runs unconditionally after edits,
            // independent of the token-pressure micro/full gates below.
            let mut expired_context_masked = false;
            if self.config.context_compaction.micro_compaction_enabled {
                let edits = collect_successful_edits(&tool_calls, &outputs_with_status);
                if !edits.is_empty()
                    && let Some(report) = mask_expired_reads_after_edits(
                        &mut conversation,
                        &edits,
                        self.config.context_compaction.micro_compaction_keep_recent,
                    )
                {
                    expired_context_masked = true;
                    self.log_event(
                        "context_expired_masked",
                        Some(self.turn_id),
                        Some(format!(
                            "expired-context masking stubbed {} stale spans across {} reads, freed {} bytes",
                            report.spans_masked,
                            report.masked_call_ids.len(),
                            report.bytes_saved,
                        )),
                        json!({
                            "masked_call_ids": &report.masked_call_ids,
                            "spans_masked": report.spans_masked,
                            "bytes_saved": report.bytes_saved,
                            "phase": "post_edit",
                        }),
                    );
                }
            }

            // Mid-turn trim: between tool rounds, reclaim older bulky
            // `FunctionCallOutput` bodies in place when usage (provider-reported
            // when available, else the local estimate) crosses the trim
            // threshold, so a long tool-heavy turn does not outgrow the window.
            // Summarize never runs mid-turn — it waits for the post-turn boundary
            // or the forced overflow path. Trimming rewrites *earlier* outputs,
            // so it forces the same response-id invalidation + full resend that
            // expired-context masking does.
            let mid_turn_observed_tokens = total_tokens_from_cost(&completed_cost);
            let micro_report = if self.config.context_compaction.enabled_mid_turn {
                maybe_micro_compact(&mut conversation, &self.config, mid_turn_observed_tokens)
            } else {
                None
            };
            if let Some(report) = micro_report.as_ref() {
                self.log_event(
                    "context_micro_compacted",
                    Some(self.turn_id),
                    Some(format!(
                        "mid-turn trim cleared {} tool outputs, freed {} bytes",
                        report.cleared_call_ids.len(),
                        report.bytes_saved,
                    )),
                    json!({
                        "cleared_call_ids": &report.cleared_call_ids,
                        "bytes_saved": report.bytes_saved,
                        "before_estimated_tokens": report.before_estimated_tokens,
                        "after_estimated_tokens": report.after_estimated_tokens,
                        "phase": "mid_turn",
                    }),
                );
            }
            let mid_turn_compacted = micro_report.is_some() || expired_context_masked;

            if self.config.store_responses {
                previous_response_id = if implicit_instructions_added || mid_turn_compacted {
                    None
                } else {
                    response_id
                };
                next_input = if mid_turn_compacted {
                    conversation.clone()
                } else {
                    outputs
                };
            } else {
                previous_response_id = None;
                next_input = conversation.clone();
            }
        }

        // P0.2 fail-soft: exhausting the tool-round budget used to return an
        // error (zero-character answer). Finalize with the best-effort text
        // gathered across the turn instead, noting the round-budget stop.
        let suffix = last_tool_round_summary
            .map(|summary| format!(" · {summary}"))
            .unwrap_or_default();
        if let Some(tail) = self
            .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
            .await
        {
            self.record_replay_model_text_delta(&tail);
        }
        broker.metrics.redactions += assistant_stream.total_redactions();
        let raw_assistant_text = std::mem::take(&mut assistant_message);
        let visible_assistant_text = merge_retried_visible_assistant_text(
            &mut deferred_retry_visible_assistant,
            &raw_assistant_text,
            load_session_mode(&self.session_mode) == SessionMode::Plan,
        );
        self.finish_soft_completion(
            format!("stopped after {MAX_TOOL_ROUNDS} tool rounds{suffix}"),
            visible_assistant_text,
            raw_assistant_text,
            &mut conversation,
            previous_response_id.clone(),
            user_transcript.clone(),
            total_cost,
            &mut broker.metrics,
            context_compaction.clone(),
            broker.calibration.clone(),
            // No per-round stop_reason at budget exhaustion — that variable is
            // loop-scoped and the turn ended by hitting MAX_TOOL_ROUNDS.
            None,
            &task_title,
        )
        .await;
        Ok(())
    }
}
